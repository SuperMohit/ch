//! Base system prompt.
//!
//! Contributes two system blocks at priority 0:
//!
//! 1. the base prompt (`prompts/system.md`, or a file from config), and
//! 2. a "tool rules" block generated from the registered tools (see [`rules`]), so the
//!    prompt only ever talks about tools that exist.
//!
//! Config (`[plugins.prompt-base]`):
//!
//! ```toml
//! system_prompt_file = "prompts/my-system.md"  # replaces system.md (relative to the workspace, `~/` allowed)
//! append = "Always answer in British English."  # extra text added as a last block
//! ```

pub mod rules;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context as _;
use async_trait::async_trait;
use ch_kernel::{Cx, Plugin, PluginManifest, PromptContributor, RegisterCx};
use serde::Deserialize;

/// The built-in base system prompt.
pub const SYSTEM_PROMPT: &str = include_str!("../../../prompts/system.md");

/// Plugin id, also the config section name.
pub const ID: &str = "prompt-base";

/// `[plugins.prompt-base]`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct PromptBaseConfig {
    /// A file whose contents replace the built-in `system.md`. Relative paths are
    /// resolved against the workspace; a leading `~/` means the home directory.
    pub system_prompt_file: Option<PathBuf>,
    /// Extra text added as the last block of this plugin.
    pub append: Option<String>,
}

/// Registers the base prompt contributor.
pub struct PromptBasePlugin;

/// Returns the plugin as a trait object.
pub fn plugin() -> Arc<dyn Plugin> {
    Arc::new(PromptBasePlugin)
}

impl Plugin for PromptBasePlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest::new(
            ID,
            "Base system prompt and a tool-rules block generated from the active tools",
        )
        .provides(&["prompt"])
        .project_settings(&["append"])
    }

    fn register(&self, cx: &mut RegisterCx<'_>) -> anyhow::Result<()> {
        let config: PromptBaseConfig = cx.config()?;
        let base = match &config.system_prompt_file {
            Some(file) => {
                let path = resolve(file, cx.workspace());
                std::fs::read_to_string(&path).with_context(|| {
                    format!(
                        "plugins.{ID}.system_prompt_file: cannot read {}. Fix the path or \
                         remove the setting to use the built-in prompt",
                        path.display()
                    )
                })?
            }
            None => SYSTEM_PROMPT.to_string(),
        };
        let append = config
            .append
            .map(|a| a.trim().to_string())
            .filter(|a| !a.is_empty());
        cx.add_prompt(BasePrompt { base, append });
        Ok(())
    }
}

/// Resolves a configured path: `~/` is the home directory, relative paths are relative
/// to the workspace.
fn resolve(path: &Path, workspace: &Path) -> PathBuf {
    if let Ok(rest) = path.strip_prefix("~")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        workspace.join(path)
    }
}

/// The prompt contributor.
pub struct BasePrompt {
    base: String,
    append: Option<String>,
}

impl BasePrompt {
    /// A contributor with the built-in prompt and nothing appended.
    pub fn builtin() -> Self {
        Self {
            base: SYSTEM_PROMPT.to_string(),
            append: None,
        }
    }

    /// The blocks for a given tool set. Deterministic, so the prompt stays cacheable.
    pub fn blocks(&self, tools: &[ch_kernel::ToolSpec]) -> Vec<String> {
        let mut blocks = vec![self.base.trim().to_string()];
        if let Some(r) = rules::tool_rules(tools) {
            blocks.push(r);
        }
        if let Some(a) = &self.append {
            blocks.push(a.clone());
        }
        blocks
    }
}

#[async_trait]
impl PromptContributor for BasePrompt {
    fn id(&self) -> &str {
        ID
    }

    fn priority(&self) -> i32 {
        0
    }

    async fn system_blocks(&self, cx: &Cx) -> Vec<String> {
        self.blocks(&cx.tools)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ch_kernel::{Config, RegistryBuilder, ToolSpec};
    use serde_json::json;

    /// Every tool name the built-in plugins register.
    const KNOWN: &[&str] = &[
        "read_file",
        "edit_file",
        "write_file",
        "list_files",
        "glob_files",
        "grep_text",
        "bash",
        "web_fetch",
        "update_plan",
    ];

    const FULL: &[&str] = &[
        "read_file",
        "edit_file",
        "write_file",
        "list_files",
        "glob_files",
        "grep_text",
        "bash",
        "web_fetch",
        "update_plan",
    ];

    fn spec(name: &str) -> ToolSpec {
        let read_only = matches!(
            name,
            "read_file" | "list_files" | "glob_files" | "grep_text" | "web_fetch"
        );
        ToolSpec {
            name: name.into(),
            description: format!("When: test tool {name}"),
            input_schema: json!({"type": "object"}),
            read_only,
        }
    }

    fn specs(names: &[&str]) -> Vec<ToolSpec> {
        names.iter().map(|n| spec(n)).collect()
    }

    async fn blocks_via_registry(cfg: &str, workspace: &Path, tools: &[&str]) -> Vec<String> {
        let config = Config::parse(cfg).unwrap();
        let registry = RegistryBuilder::build(vec![plugin()], &config, workspace).unwrap();
        assert_eq!(registry.prompts.len(), 1);
        let mut cx = Cx::detached(workspace);
        cx.tools = Arc::new(specs(tools));
        registry.prompts[0].1.system_blocks(&cx).await
    }

    fn rules_for(names: &[&str]) -> String {
        rules::tool_rules(&specs(names)).unwrap_or_default()
    }

    #[test]
    fn modes() {
        use rules::Mode;
        assert_eq!(rules::mode(&specs(&[])), Mode::None);
        assert_eq!(rules::mode(&specs(FULL)), Mode::Full);
        assert_eq!(rules::mode(&specs(&["read_file", "edit_file"])), Mode::Full);
        assert_eq!(rules::mode(&specs(&["bash", "web_fetch"])), Mode::BashOnly);
        assert_eq!(rules::mode(&specs(&["bash", "read_file"])), Mode::Mixed);
        assert_eq!(rules::mode(&specs(&["web_fetch"])), Mode::Mixed);
    }

    #[test]
    fn snapshot_full_tools() {
        insta::assert_snapshot!("tools_full", rules_for(FULL));
    }

    #[test]
    fn snapshot_full_tools_without_bash() {
        insta::assert_snapshot!(
            "tools_full_no_bash",
            rules_for(&["read_file", "edit_file", "grep_text"])
        );
    }

    #[test]
    fn snapshot_bash_only() {
        insta::assert_snapshot!("tools_bash_only", rules_for(&["bash"]));
    }

    #[test]
    fn snapshot_bash_only_with_extras() {
        insta::assert_snapshot!(
            "tools_bash_only_extras",
            rules_for(&["bash", "web_fetch", "update_plan"])
        );
    }

    #[test]
    fn snapshot_mixed() {
        insta::assert_snapshot!(
            "tools_mixed",
            rules_for(&["bash", "read_file", "grep_text"])
        );
    }

    #[test]
    fn snapshot_no_tools() {
        insta::assert_snapshot!("tools_none", rules_for(&[]));
    }

    #[test]
    fn only_web_fetch_gets_a_using_heading() {
        let r = rules_for(&["web_fetch"]);
        assert!(r.starts_with("# Using your tools"), "{r}");
        assert!(r.contains("`web_fetch`"));
    }

    #[test]
    fn unknown_tools_alone_add_nothing_misleading() {
        let mut t = spec("mcp__github__search");
        t.read_only = false;
        assert_eq!(rules::tool_rules(&[t]), None);
    }

    #[tokio::test]
    async fn never_mentions_missing_tools() {
        let sets: &[&[&str]] = &[
            &[],
            FULL,
            &["read_file", "edit_file"],
            &["read_file", "edit_file", "bash"],
            &["bash"],
            &["bash", "web_fetch"],
            &["bash", "update_plan"],
            &["bash", "read_file"],
            &["read_file", "grep_text"],
            &["edit_file"],
            &["web_fetch"],
            &["update_plan"],
        ];
        let dir = tempfile::tempdir().unwrap();
        for set in sets {
            let all = blocks_via_registry("", dir.path(), set).await.join("\n\n");
            for name in KNOWN {
                if !set.contains(name) {
                    assert!(
                        !all.contains(name),
                        "tool set {set:?}: prompt mentions missing tool `{name}`"
                    );
                }
            }
            assert!(!all.contains("{{"), "unrendered placeholder for {set:?}");
            assert!(!all.contains("<!--"), "unrendered comment for {set:?}");
        }
    }

    #[tokio::test]
    async fn assembled_blocks_full() {
        let dir = tempfile::tempdir().unwrap();
        let blocks = blocks_via_registry("", dir.path(), FULL).await;
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0], SYSTEM_PROMPT.trim());
        insta::assert_snapshot!("assembled_full", blocks.join("\n\n----\n\n"));
    }

    #[tokio::test]
    async fn assembled_blocks_bash_only() {
        let dir = tempfile::tempdir().unwrap();
        let blocks = blocks_via_registry("", dir.path(), &["bash"]).await;
        assert_eq!(blocks.len(), 2);
        assert!(blocks[1].starts_with("# Working with only a shell"));
        assert!(blocks[1].contains("Nothing validates your edits"));
    }

    #[tokio::test]
    async fn config_override_and_append() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("custom.md"), "You are a custom agent.\n").unwrap();
        let cfg = r#"
            [plugins.prompt-base]
            system_prompt_file = "custom.md"
            append = "  Answer in haiku.  "
        "#;
        let blocks = blocks_via_registry(cfg, dir.path(), &["bash"]).await;
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0], "You are a custom agent.");
        assert!(blocks[1].starts_with("# Working with only a shell"));
        assert_eq!(blocks[2], "Answer in haiku.");
    }

    #[tokio::test]
    async fn absolute_override_path() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("abs.md");
        std::fs::write(&file, "Absolute prompt").unwrap();
        let cfg = format!(
            "[plugins.prompt-base]\nsystem_prompt_file = {:?}\n",
            file.to_string_lossy()
        );
        let other = tempfile::tempdir().unwrap();
        let blocks = blocks_via_registry(&cfg, other.path(), &[]).await;
        assert_eq!(blocks[0], "Absolute prompt");
    }

    #[test]
    fn missing_override_file_is_a_clear_error() {
        let dir = tempfile::tempdir().unwrap();
        let config =
            Config::parse("[plugins.prompt-base]\nsystem_prompt_file = \"nope.md\"\n").unwrap();
        let err = match RegistryBuilder::build(vec![plugin()], &config, dir.path()) {
            Ok(_) => panic!("expected an error"),
            Err(e) => format!("{e:#}"),
        };
        assert!(err.contains("system_prompt_file"), "{err}");
        assert!(err.contains("nope.md"), "{err}");
    }

    #[test]
    fn system_prompt_is_a_reasonable_length() {
        let lines = SYSTEM_PROMPT.lines().count();
        assert!((60..=120).contains(&lines), "system.md has {lines} lines");
    }
}
