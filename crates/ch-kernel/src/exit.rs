//! Leaving the process without leaking what plugins spawned (ADR 0006 §9).
//!
//! [`std::process::exit`] runs no destructors, so a plugin that spawns processes (an MCP
//! server, an external plugin) registers a cleanup here, and every path that ends the
//! process calls [`exit`] instead, which runs the cleanups first.

use std::sync::Mutex;

type Cleanup = Box<dyn Fn() + Send + Sync>;

/// `Mutex::new` and `Vec::new` are `const`, so this needs no lazy initialisation.
static CLEANUPS: Mutex<Vec<(&str, Cleanup)>> = Mutex::new(Vec::new());

/// Registers `cleanup` under `id`, to run before the process exits through [`exit`]. A
/// second registration under the same id is ignored, so a plugin may register from code
/// that runs more than once. A cleanup must be quick and must not block on async work:
/// send signals, don't wait for children.
pub fn on_exit(id: &'static str, cleanup: impl Fn() + Send + Sync + 'static) {
    let mut list = CLEANUPS.lock().unwrap_or_else(|e| e.into_inner());
    if !list.iter().any(|(existing, _)| *existing == id) {
        list.push((id, Box::new(cleanup)));
    }
}

/// Runs every registered cleanup, in registration order. A cleanup that panics doesn't
/// stop the others.
pub fn run_cleanups() {
    let list = CLEANUPS.lock().unwrap_or_else(|e| e.into_inner());
    for (id, cleanup) in list.iter() {
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(cleanup)).is_err() {
            eprintln!("ch: the exit cleanup `{id}` panicked");
        }
    }
}

/// Runs the cleanups, then ends the process with `code`.
pub fn exit(code: i32) -> ! {
    run_cleanups();
    std::process::exit(code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn cleanups_run_once_per_id_and_survive_a_panic() {
        static RUNS: AtomicUsize = AtomicUsize::new(0);
        on_exit("test-panics", || panic!("boom"));
        on_exit("test-counts", || {
            RUNS.fetch_add(1, Ordering::SeqCst);
        });
        on_exit("test-counts", || {
            RUNS.fetch_add(100, Ordering::SeqCst);
        });
        run_cleanups();
        assert_eq!(RUNS.load(Ordering::SeqCst), 1);
    }
}
