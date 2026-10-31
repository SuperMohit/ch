//! Post-edit diagnostics (plugin `diagnostics`): after a successful `edit_file` or
//! `write_file`, runs a checker chosen by the file's extension and appends what it
//! reports to the tool result, so the model sees problems while the change is fresh.
//!
//! The command runs through the session's executor at the workspace root with a short
//! timeout. Exit 0 adds nothing. A checker that isn't installed (exit 126 or 127), times
//! out or can't start is skipped quietly: diagnostics never fail an edit.
//!
//! ```toml
//! [plugins.diagnostics]
//! timeout_secs = 10
//! max_output_chars = 2000
//!
//! [plugins.diagnostics.commands]   # by extension; "" turns a default off
//! ts = "npx --no-install tsc --noEmit --pretty false {file}"
//! py = ""
//! ```
//!
//! `{file}` is the shell-quoted path relative to the workspace. `{edition}` is the Rust
//! edition from the nearest `Cargo.toml` above the file (`2021` if none says). The
//! defaults are `ruff check --quiet {file}` for `.py` and
//! `rustfmt --check --edition {edition} {file}` for `.rs`. `commands` runs programs,
//! so a project's `.ch/config.toml` can set it only once you trust the project.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use ch_kernel::reexports::async_trait;
use ch_kernel::util::truncate_middle;
use ch_kernel::{
    Cx, ExecRequest, Hook, HookDecision, HookEvent, HookPoint, Plugin, PluginManifest, RegisterCx,
};
use serde::Deserialize;
use serde_json::Value;

/// Plugin id, also the hook id.
pub const ID: &str = "diagnostics";

/// Checkers used unless config replaces or turns them off.
pub const DEFAULT_COMMANDS: &[(&str, &str)] = &[
    ("py", "ruff check --quiet {file}"),
    ("rs", "rustfmt --check --edition {edition} {file}"),
];

/// The tools whose successful results are checked.
const EDIT_TOOLS: &[&str] = &["edit_file", "write_file"];

/// `[plugins.diagnostics]`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DiagnosticsConfig {
    pub timeout_secs: u64,
    /// Checker output longer than this is cut in the middle.
    pub max_output_chars: usize,
    /// Command by file extension, added to [`DEFAULT_COMMANDS`]; `""` removes one.
    pub commands: BTreeMap<String, String>,
}

impl Default for DiagnosticsConfig {
    fn default() -> Self {
        Self {
            timeout_secs: 10,
            max_output_chars: 2000,
            commands: BTreeMap::new(),
        }
    }
}

/// Registers the hook.
pub struct DiagnosticsPlugin;

/// The plugin, for the CLI's plugin list.
pub fn plugin() -> Arc<dyn Plugin> {
    Arc::new(DiagnosticsPlugin)
}

impl Plugin for DiagnosticsPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest::new(
            ID,
            "Runs a checker on each edited file and shows its findings to the model",
        )
        .version(env!("CARGO_PKG_VERSION"))
        .provides(&["hook:diagnostics"])
        .project_settings(&["timeout_secs", "max_output_chars"])
    }

    fn register(&self, cx: &mut RegisterCx<'_>) -> anyhow::Result<()> {
        let config: DiagnosticsConfig = cx.config()?;
        cx.add_hook(DiagnosticsHook::new(config));
        Ok(())
    }
}

/// The `post_tool` hook.
pub struct DiagnosticsHook {
    /// Extension (lowercase, no dot) → command.
    commands: BTreeMap<String, String>,
    timeout: Duration,
    max_output_chars: usize,
}

impl DiagnosticsHook {
    pub fn new(config: DiagnosticsConfig) -> Self {
        let mut commands: BTreeMap<String, String> = DEFAULT_COMMANDS
            .iter()
            .map(|(ext, cmd)| (ext.to_string(), cmd.to_string()))
            .collect();
        for (ext, cmd) in config.commands {
            let ext = ext.trim_start_matches('.').to_ascii_lowercase();
            if cmd.trim().is_empty() {
                commands.remove(&ext);
            } else {
                commands.insert(ext, cmd);
            }
        }
        Self {
            commands,
            timeout: Duration::from_secs(config.timeout_secs.max(1)),
            max_output_chars: config.max_output_chars,
        }
    }

    /// The command for `rel` (relative to `workspace`), with its placeholders filled.
    fn command_for(&self, workspace: &Path, rel: &str) -> Option<String> {
        let ext = Path::new(rel).extension()?.to_str()?.to_ascii_lowercase();
        let template = self.commands.get(&ext)?;
        let mut command = template.replace("{file}", &shell_words::quote(rel));
        if command.contains("{edition}") {
            command = command.replace("{edition}", &rust_edition(workspace, rel));
        }
        Some(command)
    }

    /// The checker's report for `rel`, if it found something.
    async fn check(&self, rel: &str, cx: &Cx) -> Option<String> {
        let executor = cx.executor.as_ref()?;
        let command = self.command_for(cx.workspace(), rel)?;
        let request = ExecRequest {
            command: command.clone(),
            cwd: cx.workspace().to_path_buf(),
            timeout: self.timeout,
            env: Vec::new(),
            stdin: None,
        };
        let result = match executor.run(request, cx.cancel.clone()).await {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!("diagnostics: `{command}` didn't start: {e:#}");
                return None;
            }
        };
        let code = match result.exit_code {
            Some(0) => return None,
            Some(126 | 127) | None => {
                tracing::debug!(?result, "diagnostics: `{command}` skipped");
                return None;
            }
            Some(code) => code,
        };
        let output = [result.stdout.trim(), result.stderr.trim()]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        let output = truncate_middle(&output, self.max_output_chars);
        Some(format!(
            "Diagnostics: `{command}` exited with {code} after this change:\n{output}\n\
             Fix the problems your change caused before moving on."
        ))
    }
}

#[async_trait]
impl Hook for DiagnosticsHook {
    fn id(&self) -> &str {
        ID
    }

    fn points(&self) -> &[HookPoint] {
        &[HookPoint::PostTool]
    }

    async fn on(&self, event: &HookEvent, cx: &Cx) -> HookDecision {
        let HookEvent::PostTool {
            call,
            output,
            not_run: false,
        } = event
        else {
            return HookDecision::Continue;
        };
        if output.is_error || !EDIT_TOOLS.contains(&call.name.as_str()) {
            return HookDecision::Continue;
        }
        // The tool's own checked path, not the model's input.
        let Some(rel) = output.metadata.get("path").and_then(Value::as_str) else {
            return HookDecision::Continue;
        };
        match self.check(rel, cx).await {
            Some(report) => HookDecision::note(report),
            None => HookDecision::Continue,
        }
    }
}

/// The `edition` of the nearest `Cargo.toml` from `rel`'s directory up to `workspace`.
/// A member's `edition.workspace = true` doesn't match, so the search goes on to the
/// workspace's `Cargo.toml`.
fn rust_edition(workspace: &Path, rel: &str) -> String {
    let mut dir = workspace.join(rel);
    while dir.pop() && dir.starts_with(workspace) {
        let Ok(text) = std::fs::read_to_string(dir.join("Cargo.toml")) else {
            continue;
        };
        let edition = text.lines().find_map(|line| {
            let value = line.trim().strip_prefix("edition")?.trim_start();
            let value = value.strip_prefix('=')?.trim().strip_prefix('"')?;
            let year = value.split('"').next()?;
            (year.len() == 4 && year.bytes().all(|b| b.is_ascii_digit())).then_some(year)
        });
        if let Some(year) = edition {
            return year.to_string();
        }
    }
    "2021".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hook(commands: &[(&str, &str)]) -> DiagnosticsHook {
        DiagnosticsHook::new(DiagnosticsConfig {
            commands: commands
                .iter()
                .map(|(e, c)| (e.to_string(), c.to_string()))
                .collect(),
            ..DiagnosticsConfig::default()
        })
    }

    #[test]
    fn config_adds_replaces_and_removes_commands() {
        let h = hook(&[(".TS", "tsc {file}"), ("py", ""), ("rs", "cargo check")]);
        let ws = Path::new("/nonexistent");
        assert_eq!(h.command_for(ws, "a/b.ts").as_deref(), Some("tsc a/b.ts"));
        assert_eq!(h.command_for(ws, "a/B.TS").as_deref(), Some("tsc a/B.TS"));
        assert_eq!(h.command_for(ws, "x.py"), None);
        assert_eq!(h.command_for(ws, "x.rs").as_deref(), Some("cargo check"));
        assert_eq!(h.command_for(ws, "Makefile"), None);
    }

    #[test]
    fn the_path_is_quoted_for_the_shell() {
        let h = hook(&[("txt", "check {file}")]);
        let cmd = h
            .command_for(Path::new("/ws"), "a b/$(touch x).txt")
            .unwrap();
        assert_eq!(cmd, "check 'a b/$(touch x).txt'");
    }

    #[test]
    fn edition_comes_from_the_nearest_cargo_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        std::fs::create_dir_all(ws.join("crates/a/src")).unwrap();
        std::fs::create_dir_all(ws.join("crates/b/src")).unwrap();
        std::fs::write(
            ws.join("Cargo.toml"),
            "[workspace.package]\nedition = \"2024\"\n",
        )
        .unwrap();
        std::fs::write(
            ws.join("crates/a/Cargo.toml"),
            "[package]\nedition.workspace = true\n",
        )
        .unwrap();
        std::fs::write(
            ws.join("crates/b/Cargo.toml"),
            "[package]\nedition = \"2018\"\n",
        )
        .unwrap();
        assert_eq!(rust_edition(ws, "crates/a/src/lib.rs"), "2024");
        assert_eq!(rust_edition(ws, "crates/b/src/lib.rs"), "2018");

        let bare = tempfile::tempdir().unwrap();
        assert_eq!(rust_edition(bare.path(), "src/main.rs"), "2021");
        let cmd = hook(&[]).command_for(ws, "crates/a/src/lib.rs").unwrap();
        assert_eq!(cmd, "rustfmt --check --edition 2024 crates/a/src/lib.rs");
    }
}
