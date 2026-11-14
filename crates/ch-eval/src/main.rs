//! The `ch-eval` binary.

use clap::Parser;

use ch_eval::{Cli, app};

fn main() {
    let cli = Cli::parse();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: starting the async runtime: {e}");
            std::process::exit(1);
        }
    };
    let code = match runtime.block_on(app::main_with(cli)) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            1
        }
    };
    std::process::exit(code);
}
