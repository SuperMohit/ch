//! Project instructions and environment info.
//!
//! Contributes two system blocks at priority 100:
//!
//! 1. an **environment** block: workspace path, platform, today's date, and git state
//!    (whether it's a repository, the branch and a short `git status` summary);
//! 2. a **project instructions** block with every `CH.md` / `AGENTS.md` from the git root
//!    down to the workspace, preceded by the user's `~/.ch/CH.md`. Each file is labelled
//!    with its path, and the total size is capped.
//!
//! Both are computed on the first `system_blocks` call of a session and then reused, so
//! the system prompt stays byte-identical across calls (good for prompt caching).
//!
//! Config (`[plugins.project-instructions]`):
//!
//! ```toml
//! files = ["CH.md", "AGENTS.md"]   # names looked for in each directory
//! user_file = "~/.ch/CH.md"        # user-level instructions ("" to turn off)
//! max_chars = 40000                # cap for all instruction files together
//! environment = true               # include the environment block
//! git_timeout_ms = 2000            # per git command
//! ```

pub mod env;
pub mod files;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use ch_kernel::{ChHome, Cx, Plugin, PluginManifest, PromptContributor, RegisterCx};
use serde::Deserialize;

/// Plugin id, also the config section name.
pub const ID: &str = "project-instructions";

/// `[plugins.project-instructions]`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct InstructionsConfig {
    /// File names looked for in each directory, in this order.
    pub files: Vec<String>,
    /// User-level instructions file. `~/` is the home directory; an empty string turns
    /// it off. Defaults to `CH.md` in the ch home (`~/.ch`).
    pub user_file: Option<String>,
    /// Maximum characters for all instruction files together.
    pub max_chars: usize,
    /// Whether to include the environment block.
    pub environment: bool,
    /// Timeout for each git command, in milliseconds.
    pub git_timeout_ms: u64,
}

impl Default for InstructionsConfig {
    fn default() -> Self {
        Self {
            files: vec!["CH.md".into(), "AGENTS.md".into()],
            user_file: None,
            max_chars: 40_000,
            environment: true,
            git_timeout_ms: 2_000,
        }
    }
}

impl InstructionsConfig {
    /// The user-level file to load, if any; `home` is the ch home.
    pub fn user_path(&self, home: Option<&Path>) -> Option<PathBuf> {
        match self.user_file.as_deref() {
            Some("") => None,
            Some(p) => match p.strip_prefix("~/") {
                Some(rest) => dirs::home_dir().map(|h| h.join(rest)),
                None => Some(PathBuf::from(p)),
            },
            None => home.map(|h| h.join("CH.md")),
        }
    }
}

/// Registers the project-instructions prompt contributor.
pub struct ProjectInstructionsPlugin;

/// Returns the plugin as a trait object.
pub fn plugin() -> Arc<dyn Plugin> {
    Arc::new(ProjectInstructionsPlugin)
}

impl Plugin for ProjectInstructionsPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest::new(
            ID,
            "CH.md / AGENTS.md project instructions and an environment block",
        )
        .provides(&["prompt"])
        .project_settings(&["max_chars", "environment", "git_timeout_ms"])
    }

    fn register(&self, cx: &mut RegisterCx<'_>) -> anyhow::Result<()> {
        let config: InstructionsConfig = cx.config()?;
        let home = cx.service::<ChHome>();
        let user = config.user_path(home.as_ref().map(|h| h.0.as_path()));
        cx.add_prompt(ProjectInstructions::new(config, user));
        Ok(())
    }
}

/// The prompt contributor. Caches its blocks per session.
pub struct ProjectInstructions {
    config: InstructionsConfig,
    /// The user-level instructions file.
    user: Option<PathBuf>,
    /// `(session id, workspace, blocks)` from the first call.
    cache: Mutex<Option<(String, PathBuf, Vec<String>)>>,
}

impl ProjectInstructions {
    pub fn new(config: InstructionsConfig, user: Option<PathBuf>) -> Self {
        Self {
            config,
            user,
            cache: Mutex::new(None),
        }
    }

    /// Computes the blocks for `workspace` (blocking: reads files and runs git).
    pub fn compute(
        config: &InstructionsConfig,
        user: Option<&Path>,
        workspace: &Path,
    ) -> Vec<String> {
        let workspace = workspace
            .canonicalize()
            .unwrap_or_else(|_| workspace.to_path_buf());
        let mut blocks = Vec::new();
        if config.environment {
            let root = files::git_root(&workspace);
            let timeout = Duration::from_millis(config.git_timeout_ms);
            let git = root.as_ref().map(|_| env::git_info(&workspace, timeout));
            blocks.push(env::render(&workspace, root.as_deref(), git.as_ref()));
        }
        let found = files::load(&workspace, user, &config.files);
        if let Some(block) = files::render(&found, config.max_chars) {
            blocks.push(block);
        }
        blocks
    }
}

#[async_trait]
impl PromptContributor for ProjectInstructions {
    fn id(&self) -> &str {
        ID
    }

    fn priority(&self) -> i32 {
        100
    }

    async fn system_blocks(&self, cx: &Cx) -> Vec<String> {
        let workspace = cx.workspace().to_path_buf();
        if let Ok(guard) = self.cache.lock()
            && let Some((sid, ws, blocks)) = guard.as_ref()
            && **sid == *cx.session_id
            && *ws == workspace
        {
            return blocks.clone();
        }

        let config = self.config.clone();
        let user = self.user.clone();
        let ws = workspace.clone();
        let blocks =
            tokio::task::spawn_blocking(move || Self::compute(&config, user.as_deref(), &ws))
                .await
                .unwrap_or_else(|e| {
                    tracing::warn!("project instructions failed: {e}");
                    Vec::new()
                });
        if let Ok(mut guard) = self.cache.lock() {
            *guard = Some((cx.session_id.to_string(), workspace, blocks.clone()));
        }
        blocks
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn instructions(config: InstructionsConfig) -> ProjectInstructions {
        let user = config.user_path(None);
        ProjectInstructions::new(config, user)
    }

    #[test]
    fn user_file_defaults_to_the_ch_home() {
        let c = InstructionsConfig::default();
        assert_eq!(
            c.user_path(Some(Path::new("/h/.ch"))),
            Some(PathBuf::from("/h/.ch/CH.md"))
        );
        assert_eq!(c.user_path(None), None);
    }

    fn config(user_file: Option<&Path>) -> InstructionsConfig {
        InstructionsConfig {
            user_file: Some(
                user_file
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default(),
            ),
            ..Default::default()
        }
    }

    fn cx(workspace: &Path, session: &str) -> Cx {
        let mut cx = Cx::detached(workspace);
        cx.session_id = Arc::from(session);
        cx
    }

    fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        assert!(ok, "git {args:?} failed");
    }

    /// Positions of each needle in `hay`, asserting they appear in order.
    fn assert_in_order(hay: &str, needles: &[&str]) {
        let mut last = 0;
        for n in needles {
            let pos = hay[last..]
                .find(n)
                .unwrap_or_else(|| panic!("`{n}` missing or out of order in:\n{hay}"));
            last += pos + n.len();
        }
    }

    #[tokio::test]
    async fn nested_files_outer_first_and_stop_at_git_root() {
        let tmp = tempfile::tempdir().unwrap();
        let top = tmp.path().canonicalize().unwrap();
        // Above the git root: must not be loaded.
        write(&top.join("CH.md"), "ABOVE-ROOT");
        let repo = top.join("repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        write(&repo.join("CH.md"), "ROOT-CH");
        write(&repo.join("AGENTS.md"), "ROOT-AGENTS");
        write(&repo.join("a/AGENTS.md"), "MIDDLE");
        let ws = repo.join("a/b");
        write(&ws.join("CH.md"), "INNER");
        let user = top.join("home/.ch/CH.md");
        write(&user, "USER-LEVEL");

        let p = instructions(config(Some(&user)));
        let blocks = p.system_blocks(&cx(&ws, "s1")).await;
        assert_eq!(blocks.len(), 2, "{blocks:#?}");
        assert!(blocks[0].starts_with("# Environment"));
        assert!(blocks[0].contains("- Git repository: yes (root:"));
        let instr = &blocks[1];
        assert!(instr.starts_with("# Project instructions"));
        assert!(!instr.contains("ABOVE-ROOT"));
        assert_in_order(
            instr,
            &[
                "USER-LEVEL",
                "ROOT-CH",
                "ROOT-AGENTS",
                "MIDDLE",
                "INNER",
                "</instructions>",
            ],
        );
        assert!(instr.contains(&format!(
            "<instructions path=\"{}\">\nINNER",
            ws.join("CH.md").display()
        )));
    }

    #[tokio::test]
    async fn no_git_only_reads_the_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let top = tmp.path().canonicalize().unwrap();
        write(&top.join("CH.md"), "PARENT");
        let ws = top.join("ws");
        write(&ws.join("AGENTS.md"), "WS-AGENTS");

        let p = instructions(config(None));
        let blocks = p.system_blocks(&cx(&ws, "s1")).await;
        let all = blocks.join("\n");
        assert!(all.contains("- Git repository: no"), "{all}");
        assert!(!all.contains("Current branch"));
        assert!(all.contains("WS-AGENTS"));
        assert!(!all.contains("PARENT"));
        assert!(all.contains(&format!("- Working directory: {}", ws.display())));
        assert!(all.contains(std::env::consts::OS));
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        assert!(all.contains(&today));
    }

    #[tokio::test]
    async fn no_files_means_no_instructions_block() {
        let tmp = tempfile::tempdir().unwrap();
        let p = instructions(InstructionsConfig {
            environment: false,
            ..config(None)
        });
        assert!(p.system_blocks(&cx(tmp.path(), "s")).await.is_empty());
    }

    #[tokio::test]
    async fn real_git_repo_shows_branch_and_status() {
        if !git_available() {
            eprintln!("git not available; skipping");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().canonicalize().unwrap();
        git(&ws, &["init", "-q"]);
        git(&ws, &["symbolic-ref", "HEAD", "refs/heads/trunk"]);
        for i in 0..12 {
            write(&ws.join(format!("f{i:02}.txt")), "x");
        }
        write(&ws.join("CH.md"), "Use tabs.");

        let p = instructions(config(None));
        let blocks = p.system_blocks(&cx(&ws, "s1")).await;
        let env = &blocks[0];
        assert!(env.contains("- Git repository: yes\n"), "{env}");
        assert!(env.contains("- Current branch: trunk"), "{env}");
        assert!(env.contains("13 changed entries"), "{env}");
        assert!(env.contains("?? CH.md"), "{env}");
        assert!(env.contains("... and 3 more"), "{env}");
        assert!(blocks[1].contains("Use tabs."));
    }

    #[tokio::test]
    async fn cached_per_session() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().canonicalize().unwrap();
        write(&ws.join("CH.md"), "VERSION-1");
        let p = instructions(config(None));
        let first = p.system_blocks(&cx(&ws, "s1")).await;
        write(&ws.join("CH.md"), "VERSION-2");
        let again = p.system_blocks(&cx(&ws, "s1")).await;
        assert_eq!(first, again);
        assert!(again.join("").contains("VERSION-1"));
        let other = p.system_blocks(&cx(&ws, "s2")).await;
        assert!(other.join("").contains("VERSION-2"));
    }

    #[tokio::test]
    async fn total_size_is_capped_and_inner_files_win() {
        let tmp = tempfile::tempdir().unwrap();
        let top = tmp.path().canonicalize().unwrap();
        fs::create_dir_all(top.join(".git")).unwrap();
        write(&top.join("CH.md"), &"o".repeat(500));
        let ws = top.join("inner");
        write(&ws.join("CH.md"), &"i".repeat(300));
        let p = instructions(InstructionsConfig {
            max_chars: 400,
            environment: false,
            ..config(None)
        });
        let blocks = p.system_blocks(&cx(&ws, "s")).await;
        let b = &blocks[0];
        assert!(b.contains(&"i".repeat(300)));
        assert!(b.contains(&"o".repeat(100)));
        assert!(!b.contains(&"o".repeat(101)));
        assert!(b.contains("[truncated: showing the first 100 of 500 characters."));

        let p = instructions(InstructionsConfig {
            max_chars: 200,
            environment: false,
            ..config(None)
        });
        let b = p.system_blocks(&cx(&ws, "s")).await.join("");
        assert!(b.contains("[omitted: this file (500 characters)"));
        assert!(b.contains("[truncated: showing the first 200 of 300 characters."));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlinked_files_are_loaded_once() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().canonicalize().unwrap();
        write(&ws.join("AGENTS.md"), "SHARED");
        std::os::unix::fs::symlink(ws.join("AGENTS.md"), ws.join("CH.md")).unwrap();
        let p = instructions(InstructionsConfig {
            environment: false,
            ..config(None)
        });
        let b = p.system_blocks(&cx(&ws, "s")).await.join("");
        assert_eq!(b.matches("SHARED").count(), 1, "{b}");
    }

    #[test]
    fn registers_through_the_kernel() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg =
            ch_kernel::Config::parse("[plugins.project-instructions]\nmax_chars = 10\n").unwrap();
        let reg = ch_kernel::RegistryBuilder::build(vec![plugin()], &cfg, tmp.path()).unwrap();
        assert_eq!(reg.prompts.len(), 1);
        assert_eq!(reg.prompts[0].1.priority(), 100);
    }
}
