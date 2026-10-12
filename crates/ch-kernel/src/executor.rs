//! Executors run shell commands for tools (local process, Docker, ...).

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

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
