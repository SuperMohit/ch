//! Interactive REPL frontend.
//!
//! Registers the frontend `repl` and the approver `repl`.
//!
//! - A reedline prompt with file-backed history (`~/.ch/history`, or `$CH_HOME/history`),
//!   multi-line input (Alt-Enter or a trailing `\`) and Tab completion of slash commands.
//! - Slash commands go to `Session::run_command`; other input is a turn.
//! - During a turn, events are rendered as they arrive: streamed text (restyled as
//!   markdown line by line), a spinner while waiting, compact tool lines with a one-line
//!   result, and coloured diffs for edits. A footer shows tokens, cost and time.
//! - Esc or Ctrl-C interrupts the turn; the conversation so far is kept. Ctrl-C twice
//!   at the prompt, or Ctrl-D, exits.
//! - The `repl` approver asks y / n / a(lways) at the prompt; "always" can also save the
//!   rule to `.ch/config.toml` under `plugins.safety.allow`.
//!
//! # Configuration (`[plugins.frontend-repl]`)
//!
//! ```toml
//! [plugins.frontend-repl]
//! history_file = "~/.ch/history"   # default: $CH_HOME/history or ~/.ch/history
//! history_size = 1000
//! ```
//!
//! # Initial prompt
//!
//! `ch "task"` starts the REPL and runs `task` as the first turn. The CLI passes it as a
//! [`ReplOptions`] service when building the registry.

mod approval;
mod format;
mod input;
mod markdown;
mod repl;
mod terminal;
#[cfg(test)]
mod turn_tests;
mod view;

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use ch_kernel::{Frontend, Plugin, PluginManifest, RegisterCx, Session};
use serde::{Deserialize, Serialize};

pub use approval::{
    ApprovalBridge, ApprovalKey, ReplApprover, add_allow_rule, parse_approval_key, save_allow_rule,
};
pub use format::{color_diff, result_summary, strip_ansi, tool_end_lines, tool_start_line};
pub use markdown::{MarkdownLines, render_message};
pub use view::{Action, TurnView};

/// Plugin id.
pub const PLUGIN_ID: &str = "frontend-repl";

/// `[plugins.frontend-repl]`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReplConfig {
    /// History file; `~` is expanded. Defaults to `<ch home>/history`.
    pub history_file: Option<String>,
    /// Entries kept in the history file.
    pub history_size: usize,
}

impl Default for ReplConfig {
    fn default() -> Self {
        Self {
            history_file: None,
            history_size: 1000,
        }
    }
}

impl ReplConfig {
    /// The history file to use, if a home directory can be found.
    pub fn history_path(&self) -> Option<PathBuf> {
        match &self.history_file {
            Some(p) => Some(expand_home(p)),
            None => ch_home().map(|h| h.join("history")),
        }
    }
}

/// `$CH_HOME`, or `~/.ch`.
pub fn ch_home() -> Option<PathBuf> {
    std::env::var_os("CH_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".ch")))
}

fn expand_home(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(p)
}

/// Options from the command line.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ReplOptions {
    /// Run this as the first turn.
    pub initial_prompt: Option<String>,
}

/// Called with the rule after the user saves an "always" rule to `.ch/config.toml`.
/// The CLI provides it as a service so a rule the user saved doesn't count as an
/// untrusted change to the project config.
pub struct RuleSaved(pub Box<dyn Fn(&str) + Send + Sync>);

fn resolve_options(session: &Session) -> ReplOptions {
    session
        .registry()
        .services
        .get::<ReplOptions>()
        .map(|o| (*o).clone())
        .unwrap_or_default()
}

/// The `repl` frontend.
pub struct ReplFrontend {
    bridge: Arc<ApprovalBridge>,
    config: ReplConfig,
}

#[async_trait]
impl Frontend for ReplFrontend {
    fn id(&self) -> &str {
        "repl"
    }

    async fn run(&self, mut session: Session) -> anyhow::Result<i32> {
        let Some(mut approvals) = self.bridge.take_receiver() else {
            session.end().await;
            anyhow::bail!("the REPL frontend is already running");
        };
        let options = resolve_options(&session);
        self.bridge.set_active(true);
        let result = repl::run_repl(session, &mut approvals, &self.config, options).await;
        self.bridge.set_active(false);
        self.bridge.return_receiver(approvals);
        result
    }
}

/// The `frontend-repl` plugin.
pub struct FrontendReplPlugin;

impl Plugin for FrontendReplPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest::new(
            PLUGIN_ID,
            "Interactive REPL: streaming output, tool lines and diffs, approvals, interrupts",
        )
        .version(env!("CARGO_PKG_VERSION"))
        .provides(&["frontend:repl", "approver:repl"])
    }

    fn register(&self, cx: &mut RegisterCx<'_>) -> anyhow::Result<()> {
        let config: ReplConfig = cx.config()?;
        let bridge = Arc::new(ApprovalBridge::new());
        cx.add_frontend(ReplFrontend {
            bridge: bridge.clone(),
            config,
        });
        cx.add_approver(ReplApprover { bridge });
        Ok(())
    }
}

/// The plugin, ready for `RegistryBuilder::build`.
pub fn plugin() -> Arc<dyn Plugin> {
    Arc::new(FrontendReplPlugin)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registers_frontend_and_approver() {
        let cfg = ch_kernel::Config::default();
        let reg =
            ch_kernel::RegistryBuilder::build(vec![plugin()], &cfg, std::path::Path::new("/tmp"))
                .unwrap();
        assert!(reg.frontend(Some("repl")).is_ok());
        assert!(reg.approver(Some("repl")).unwrap().is_some());
    }

    #[test]
    fn config_defaults_and_history_path() {
        let c = ReplConfig::default();
        assert_eq!(c.history_size, 1000);
        let c = ReplConfig {
            history_file: Some("/tmp/h".into()),
            ..Default::default()
        };
        assert_eq!(c.history_path(), Some(PathBuf::from("/tmp/h")));
    }
}
