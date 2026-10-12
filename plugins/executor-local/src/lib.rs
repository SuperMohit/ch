//! Local process executor (plugin `executor-local`, executor id `local`).
//!
//! Runs `/bin/sh -c <command>` in a new process group. Stdout and stderr are captured
//! concurrently and capped (keeping the tail). On timeout or cancellation the whole
//! group gets SIGTERM, then SIGKILL after a short grace, and the partial output is
//! returned. Background processes that keep the pipes open don't hang the call: once
//! the shell exits, reading stops after a short drain timeout.
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

use ch_kernel::{Plugin, PluginManifest, RegisterCx};
use serde::Deserialize;

pub use executor::LocalExecutor;

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
        cx.add_executor(LocalExecutor::new(config));
        Ok(())
    }
}

/// The plugin, for the CLI's plugin list.
pub fn plugin() -> Arc<dyn Plugin> {
    Arc::new(ExecutorLocalPlugin)
}
