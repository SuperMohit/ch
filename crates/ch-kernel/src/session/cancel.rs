use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;

/// Cancels the running turn. Clone it before starting a turn and call
/// [`CancelHandle::cancel`] from another task (e.g. on Ctrl-C).
///
/// A cancel that arrives while no turn is running is kept and applies to the next turn
/// (so an early Ctrl-C in headless mode isn't lost). Call
/// [`CancelHandle::clear_pending`] to drop it.
#[derive(Clone, Default)]
pub struct CancelHandle(Arc<Mutex<CancelState>>);

#[derive(Default)]
struct CancelState {
    token: CancellationToken,
    active: bool,
    pending: bool,
}

impl CancelHandle {
    pub fn cancel(&self) {
        let mut s = self.0.lock().expect("cancel lock");
        if s.active {
            s.token.cancel();
        } else {
            s.pending = true;
        }
    }

    /// Whether the running turn is cancelled, or a cancel is waiting for the next one.
    pub fn is_cancelled(&self) -> bool {
        let s = self.0.lock().expect("cancel lock");
        s.pending || (s.active && s.token.is_cancelled())
    }

    /// Forgets a cancel requested while no turn was running.
    pub fn clear_pending(&self) {
        self.0.lock().expect("cancel lock").pending = false;
    }

    /// Starts a turn: a fresh token, already cancelled if a cancel is pending.
    pub(super) fn begin(&self) -> CancellationToken {
        let mut s = self.0.lock().expect("cancel lock");
        s.token = CancellationToken::new();
        if std::mem::take(&mut s.pending) {
            s.token.cancel();
        }
        s.active = true;
        s.token.clone()
    }

    pub(super) fn finish(&self) {
        self.0.lock().expect("cancel lock").active = false;
    }
}
