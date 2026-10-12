//! Tracing to `<ch home>/logs/ch.log` (daily rolling), never to the terminal.
//!
//! The level comes from `CH_LOG` (an `EnvFilter` directive such as `debug` or
//! `ch_kernel=trace,warn`), defaulting to `warn`.

use std::path::Path;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;

/// Starts logging. Keep the guard alive until exit so buffered lines are written.
/// Returns `None` (and logs nothing) if the log directory can't be created.
pub fn init(log_dir: &Path) -> Option<WorkerGuard> {
    std::fs::create_dir_all(log_dir).ok()?;
    let filter = EnvFilter::try_from_env("CH_LOG").unwrap_or_else(|_| EnvFilter::new("warn"));
    let appender = tracing_appender::rolling::daily(log_dir, "ch.log");
    let (writer, guard) = tracing_appender::non_blocking(appender);
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(writer)
        .with_ansi(false)
        .try_init()
        .ok()?;
    Some(guard)
}
