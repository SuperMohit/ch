//! Headless print frontend (`ch -p "task"`).
//!
//! Registers the frontend `print` and the approvers `auto-deny` and `auto-allow`.
//!
//! # How the frontend gets its task
//!
//! The kernel starts a frontend with [`Frontend::run`]`(session)` only, so the prompt and
//! output settings travel separately as a [`PrintOptions`] service in the session's
//! registry. The host provides it with `RegistryBuilder::build_with_services` (what the
//! `ch` binary does); a plugin could also provide one. If it is missing, `run` fails
//! with an error that says so.
//!
//! # Output formats
//!
//! - `text`: assistant text streamed to stdout as it arrives, ending with a newline. With
//!   `verbose`, tool calls are printed to stderr as compact lines.
//! - `json`: nothing until the end, then one [`PrintResult`] object.
//! - `stream-json`: every [`ch_kernel::AgentEvent`] as one JSON line, then the result.
//!
//! The process exit code comes from the turn's termination reason, see [`exit_code`].

mod approvers;
mod render;
mod run;

use std::str::FromStr;
use std::sync::Arc;

use async_trait::async_trait;
use ch_kernel::{Frontend, Plugin, PluginManifest, RegisterCx, Session};
use serde::{Deserialize, Serialize};

pub use approvers::{AutoAllow, AutoDeny};
pub use run::{PrintResult, exit_code, run_with};

/// Plugin id.
pub const PLUGIN_ID: &str = "frontend-print";

/// How the print frontend writes its output.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OutputFormat {
    /// Assistant text streamed to stdout.
    #[default]
    Text,
    /// One JSON object at the end.
    Json,
    /// Every agent event as a JSON line, then the result object.
    StreamJson,
}

impl OutputFormat {
    /// The name used on the command line.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Json => "json",
            Self::StreamJson => "stream-json",
        }
    }
}

impl FromStr for OutputFormat {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "text" => Ok(Self::Text),
            "json" => Ok(Self::Json),
            "stream-json" => Ok(Self::StreamJson),
            other => anyhow::bail!(
                "unknown output format `{other}`; use one of: text, json, stream-json"
            ),
        }
    }
}

/// What to run and how to report it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PrintOptions {
    /// The task, sent as the single user turn.
    pub prompt: String,
    pub output_format: OutputFormat,
    /// In `text` mode, also print tool calls and notes to stderr.
    pub verbose: bool,
}

/// The options the frontend will use for `session`.
pub fn resolve_options(session: &Session) -> Option<PrintOptions> {
    session
        .registry()
        .services
        .get::<PrintOptions>()
        .map(|o| (*o).clone())
}

/// The `print` frontend.
pub struct PrintFrontend;

#[async_trait]
impl Frontend for PrintFrontend {
    fn id(&self) -> &str {
        "print"
    }

    async fn run(&self, mut session: Session) -> anyhow::Result<i32> {
        let Some(opts) = resolve_options(&session) else {
            session.end().await;
            anyhow::bail!(
                "the print frontend has no task: provide a PrintOptions service when \
                 building the registry"
            );
        };

        // Ctrl-C cancels the turn (exit code 130); a second Ctrl-C exits immediately.
        let cancel = session.cancel_handle();
        let ctrl_c = tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                cancel.cancel();
                if tokio::signal::ctrl_c().await.is_ok() {
                    std::process::exit(130);
                }
            }
        });

        let mut out = std::io::stdout();
        let mut err = std::io::stderr();
        let result = run_with(session, &opts, &mut out, &mut err).await;
        ctrl_c.abort();
        result.map(|r| r.exit_code)
    }
}

/// The `frontend-print` plugin.
pub struct FrontendPrintPlugin;

impl Plugin for FrontendPrintPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest::new(
            PLUGIN_ID,
            "Headless print mode (`ch -p`): text, json or stream-json output, plus the \
             auto-deny and auto-allow approvers",
        )
        .version(env!("CARGO_PKG_VERSION"))
        .provides(&[
            "frontend:print",
            "approver:auto-deny",
            "approver:auto-allow",
        ])
    }

    fn register(&self, cx: &mut RegisterCx<'_>) -> anyhow::Result<()> {
        cx.add_frontend(PrintFrontend);
        cx.add_approver(AutoDeny);
        cx.add_approver(AutoAllow);
        Ok(())
    }
}

/// The plugin, ready for `RegistryBuilder::build`.
pub fn plugin() -> Arc<dyn Plugin> {
    Arc::new(FrontendPrintPlugin)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_format_round_trips() {
        for f in [
            OutputFormat::Text,
            OutputFormat::Json,
            OutputFormat::StreamJson,
        ] {
            assert_eq!(f.as_str().parse::<OutputFormat>().unwrap(), f);
        }
        assert!("yaml".parse::<OutputFormat>().is_err());
    }
}
