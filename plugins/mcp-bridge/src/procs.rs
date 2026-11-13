//! The process groups servers run in, and their stderr.
//!
//! Every spawned group is recorded in a process-wide registry, and the first one
//! registers a [`ch_kernel::exit`] cleanup that kills them all, so `process::exit` (which
//! runs no destructors) never leaves a server behind.

use std::collections::{HashSet, VecDeque};
use std::sync::{Mutex, OnceLock};

use ch_executor_local::group::signal_group;
use nix::sys::signal::Signal;
use nix::unistd::Pid;
use tokio::io::{AsyncRead, AsyncReadExt as _};

fn groups() -> &'static Mutex<HashSet<i32>> {
    static GROUPS: OnceLock<Mutex<HashSet<i32>>> = OnceLock::new();
    GROUPS.get_or_init(Mutex::default)
}

fn lock() -> std::sync::MutexGuard<'static, HashSet<i32>> {
    groups().lock().unwrap_or_else(|e| e.into_inner())
}

/// Records a spawned group.
pub fn register(pgid: i32) {
    ch_kernel::exit::on_exit("mcp-bridge", kill_all);
    lock().insert(pgid);
}

/// Forgets a group once it is stopped.
pub fn unregister(pgid: i32) {
    lock().remove(&pgid);
}

/// The groups recorded now.
pub fn registered() -> Vec<i32> {
    lock().iter().copied().collect()
}

/// SIGKILLs every recorded group. Runs on exit; never blocks.
pub fn kill_all() {
    let all: Vec<i32> = lock().drain().collect();
    for pgid in all {
        signal_group(Pid::from_raw(pgid), Signal::SIGKILL);
    }
}

/// How much of a server's stderr is kept.
pub const STDERR_CAP: usize = 64 * 1024;

/// The tail of a server's stderr, read continuously so a chatty server never blocks
/// on a full pipe.
#[derive(Default)]
pub struct StderrTail {
    buf: Mutex<VecDeque<u8>>,
}

impl StderrTail {
    fn push(&self, bytes: &[u8]) {
        let mut buf = self.buf.lock().unwrap_or_else(|e| e.into_inner());
        buf.extend(bytes);
        let over = buf.len().saturating_sub(STDERR_CAP);
        buf.drain(..over);
    }

    /// Reads `reader` to its end.
    pub async fn drain(&self, mut reader: impl AsyncRead + Unpin) {
        let mut chunk = vec![0u8; 8192];
        loop {
            match reader.read(&mut chunk).await {
                Ok(0) => break,
                Ok(n) => self.push(&chunk[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    }

    /// The last `lines` lines (at most `max_bytes`), as lossy UTF-8. Not yet redacted
    /// or made printable.
    pub fn tail(&self, lines: usize, max_bytes: usize) -> String {
        let buf = self.buf.lock().unwrap_or_else(|e| e.into_inner());
        let bytes: Vec<u8> = buf.iter().copied().collect();
        drop(buf);
        let text = String::from_utf8_lossy(&bytes);
        let trimmed = text.trim_end();
        let mut start = trimmed.len();
        for _ in 0..lines {
            match trimmed[..start].rfind('\n') {
                Some(i) => start = i,
                None => {
                    start = 0;
                    break;
                }
            }
        }
        let mut tail = trimmed[start..].trim_start_matches('\n');
        if tail.len() > max_bytes {
            let mut cut = tail.len() - max_bytes;
            while !tail.is_char_boundary(cut) {
                cut += 1;
            }
            tail = &tail[cut..];
        }
        tail.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stderr_keeps_the_tail() {
        let t = StderrTail::default();
        t.push(&vec![b'x'; STDERR_CAP]);
        t.push(b"\nline one\nline two\nline three\n");
        assert_eq!(t.tail(2, 1000), "line two\nline three");
        assert_eq!(t.tail(1, 5), "three");
        assert_eq!(t.buf.lock().unwrap().len(), STDERR_CAP);
        assert_eq!(StderrTail::default().tail(3, 100), "");
    }
}
