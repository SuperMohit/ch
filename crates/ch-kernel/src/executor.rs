//! Executors run shell commands for tools (local process, Docker, ...).

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

/// Environment variables that hold secrets (API keys, tokens), which executors remove
/// from every command's environment so the model can't print them.
///
/// Plugins add to it while registering, with
/// [`RegisterCx::add_secret_env`](crate::RegisterCx::add_secret_env): a provider names
/// the variable it reads its key from. Executors keep a clone from
/// [`RegisterCx::secret_env`](crate::RegisterCx::secret_env) and read it when they run
/// a command, so it doesn't matter which plugin registers first.
///
/// Clones share one set: a name added through any clone is seen by all of them.
#[derive(Debug, Clone, Default)]
pub struct SecretEnv(Arc<RwLock<BTreeSet<String>>>);

impl SecretEnv {
    /// Adds `name`, exactly as the variable is named (an empty name is ignored).
    pub fn add(&self, name: &str) {
        if !name.is_empty() {
            self.0
                .write()
                .unwrap_or_else(|p| p.into_inner())
                .insert(name.to_string());
        }
    }

    /// The names declared so far, sorted.
    pub fn names(&self) -> Vec<String> {
        // A panic can't leave the set half-updated (each change is one insert).
        let set = self.0.read().unwrap_or_else(|p| p.into_inner());
        set.iter().cloned().collect()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecRequest {
    /// Passed to `/bin/sh -c`.
    pub command: String,
    pub cwd: PathBuf,
    pub timeout: Duration,
    #[serde(default)]
    pub env: Vec<(String, String)>,
    /// Optional stdin contents.
    #[serde(default)]
    pub stdin: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ExecResult {
    /// `None` when killed by a signal, timeout or cancellation.
    pub exit_code: Option<i32>,
    /// The signal that terminated the process, if it was killed by one.
    #[serde(default)]
    pub signal: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    pub cancelled: bool,
    pub duration_ms: u64,
}

impl ExecResult {
    pub fn success(&self) -> bool {
        self.exit_code == Some(0)
    }
}

#[async_trait]
pub trait Executor: Send + Sync {
    fn id(&self) -> &str;
    /// Runs the command. On timeout or cancellation the whole process group is killed
    /// and the partial output is returned. `Err` is only for failures to start.
    async fn run(
        &self,
        request: ExecRequest,
        cancel: CancellationToken,
    ) -> anyhow::Result<ExecResult>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_env_is_shared_by_its_clones_and_ignores_blank_names() {
        let declared = SecretEnv::default();
        let held_by_an_executor = declared.clone();
        declared.add("B_KEY");
        declared.add("A_KEY");
        declared.add("");
        declared.add("B_KEY");
        assert_eq!(held_by_an_executor.names(), ["A_KEY", "B_KEY"]);
    }
}
