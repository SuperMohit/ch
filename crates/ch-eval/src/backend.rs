//! Where a run's steps run: the agent, the diff and the check.
//!
//! The real backend is [`crate::docker::DockerBackend`]. `HostBackend`, behind the
//! `host-backend` feature, runs the steps on the host with no isolation, for tests.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use async_trait::async_trait;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::ledger::StateDir;

/// A step of a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// `ch -p`, the instruction on stdin.
    Agent,
    /// [`crate::workspace::DIFF_SCRIPT`] against the baseline; stdout is the patch.
    Diff,
    /// The task's check.
    Verify,
}

/// What a backend needs to know about one run.
#[derive(Debug, Clone, Copy)]
pub struct Job<'a> {
    /// Unique among this invocation's runs; names the containers.
    pub id: &'a str,
    pub image: &'a str,
    /// The workspace.
    pub work: &'a Path,
    /// `CH_HOME` for the run.
    pub ch_home: &'a Path,
    /// A scratch copy of the baseline's git directory, for the diff.
    pub git_dir: &'a Path,
    /// `ch`'s arguments, after the program name.
    pub ch_args: &'a [String],
    /// Variables for the agent (its proxy token), passed by name on command lines.
    pub env: &'a [(String, String)],
    /// The task's check, a shell command.
    pub verify: &'a str,
}

/// A file a step's output goes to, and how much of it is kept. Past the cap the rest
/// is read and dropped, with a note, so the step never blocks on a full pipe.
#[derive(Debug)]
pub struct Sink {
    pub file: std::fs::File,
    pub cap: u64,
}

/// Where a step's output goes.
#[derive(Debug)]
pub struct Io<'a> {
    pub stdin: Option<&'a str>,
    pub stdout: Sink,
    pub stderr: Sink,
}

/// How a step ended.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Exit {
    /// The step's own exit code; `None` when it was killed at its timeout.
    pub code: Option<i32>,
    pub timed_out: bool,
    pub wall: Duration,
    /// The step's command never started (the container runtime's error).
    pub start_error: Option<String>,
}

/// The error of a step the backend stopped because `ch-eval` is shutting down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stopping;

impl std::fmt::Display for Stopping {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ch-eval is stopping")
    }
}

impl std::error::Error for Stopping {}

/// The credential proxy the backend runs: its URL as `ch` sees it, and its state.
#[derive(Debug, Clone, PartialEq)]
pub struct ProxyRoute {
    pub url: String,
    pub state: StateDir,
}

/// Runs the steps of runs.
#[async_trait]
pub trait Backend: Send + Sync {
    /// The command line a step runs, for `--dry-run` and `run.json`. Variables are
    /// named, never given values.
    fn command(&self, step: Step, job: &Job<'_>) -> Vec<String>;

    /// Runs a step for at most `limit`. An error means the backend itself failed.
    async fn run(
        &self,
        step: Step,
        job: &Job<'_>,
        io: Io<'_>,
        limit: Duration,
    ) -> anyhow::Result<Exit>;

    /// The proxy model calls go through, if any.
    fn proxy(&self) -> Option<&ProxyRoute>;

    /// Identifies the `ch` binary and, in a container, `image`, for the fingerprint.
    fn identity(&self, image: &str) -> String;
}

/// A process to run.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Invocation {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    /// Variables set for the process.
    pub env: Vec<(String, String)>,
    /// Start from an empty environment instead of the runner's.
    pub clear_env: bool,
}

impl Invocation {
    /// The program and its arguments.
    pub fn command_line(&self) -> Vec<String> {
        std::iter::once(self.program.clone())
            .chain(self.args.iter().cloned())
            .collect()
    }
}

/// Runs `inv` with `io`. Past `limit` it is killed, after `on_timeout` (which can stop
/// what the process started, such as a container).
pub async fn execute(
    inv: &Invocation,
    io: Io<'_>,
    limit: Duration,
    on_timeout: impl AsyncFnOnce(),
) -> anyhow::Result<Exit> {
    let mut cmd = Command::new(&inv.program);
    cmd.args(&inv.args)
        .stdin(if io.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if inv.clear_env {
        cmd.env_clear();
    }
    cmd.envs(inv.env.iter().map(|(k, v)| (k, v)));
    if let Some(dir) = &inv.cwd {
        cmd.current_dir(dir);
    }
    let started = Instant::now();
    let mut child = cmd
        .spawn()
        .with_context(|| format!("starting {}", inv.program))?;
    let copies = [
        child
            .stdout
            .take()
            .map(|o| tokio::spawn(copy_capped(o, io.stdout))),
        child
            .stderr
            .take()
            .map(|e| tokio::spawn(copy_capped(e, io.stderr))),
    ];
    if let (Some(text), Some(mut pipe)) = (io.stdin, child.stdin.take()) {
        // A process that exits without reading closes the pipe; that's its business.
        let _ = pipe.write_all(text.as_bytes()).await;
        drop(pipe);
    }
    let waited = tokio::time::timeout(limit, child.wait()).await;
    let (status, timed_out) = match waited {
        Ok(status) => (
            status.with_context(|| format!("waiting for {}", inv.program))?,
            false,
        ),
        Err(_) => {
            on_timeout().await;
            let _ = child.start_kill();
            let status = child
                .wait()
                .await
                .with_context(|| format!("waiting for {}", inv.program))?;
            (status, true)
        }
    };
    // Output still held open by something the process started is cut off.
    let copies = futures::future::join_all(copies.into_iter().flatten());
    let _ = tokio::time::timeout(Duration::from_secs(2), copies).await;
    Ok(Exit {
        code: if timed_out { None } else { status.code() },
        timed_out,
        wall: started.elapsed(),
        start_error: None,
    })
}

/// Copies `from` to `to` up to its cap, then reads and drops the rest.
async fn copy_capped(mut from: impl tokio::io::AsyncRead + Unpin, to: Sink) {
    use std::io::Write as _;
    use tokio::io::AsyncReadExt as _;
    let Sink { mut file, cap } = to;
    let mut kept = 0u64;
    let mut cut = false;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = match from.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        let room = usize::try_from(cap.saturating_sub(kept)).unwrap_or(usize::MAX);
        let take = n.min(room);
        if take > 0 && file.write_all(&buf[..take]).is_err() {
            cut = true;
        }
        kept += take as u64;
        if take < n && !cut {
            cut = true;
            let _ = write!(file, "\n[ch-eval: output past {cap} bytes was dropped]\n");
        }
    }
    let _ = file.flush();
}

/// Runs the steps directly on the host, as the user, with no isolation: the model's
/// commands could touch anything. Only for tests (the `host-backend` feature); the
/// command line has no way to choose it.
#[cfg(feature = "host-backend")]
#[derive(Debug, Clone)]
pub struct HostBackend {
    /// The `ch` binary.
    pub ch: PathBuf,
    /// A proxy the test started, if model calls should go through one.
    pub proxy: Option<ProxyRoute>,
}

#[cfg(feature = "host-backend")]
impl HostBackend {
    fn invocation(&self, step: Step, job: &Job<'_>) -> Invocation {
        // `PATH` from the runner (the check needs its tools), `HOME` in the run's ch
        // home so nothing is written to the user's.
        let mut env = vec![("HOME".to_string(), job.ch_home.display().to_string())];
        if let Some(path) = std::env::var_os("PATH") {
            env.push(("PATH".into(), path.to_string_lossy().into_owned()));
        }
        let (program, args) = match step {
            Step::Agent => {
                env.push(("CH_HOME".into(), job.ch_home.display().to_string()));
                env.extend(job.env.iter().cloned());
                let mut args = job.ch_args.to_vec();
                args.extend(["--cwd".into(), job.work.display().to_string()]);
                (self.ch.display().to_string(), args)
            }
            Step::Diff => {
                env.push(("GD".into(), job.git_dir.display().to_string()));
                env.push(("WT".into(), job.work.display().to_string()));
                env.extend(
                    crate::workspace::DIFF_ENV
                        .iter()
                        .map(|(k, v)| (k.to_string(), v.to_string())),
                );
                let script = crate::workspace::DIFF_SCRIPT.to_string();
                ("sh".into(), vec!["-c".into(), script])
            }
            Step::Verify => ("sh".into(), vec!["-c".into(), job.verify.into()]),
        };
        Invocation {
            program,
            args,
            cwd: Some(job.work.to_path_buf()),
            env,
            clear_env: true,
        }
    }
}

#[cfg(feature = "host-backend")]
#[async_trait]
impl Backend for HostBackend {
    fn command(&self, step: Step, job: &Job<'_>) -> Vec<String> {
        self.invocation(step, job).command_line()
    }

    async fn run(
        &self,
        step: Step,
        job: &Job<'_>,
        io: Io<'_>,
        limit: Duration,
    ) -> anyhow::Result<Exit> {
        execute(&self.invocation(step, job), io, limit, async || {}).await
    }

    fn proxy(&self) -> Option<&ProxyRoute> {
        self.proxy.as_ref()
    }

    fn identity(&self, _image: &str) -> String {
        crate::fingerprint::hash_file_cached(&self.ch).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Invocation {
        Invocation {
            program: "sh".into(),
            args: vec!["-c".into(), script.into()],
            ..Default::default()
        }
    }

    fn io(dir: &Path, stdin: Option<&'static str>) -> Io<'static> {
        let sink = |name: &str| Sink {
            file: std::fs::File::create(dir.join(name)).unwrap(),
            cap: 1024,
        };
        Io {
            stdin,
            stdout: sink("out"),
            stderr: sink("err"),
        }
    }

    #[tokio::test]
    async fn passes_stdin_and_captures_output() {
        let dir = tempfile::tempdir().unwrap();
        let exit = execute(
            &sh("cat; echo oops >&2; exit 3"),
            io(dir.path(), Some("the task")),
            Duration::from_secs(30),
            async || {},
        )
        .await
        .unwrap();
        assert_eq!(exit.code, Some(3));
        assert!(!exit.timed_out);
        let read = |n| std::fs::read_to_string(dir.path().join(n)).unwrap();
        assert_eq!(read("out"), "the task");
        assert_eq!(read("err"), "oops\n");
    }

    #[tokio::test]
    async fn kills_at_the_timeout_after_the_hook() {
        let dir = tempfile::tempdir().unwrap();
        let hook_ran = std::sync::atomic::AtomicBool::new(false);
        let exit = execute(
            &sh("echo started; exec sleep 30"),
            io(dir.path(), None),
            Duration::from_millis(300),
            async || hook_ran.store(true, std::sync::atomic::Ordering::SeqCst),
        )
        .await
        .unwrap();
        assert!(exit.timed_out);
        assert_eq!(exit.code, None);
        assert!(hook_ran.load(std::sync::atomic::Ordering::SeqCst));
        assert!(exit.wall < Duration::from_secs(10));
        let out = std::fs::read_to_string(dir.path().join("out")).unwrap();
        assert_eq!(out, "started\n");
    }

    #[tokio::test]
    async fn environment_can_be_cleared() {
        let dir = tempfile::tempdir().unwrap();
        let mut inv = sh("echo \"[$HOME][$ONLY]\"");
        inv.program = "/bin/sh".into();
        inv.clear_env = true;
        inv.env = vec![("ONLY".into(), "this".into())];
        execute(
            &inv,
            io(dir.path(), None),
            Duration::from_secs(30),
            async || {},
        )
        .await
        .unwrap();
        let out = std::fs::read_to_string(dir.path().join("out")).unwrap();
        assert_eq!(out, "[][this]\n");
    }

    #[tokio::test]
    async fn output_past_the_cap_is_dropped_without_blocking() {
        let dir = tempfile::tempdir().unwrap();
        // 4 MB through a 1 KB cap: the writer must not block on a full pipe.
        let exit = execute(
            &sh("head -c 4000000 /dev/zero; echo done >&2"),
            io(dir.path(), None),
            Duration::from_secs(30),
            async || {},
        )
        .await
        .unwrap();
        assert_eq!(exit.code, Some(0));
        let out = std::fs::read(dir.path().join("out")).unwrap();
        assert!(out.len() < 2048, "{}", out.len());
        assert!(String::from_utf8_lossy(&out).contains("output past 1024 bytes was dropped"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("err")).unwrap(),
            "done\n"
        );
    }
}
