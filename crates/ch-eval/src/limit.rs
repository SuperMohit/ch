//! `ch-eval limit SECS -- COMMAND...`: runs a command with a wall-clock limit. It is
//! the entrypoint of every container `ch-eval` starts, so a container stops by itself
//! even when the runner that would kill it is gone. The static binary is mounted into
//! each container, so this works in any image.
//!
//! The exit code is the command's, or 128 + the signal that killed it; 124 when the
//! limit was reached (as `timeout(1)`), and 127 when the command couldn't be started.
//! SIGINT and SIGTERM are passed on to the command.

use std::process::Stdio;
use std::time::Duration;

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use tokio::signal::unix::{SignalKind, signal};

/// Exit code when the limit was reached.
pub const TIMED_OUT: i32 = 124;
/// Exit code when the command couldn't be started.
pub const NOT_STARTED: i32 = 127;

/// Runs `command` for at most `limit`, and returns the exit code to exit with.
pub async fn run(limit: Duration, command: &[String]) -> i32 {
    let Some((program, args)) = command.split_first() else {
        eprintln!("ch-eval limit: no command given");
        return NOT_STARTED;
    };
    let mut child = match tokio::process::Command::new(program)
        .args(args)
        .stdin(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("ch-eval limit: can't run {program}: {e}");
            return NOT_STARTED;
        }
    };
    let pid = child
        .id()
        .and_then(|id| i32::try_from(id).ok())
        .map(Pid::from_raw);
    let (Ok(mut int), Ok(mut term)) = (
        signal(SignalKind::interrupt()),
        signal(SignalKind::terminate()),
    ) else {
        return NOT_STARTED;
    };
    let deadline = tokio::time::sleep(limit);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            status = child.wait() => {
                return match status {
                    Ok(s) => exit_code(s),
                    Err(_) => NOT_STARTED,
                };
            }
            _ = &mut deadline => {
                eprintln!("ch-eval limit: stopped after {}s", limit.as_secs());
                let _ = child.start_kill();
                let _ = child.wait().await;
                return TIMED_OUT;
            }
            _ = int.recv() => forward(pid, Signal::SIGINT),
            _ = term.recv() => forward(pid, Signal::SIGTERM),
        }
    }
}

fn forward(pid: Option<Pid>, sig: Signal) {
    if let Some(pid) = pid {
        let _ = kill(pid, sig);
    }
}

fn exit_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .or_else(|| status.signal().map(|s| 128 + s))
        .unwrap_or(NOT_STARTED)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Vec<String> {
        ["sh", "-c", script].map(String::from).to_vec()
    }

    #[tokio::test]
    async fn passes_the_exit_code_through() {
        assert_eq!(run(Duration::from_secs(30), &sh("exit 0")).await, 0);
        assert_eq!(run(Duration::from_secs(30), &sh("exit 125")).await, 125);
        assert_eq!(run(Duration::from_secs(30), &sh("kill -9 $$")).await, 137);
    }

    #[tokio::test]
    async fn stops_the_command_at_the_limit() {
        let started = std::time::Instant::now();
        let code = run(Duration::from_millis(300), &sh("exec sleep 30")).await;
        assert_eq!(code, TIMED_OUT);
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[tokio::test]
    async fn a_missing_command_is_127() {
        assert_eq!(run(Duration::from_secs(5), &[]).await, NOT_STARTED);
        let missing = vec!["/nonexistent/ch".to_string()];
        assert_eq!(run(Duration::from_secs(5), &missing).await, NOT_STARTED);
    }
}
