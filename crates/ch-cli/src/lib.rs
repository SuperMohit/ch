//! The `ch` command-line coding agent, as a library so the pieces can be tested.
//!
//! `main.rs` parses [`args::Cli`], starts logging and calls [`app::run`] with
//! [`plugins::builtin_plugins`].

pub mod app;
pub mod args;
pub mod config;
pub mod list;
pub mod logging;
pub mod plugins;
pub mod sessions;
pub mod trust;

pub use app::{Stdin, run, run_with};
pub use args::Cli;
pub use plugins::builtin_plugins;
