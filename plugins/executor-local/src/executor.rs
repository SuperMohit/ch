//! [`LocalExecutor`]: runs commands as local processes.

use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use async_trait::async_trait;
use ch_kernel::{ExecRequest, ExecResult, Executor};
use nix::unistd::Pid;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::LocalExecutorConfig;
use crate::capture::{TailBuffer, read_capped};
use crate::group::{GroupGuard, terminate_group};

/// Environment set for every command so tools behave non-interactively.
const FIXED_ENV: &[(&str, &str)] = &[
    ("CH", "1"),
    ("PAGER", "cat"),
    ("GIT_PAGER", "cat"),
    ("TERM", "dumb"),
];

/// Variables the built-in providers read their API key from by default.
pub const DEFAULT_KEY_VARS: &[&str] = &["ANTHROPIC_API_KEY", "OPENAI_API_KEY"];

/// Runs `/bin/sh -c <command>` in a new process group on this machine.
#[derive(Debug, Clone)]
pub struct LocalExecutor {
    config: LocalExecutorConfig,
    /// Removed from every command's environment, so the model can't read the
    /// provider's API key with `printenv` or `echo $VAR`.
    hidden_env: Vec<String>,
}

impl Default for LocalExecutor {
    fn default() -> Self {
        Self::new(LocalExecutorConfig::default())
    }
}

/// How the main process ended.
enum Ending {
    Exited(Option<std::process::ExitStatus>),
    TimedOut,
    Cancelled,
}

impl LocalExecutor {
    pub fn new(config: LocalExecutorConfig) -> Self {
        Self {
            config,
            hidden_env: DEFAULT_KEY_VARS.iter().map(|v| v.to_string()).collect(),
        }
    }

    /// Also removes `names` from every command's environment.
    pub fn hide_env(mut self, names: impl IntoIterator<Item = String>) -> Self {
        for name in names {
            if !self.hidden_env.contains(&name) {
                self.hidden_env.push(name);
            }
        }
        self
    }

    fn build_command(&self, request: &ExecRequest) -> Command {
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c")
            .arg(&request.command)
            .current_dir(&request.cwd)
            .process_group(0)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(if request.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            });
        for name in &self.hidden_env {
            cmd.env_remove(name);
        }
        for (k, v) in &request.env {
            cmd.env(k, v);
        }
        for (k, v) in FIXED_ENV {
            cmd.env(k, v);
        }
        cmd
    }

    /// Waits for the readers, giving up after the drain timeout (background children
    /// can keep the pipes open forever).
    async fn collect(
        &self,
        stdout: JoinHandle<TailBuffer>,
        stderr: JoinHandle<TailBuffer>,
        stop: CancellationToken,
    ) -> (String, String) {
        let drain = Duration::from_millis(self.config.drain_timeout_ms);
        let both = async { (stdout.await, stderr.await) };
        tokio::pin!(both);
        let (out, err) = match tokio::time::timeout(drain, &mut both).await {
            Ok(pair) => pair,
            Err(_) => {
                stop.cancel();
                both.await
            }
        };
        let text = |r: Result<TailBuffer, tokio::task::JoinError>| match r {
            Ok(buf) => buf.into_string(),
            Err(e) => format!("[output capture failed: {e}]"),
        };
        (text(out), text(err))
    }
}

#[async_trait]
impl Executor for LocalExecutor {
    fn id(&self) -> &str {
        "local"
    }

    async fn run(
        &self,
        request: ExecRequest,
        cancel: CancellationToken,
    ) -> anyhow::Result<ExecResult> {
        let started = Instant::now();
        let mut child = self.build_command(&request).spawn().with_context(|| {
            format!(
                "failed to start /bin/sh in {}",
                request.cwd.as_path().display()
            )
        })?;
        let pid = child
            .id()
            .context("the spawned shell exited before its pid could be read")?;
        let pgid = Pid::from_raw(i32::try_from(pid).context("pid out of range")?);
        let mut guard = GroupGuard::new(pgid);

        let cap = self.config.max_output_bytes;
        let stop = CancellationToken::new();
        let stdout = child.stdout.take().context("stdout was not piped")?;
        let stderr = child.stderr.take().context("stderr was not piped")?;
        let out_task = tokio::spawn(read_capped(stdout, cap, stop.clone()));
        let err_task = tokio::spawn(read_capped(stderr, cap, stop.clone()));

        if let (Some(mut pipe), Some(input)) = (child.stdin.take(), request.stdin.clone()) {
            tokio::spawn(async move {
                // A command that doesn't read its stdin closes the pipe; that's fine.
                let _ = pipe.write_all(input.as_bytes()).await;
                let _ = pipe.shutdown().await;
            });
        }

        let ending = tokio::select! {
            status = child.wait() => Ending::Exited(status.ok()),
            _ = tokio::time::sleep(request.timeout) => Ending::TimedOut,
            _ = cancel.cancelled() => Ending::Cancelled,
        };

        let grace = Duration::from_millis(self.config.kill_grace_ms);
        let (exit_code, signal) = match ending {
            Ending::Exited(status) => {
                guard.disarm();
                (
                    status.and_then(|s| s.code()),
                    status.and_then(|s| std::os::unix::process::ExitStatusExt::signal(&s)),
                )
            }
            Ending::TimedOut | Ending::Cancelled => {
                terminate_group(pgid, &mut child, grace).await;
                guard.disarm();
                (None, None)
            }
        };

        let (stdout, stderr) = self.collect(out_task, err_task, stop).await;
        Ok(ExecResult {
            exit_code,
            signal,
            stdout,
            stderr,
            timed_out: matches!(ending, Ending::TimedOut),
            cancelled: matches!(ending, Ending::Cancelled),
            duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        })
    }
}
