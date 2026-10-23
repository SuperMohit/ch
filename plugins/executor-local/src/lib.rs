//! Local process executor (plugin `executor-local`, executor id `local`).
//!
//! Runs `/bin/sh -c <command>` in a new process group. Stdout and stderr are captured
//! concurrently and capped (keeping the tail). On timeout or cancellation the whole
//! group gets SIGTERM, then SIGKILL after a short grace, and the partial output is
//! returned. Background processes that keep the pipes open don't hang the call: once
//! the shell exits, reading stops after a short drain timeout.
//!
//! Provider API keys are removed from the commands' environment: `ANTHROPIC_API_KEY`,
//! `OPENAI_API_KEY`, and any variable a `[plugins.<id>] api_key_env` setting names.
//! There is deliberately no setting to pass them through, since a project's
//! `.ch/config.toml` could otherwise expose the user's key to the model.
//!
//! ```toml
//! [plugins.executor-local]
//! max_output_bytes = 2097152   # per stream
//! kill_grace_ms = 1000
//! drain_timeout_ms = 500
//! ```

mod capture;
mod executor;
mod group;

use std::sync::Arc;

use ch_kernel::{Config, Plugin, PluginManifest, RegisterCx};
use serde::Deserialize;

pub use executor::{DEFAULT_KEY_VARS, LocalExecutor};

/// `[plugins.executor-local]` settings.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct LocalExecutorConfig {
    /// Cap per stream (stdout, stderr). Overflow keeps the tail.
    pub max_output_bytes: usize,
    /// Time between SIGTERM and SIGKILL when stopping a process group.
    pub kill_grace_ms: u64,
    /// How long to keep reading the pipes after the shell exits.
    pub drain_timeout_ms: u64,
}

impl Default for LocalExecutorConfig {
    fn default() -> Self {
        Self {
            max_output_bytes: 2 * 1024 * 1024,
            kill_grace_ms: 1000,
            drain_timeout_ms: 500,
        }
    }
}

/// Registers the `local` executor.
pub struct ExecutorLocalPlugin;

impl Plugin for ExecutorLocalPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest::new(
            "executor-local",
            "Runs shell commands as local processes (/bin/sh -c, own process group)",
        )
        .version(env!("CARGO_PKG_VERSION"))
        .provides(&["executor:local"])
    }

    fn register(&self, cx: &mut RegisterCx<'_>) -> anyhow::Result<()> {
        let config: LocalExecutorConfig = cx.config()?;
        let keys = api_key_vars(cx.full_config());
        cx.add_executor(LocalExecutor::new(config).hide_env(keys));
        Ok(())
    }
}

/// The variables named by `api_key_env` in any `[plugins.<id>]` section.
pub fn api_key_vars(config: &Config) -> Vec<String> {
    let Some(plugins) = config.get_value("plugins").and_then(|v| v.as_table()) else {
        return Vec::new();
    };
    plugins
        .values()
        .filter_map(|section| section.get("api_key_env")?.as_str())
        .map(str::to_string)
        .collect()
}

/// The plugin, for the CLI's plugin list.
pub fn plugin() -> Arc<dyn Plugin> {
    Arc::new(ExecutorLocalPlugin)
}
