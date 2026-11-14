//! Eval runner for ch (`ch-eval`), as a library so the pieces can be tested.
//!
//! For each task, matrix cell and repeat, `ch-eval run` runs `ch -p` in a fresh Docker
//! container that holds only a copy of the task's repo, then runs the task's check in a
//! second container. It saves the event stream of every run and writes a report that
//! compares the cells. See `docs/adr/0005-eval-runner.md` and `eval/README.md`.
//!
//! - [`task`]: reading a suite of tasks.
//! - [`matrix`]: `--vary` dimensions, cells, run order and `ch`'s command line.
//! - [`run`]: one run (workspace, agent, diff, check, `run.json`) and the scheduler.
//! - [`backend`] and [`docker`]: where the steps run; [`limit`] bounds each container.
//! - [`proxy`] and [`ledger`]: the credential proxy, and its record of every model call.
//! - [`fingerprint`]: what each run was made from.
//! - [`events`]: counting what the harness did from the event stream.
//! - [`report`] and [`stats`]: `report.md` and `results.json`.
//!
//! `ch-eval` treats tasks as data: the check is an opaque shell command, and nothing
//! here knows about any language.

pub mod app;
pub mod args;
pub mod backend;
pub mod docker;
pub mod events;
pub mod fingerprint;
pub mod ledger;
pub mod limit;
pub mod matrix;
pub mod proxy;
pub mod record;
pub mod report;
pub mod run;
pub mod stats;
pub mod task;
pub mod workspace;

pub use args::Cli;
