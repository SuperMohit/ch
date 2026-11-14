//! Command-line arguments.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

/// ch-eval: runs a suite of tasks with `ch -p` across a matrix of configs, each run in a
/// fresh Docker container, and reports how the configs compare.
///
/// Every run calls the model, so a matrix costs money: check its size with --dry-run
/// and cap it with --max-cost. The model's commands run unattended inside the
/// container. Its only way out is a credential proxy that adds the API key to model
/// calls; the key never enters the container, but a dedicated key with a spending
/// limit is still the safe choice.
#[derive(Debug, Clone, Parser)]
#[command(name = "ch-eval", version, about, long_about)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Clone, Subcommand)]
pub enum Command {
    /// Build the `ch-eval-base` image and a static `ch` for the containers.
    Build,
    /// Run the suite's tasks in every cell of the matrix, then write the report.
    Run(Box<RunArgs>),
    /// Write report.md and results.json again from the run.json files in OUT.
    Report {
        /// The output directory of `ch-eval run`.
        out: PathBuf,
    },
    /// Compare the runs of several output directories, such as one per `ch` build: each
    /// directory's cells are named after it, and the paired comparison covers them.
    Compare {
        /// Output directories of `ch-eval run`, with the same tasks.
        #[arg(required = true, num_args = 2..)]
        dirs: Vec<PathBuf>,
        /// Where report.md and results.json go.
        #[arg(long, value_name = "DIR")]
        to: PathBuf,
        /// Compare even when the base config or the cells differ; the report lists how.
        #[arg(long)]
        allow_different_config: bool,
    },
    /// The credential proxy, run inside its container by `ch-eval run`.
    #[command(hide = true)]
    Proxy {
        /// The proxy's state directory (proxy.json, tokens, ledger).
        #[arg(long, value_name = "DIR")]
        state: PathBuf,
        #[arg(long, value_name = "ADDR", default_value = "0.0.0.0:8080")]
        listen: String,
    },
    /// Run COMMAND for at most SECS seconds: every container's entrypoint.
    #[command(hide = true)]
    Limit {
        secs: u64,
        #[arg(last = true, required = true, value_name = "COMMAND")]
        command: Vec<String>,
    },
}

#[derive(Debug, Clone, Args)]
pub struct RunArgs {
    /// The suite: a directory with one subdirectory per task [default: eval/tasks in
    /// the source tree]
    #[arg(long, value_name = "DIR")]
    pub suite: Option<PathBuf>,

    /// Where the runs, report.md and results.json go. Running again with the same
    /// directory resumes: runs that have a run.json are kept.
    #[arg(long, value_name = "DIR")]
    pub out: PathBuf,

    /// Base config for every run (model, provider, pricing), copied to each run's ch
    /// home as config.toml.
    #[arg(long, value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// Apply `[profiles.NAME]` from the base config in every run.
    #[arg(long, value_name = "NAME")]
    pub profile: Option<String>,

    /// Add a matrix dimension: each value is a TOML literal passed as `--set KEY=V`,
    /// e.g. `--vary plugins.context.strategy='"none"','"staged"'`. A KEY without a dot
    /// is a plugin, switched `on` or `off`: `--vary planning=on,off`.
    #[arg(long, value_name = "KEY=V1,V2")]
    pub vary: Vec<String>,

    /// Runs per task and cell.
    #[arg(long, value_name = "N", default_value_t = 1,
          value_parser = clap::value_parser!(u32).range(1..))]
    pub repeat: u32,

    /// Runs at once.
    #[arg(short = 'j', long, value_name = "N", default_value_t = 2,
          value_parser = clap::value_parser!(u32).range(1..))]
    pub jobs: u32,

    /// Stop once the recorded cost reaches USD: the proxy refuses further model calls,
    /// and runs it cut off are made again on resume. Needs `[model.pricing]` in the
    /// config.
    #[arg(long, value_name = "USD")]
    pub max_cost: Option<f64>,

    /// Print the planned runs and the command line of one, then exit.
    #[arg(long)]
    pub dry_run: bool,

    /// Try a run at most N times when it ends because the model API failed (rate
    /// limits, overloads, server errors), waiting longer before each try; past that
    /// the matrix stops.
    #[arg(long, value_name = "N", default_value_t = 3,
          value_parser = clap::value_parser!(u32).range(1..))]
    pub max_attempts: u32,

    /// Use this static Linux `ch` instead of building one.
    #[arg(long, value_name = "PATH")]
    pub ch: Option<PathBuf>,

    /// Run only these tasks (by id).
    #[arg(long = "task", value_name = "ID", value_delimiter = ',')]
    pub tasks: Vec<String>,

    /// Run only tasks with one of these tags.
    #[arg(long = "tag", value_name = "TAG", value_delimiter = ',')]
    pub tags: Vec<String>,
}
