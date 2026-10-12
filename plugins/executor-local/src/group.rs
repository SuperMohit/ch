//! Process-group signalling.

use std::time::Duration;

use nix::errno::Errno;
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use tokio::process::Child;
use tokio::time::Instant;

/// Whether any process is still in the group (zombies count until reaped).
fn group_alive(pgid: Pid) -> bool {
    !matches!(killpg(pgid, None), Err(Errno::ESRCH))
}

/// Sends `signal` to the group, ignoring "no such process".
pub(crate) fn signal_group(pgid: Pid, signal: Signal) {
    if let Err(e) = killpg(pgid, signal)
        && e != Errno::ESRCH
    {
        tracing::warn!("executor-local: killpg({pgid}, {signal}) failed: {e}");
    }
}

/// Stops the whole group: SIGTERM, then SIGKILL for anything still alive after `grace`.
/// Reaps the direct child.
pub(crate) async fn terminate_group(pgid: Pid, child: &mut Child, grace: Duration) {
    signal_group(pgid, Signal::SIGTERM);
    let deadline = Instant::now() + grace;
    let mut reaped = false;
    loop {
        if !reaped {
            reaped = matches!(child.try_wait(), Ok(Some(_)) | Err(_));
        }
        if reaped && !group_alive(pgid) {
            return;
        }
        if Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    signal_group(pgid, Signal::SIGKILL);
    if !reaped {
        let _ = child.wait().await;
    }
}

/// Kills the group with SIGKILL when dropped while armed, so dropping the `run` future
/// (for example when the caller is aborted) never leaks processes.
pub(crate) struct GroupGuard {
    pgid: Pid,
    armed: bool,
}

impl GroupGuard {
    pub(crate) fn new(pgid: Pid) -> Self {
        Self { pgid, armed: true }
    }

    /// The command finished normally; leave intentional background processes alone.
    pub(crate) fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        if self.armed {
            signal_group(self.pgid, Signal::SIGKILL);
        }
    }
}
