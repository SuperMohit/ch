//! Shell tool (plugin `tool-bash`, tool `bash`).
//!
//! Runs commands through the session's active executor (`cx.executor`). A nonzero exit,
//! a timeout or a cancellation sets `is_error`. The description has two variants, chosen
//! at registration: with `tools-fs` enabled it steers file work to the dedicated tools;
//! without it ("bash-only mode") it teaches shell-based reading and editing and warns
//! that nothing validates the edits.
//!
//! ```toml
//! [plugins.tool-bash]
//! default_timeout_secs = 120
//! max_timeout_secs = 600
//! ```

mod cwd;
mod description;
mod format;
mod tool;

use std::sync::Arc;

use ch_kernel::{Plugin, PluginManifest, RegisterCx};
use serde::Deserialize;

pub use tool::{BashTool, DescriptionVariant};

/// `[plugins.tool-bash]` settings.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct BashConfig {
    /// Timeout when the model doesn't pass `timeout_seconds`.
    pub default_timeout_secs: u64,
    /// Larger requested timeouts are clamped to this, with a note.
    pub max_timeout_secs: u64,
}

impl Default for BashConfig {
    fn default() -> Self {
        Self {
            default_timeout_secs: 120,
            max_timeout_secs: 600,
        }
    }
}

/// Registers the `bash` tool.
pub struct ToolBashPlugin;

impl Plugin for ToolBashPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest::new(
            "tool-bash",
            "The `bash` tool: runs shell commands through the active executor",
        )
        .version(env!("CARGO_PKG_VERSION"))
        .provides(&["tool:bash"])
        .project_settings(&["default_timeout_secs", "max_timeout_secs"])
    }

    fn register(&self, cx: &mut RegisterCx<'_>) -> anyhow::Result<()> {
        let config: BashConfig = cx.config()?;
        let variant = if cx.plugin_enabled("tools-fs") {
            DescriptionVariant::WithFileTools
        } else {
            DescriptionVariant::BashOnly
        };
        cx.add_tool(BashTool::new(config, variant));
        Ok(())
    }
}

/// The plugin, for the CLI's plugin list.
pub fn plugin() -> Arc<dyn Plugin> {
    Arc::new(ToolBashPlugin)
}
