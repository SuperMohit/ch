//! Tests for the local executor against real processes.

use std::path::Path;
use std::time::{Duration, Instant};

use ch_executor_local::{LocalExecutor, LocalExecutorConfig};
use ch_kernel::reexports::CancellationToken;
use ch_kernel::{ExecRequest, ExecResult, Executor};

fn request(command: &str, cwd: &Path, timeout: Duration) -> ExecRequest {
    ExecRequest {
        command: command.to_string(),
        cwd: cwd.to_path_buf(),
        timeout,
        env: Vec::new(),
        stdin: None,
    }
}

async fn run(command: &str) -> ExecResult {
    let dir = tempfile::tempdir().unwrap();
    LocalExecutor::default()
        .run(
            request(command, dir.path(), Duration::from_secs(10)),
            CancellationToken::new(),
        )
        .await
        .unwrap()
}

fn fast_executor() -> LocalExecutor {
    LocalExecutor::new(LocalExecutorConfig {
        kill_grace_ms: 300,
        drain_timeout_ms: 200,
        ..Default::default()
    })
}

/// Whether `pid` is still running (polls briefly, since a killed orphan is reaped
/// by init asynchronously).
fn wait_until_dead(pid: &str) -> bool {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        let alive = std::process::Command::new("kill")
            .args(["-0", pid])
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !alive {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

#[tokio::test]
async fn exit_codes_are_reported() {
    let ok = run("true").await;
    assert_eq!(ok.exit_code, Some(0));
    assert!(ok.success());
    let failed = run("exit 3").await;
    assert_eq!(failed.exit_code, Some(3));
    assert!(!failed.success());
    assert!(!failed.timed_out && !failed.cancelled);
}

#[tokio::test]
async fn stdout_and_stderr_are_separate() {
    let r = run("echo out; echo err >&2").await;
    assert_eq!(r.stdout, "out\n");
    assert_eq!(r.stderr, "err\n");
}

#[tokio::test]
async fn env_includes_request_and_fixed_values() {
    let dir = tempfile::tempdir().unwrap();
    let mut req = request(
        "printf '%s %s %s %s' \"$CH\" \"$PAGER\" \"$TERM\" \"$FOO\"",
        dir.path(),
        Duration::from_secs(5),
    );
    req.env.push(("FOO".into(), "bar".into()));
    let r = LocalExecutor::default()
        .run(req, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(r.stdout, "1 cat dumb bar");
}

#[tokio::test]
async fn stdin_is_null_unless_given() {
    let dir = tempfile::tempdir().unwrap();
    // With no stdin, `cat` sees EOF immediately instead of hanging.
    let r = LocalExecutor::default()
        .run(
            request("cat", dir.path(), Duration::from_secs(5)),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(r.exit_code, Some(0));
    assert_eq!(r.stdout, "");

    let mut req = request("tr a-z A-Z", dir.path(), Duration::from_secs(5));
    req.stdin = Some("hello".into());
    let r = LocalExecutor::default()
        .run(req, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(r.stdout, "HELLO");
}

#[tokio::test]
async fn cwd_is_respected() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("marker.txt"), "x").unwrap();
    let r = LocalExecutor::default()
        .run(
            request("ls", dir.path(), Duration::from_secs(5)),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(r.stdout.contains("marker.txt"), "{}", r.stdout);
}

#[tokio::test]
async fn missing_cwd_is_a_spawn_error() {
    let dir = tempfile::tempdir().unwrap();
    let err = LocalExecutor::default()
        .run(
            request("true", &dir.path().join("nope"), Duration::from_secs(5)),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("failed to start"), "{err}");
}

#[tokio::test]
async fn timeout_kills_the_whole_group_and_keeps_partial_output() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("bg.pid");
    let cmd = format!(
        "echo started; echo oops >&2; sleep 30 & echo $! > {}; sleep 30",
        pidfile.display()
    );
    let start = Instant::now();
    let r = fast_executor()
        .run(
            request(&cmd, dir.path(), Duration::from_millis(500)),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "{:?}",
        start.elapsed()
    );
    assert!(r.timed_out);
    assert!(!r.cancelled);
    assert_eq!(r.exit_code, None);
    assert_eq!(r.stdout, "started\n");
    assert_eq!(r.stderr, "oops\n");
    let pid = std::fs::read_to_string(&pidfile).unwrap();
    assert!(
        wait_until_dead(pid.trim()),
        "background child {pid} survived"
    );
}

#[tokio::test]
async fn sigterm_is_followed_by_sigkill() {
    let dir = tempfile::tempdir().unwrap();
    let start = Instant::now();
    let r = fast_executor()
        .run(
            request(
                "trap '' TERM; sleep 30",
                dir.path(),
                Duration::from_millis(200),
            ),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(r.timed_out);
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "{:?}",
        start.elapsed()
    );
}

#[tokio::test]
async fn cancellation_stops_the_command() {
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        trigger.cancel();
    });
    let start = Instant::now();
    let r = fast_executor()
        .run(
            request(
                "echo partial; sleep 30",
                dir.path(),
                Duration::from_secs(60),
            ),
            cancel,
        )
        .await
        .unwrap();
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "{:?}",
        start.elapsed()
    );
    assert!(r.cancelled);
    assert!(!r.timed_out);
    assert_eq!(r.exit_code, None);
    assert_eq!(r.stdout, "partial\n");
}

#[tokio::test]
async fn huge_output_is_capped_keeping_the_tail() {
    let dir = tempfile::tempdir().unwrap();
    let exec = LocalExecutor::new(LocalExecutorConfig {
        max_output_bytes: 10_000,
        ..Default::default()
    });
    let r = exec
        .run(
            request(
                "i=0; while [ $i -lt 20000 ]; do echo line$i; i=$((i+1)); done; echo END",
                dir.path(),
                Duration::from_secs(30),
            ),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(r.exit_code, Some(0));
    assert!(r.stdout.len() < 10_200, "{}", r.stdout.len());
    assert!(r.stdout.starts_with("[... "), "{}", &r.stdout[..80]);
    assert!(r.stdout.ends_with("line19999\nEND\n"));
}

#[tokio::test]
async fn background_process_holding_the_pipe_does_not_hang() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("bg.pid");
    let cmd = format!("sleep 30 & echo $! > {}; echo done", pidfile.display());
    let start = Instant::now();
    let r = fast_executor()
        .run(
            request(&cmd, dir.path(), Duration::from_secs(20)),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(
        start.elapsed() < Duration::from_secs(3),
        "{:?}",
        start.elapsed()
    );
    assert_eq!(r.exit_code, Some(0));
    assert_eq!(r.stdout, "done\n");
    assert!(!r.timed_out);
    // Clean up the intentional background process.
    let pid = std::fs::read_to_string(&pidfile).unwrap();
    let _ = std::process::Command::new("kill").arg(pid.trim()).status();
}

#[test]
fn plugin_registers_the_local_executor() {
    let cfg = ch_kernel::Config::default();
    let registry = ch_kernel::RegistryBuilder::build(
        vec![ch_executor_local::plugin()],
        &cfg,
        Path::new("/tmp"),
    )
    .unwrap();
    let exec = registry.executor(Some("local")).unwrap().unwrap();
    assert_eq!(exec.id(), "local");
}

#[tokio::test]
async fn dropping_the_run_future_kills_the_group() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("pid");
    let cmd = format!("echo $$ > {}; sleep 30", pidfile.display());
    let req = request(&cmd, dir.path(), Duration::from_secs(60));
    let task = tokio::spawn(async move {
        LocalExecutor::default()
            .run(req, CancellationToken::new())
            .await
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while !pidfile.exists() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    task.abort();
    let _ = task.await;
    let pid = std::fs::read_to_string(&pidfile).unwrap();
    assert!(
        wait_until_dead(pid.trim()),
        "shell {pid} survived the abort"
    );
}
