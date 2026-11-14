//! The `ch` binary.

use clap::Parser;

use ch_cli::{Cli, builtin_plugins, config, logging};

fn main() {
    harden();
    let code = real_main();
    std::process::exit(code);
}

/// Marks this process as not dumpable, so other processes of the same user, such as
/// the model's commands, can't read its environment (with the API key), memory or open
/// files through `/proc/<pid>` or attach to it with ptrace, and no core dump is written.
/// Programs `ch` starts are dumpable again once they `exec`. Linux only; elsewhere
/// nothing changes.
#[cfg(target_os = "linux")]
fn harden() {
    if let Err(e) = nix::sys::prctl::set_dumpable(false) {
        eprintln!("ch: warning: couldn't mark the process as not dumpable: {e}");
    }
}

#[cfg(not(target_os = "linux"))]
fn harden() {}

fn real_main() -> i32 {
    let cli = Cli::parse();
    // Logging is best-effort: without a home directory, run without it.
    let _log_guard = config::ch_home()
        .ok()
        .and_then(|home| logging::init(&home.join("logs")));

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: starting the async runtime: {e}");
            return 1;
        }
    };
    let result = runtime.block_on(ch_cli::run(cli, builtin_plugins()));
    // Don't wait for stray background tasks (e.g. a pending stdin read).
    runtime.shutdown_timeout(std::time::Duration::from_secs(1));
    match result {
        Ok(code) => code,
        Err(e) => {
            tracing::error!("{e:#}");
            eprintln!("error: {e:#}");
            1
        }
    }
}
