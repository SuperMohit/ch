//! Command-line arguments.

use std::path::PathBuf;

use clap::{Parser, ValueEnum};

/// ch: a plugin-based coding agent.
///
/// Without `-p`, starts the interactive REPL (running PROMPT first, if given).
/// With `-p`, runs one task headlessly and exits with a code that reflects how it ended:
/// 0 done, 1 error, 2 step limit, 3 context overflow, 4 stopped by a hook, 5 prompt
/// blocked, 130 cancelled.
#[derive(Debug, Clone, Parser)]
#[command(name = "ch", version, about, long_about)]
pub struct Cli {
    /// The task. In REPL mode it runs as the first turn.
    pub prompt: Option<String>,

    /// Run headlessly. Takes the task as its value, or PROMPT; `-` (or piped stdin)
    /// reads it from stdin.
    #[arg(short = 'p', long = "print", value_name = "PROMPT", num_args = 0..=1)]
    pub print: Option<Option<String>>,

    /// Output format for `-p`.
    #[arg(long, value_enum, default_value_t = OutputFormatArg::Text)]
    pub output_format: OutputFormatArg,

    /// With `-p`, also print tool calls and notes to stderr.
    #[arg(long)]
    pub verbose: bool,

    /// Model id (sets `model.id`).
    #[arg(long, value_name = "MODEL")]
    pub model: Option<String>,

    /// Apply `[profiles.<NAME>]` from the config.
    #[arg(long, value_name = "NAME")]
    pub profile: Option<String>,

    /// Override a config value, e.g. `--set plugins.context.strategy=elide`.
    #[arg(long = "set", value_name = "KEY=VALUE")]
    pub set: Vec<String>,

    /// Allow a permission rule for this run (appends to `plugins.safety.allow`), e.g.
    /// `--allow edit_file` or `--allow 'bash:cargo *'`.
    #[arg(long = "allow", value_name = "RULE")]
    pub allow: Vec<String>,

    /// Permission mode (sets `plugins.safety.mode`).
    #[arg(long, value_enum, value_name = "MODE")]
    pub permission_mode: Option<PermissionMode>,

    /// Apply settings from the project's `.ch/config.toml` that need trust (permission
    /// mode, allow rules, endpoints, commands, ...) for this run, without asking.
    #[arg(long)]
    pub trust_project: bool,

    /// Approver for `-p`: what happens when a call needs approval.
    #[arg(long, value_enum, default_value_t = ApproverArg::AutoDeny)]
    pub approver: ApproverArg,

    /// Continue the most recent session in this project.
    #[arg(short = 'c', long = "continue", conflicts_with = "resume")]
    pub continue_last: bool,

    /// Resume a session by id; without an id, choose from a list.
    #[arg(short = 'r', long, value_name = "ID", num_args = 0..=1)]
    pub resume: Option<Option<String>>,

    /// List the plugins, whether they're enabled and why, and what they register.
    #[arg(long)]
    pub list_plugins: bool,

    /// Print where configuration, sessions and logs live, then exit.
    #[arg(long)]
    pub config_path: bool,

    /// Run in this directory (the workspace) instead of the current one.
    #[arg(long, value_name = "DIR")]
    pub cwd: Option<PathBuf>,
}

impl Cli {
    /// Headless mode (`-p`).
    pub fn is_print(&self) -> bool {
        self.print.is_some()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormatArg {
    Text,
    Json,
    StreamJson,
}

impl OutputFormatArg {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Json => "json",
            Self::StreamJson => "stream-json",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum PermissionMode {
    /// Read-only actions run; edits and non-read-only commands ask.
    Ask,
    /// Like ask, but edits inside the workspace are allowed.
    AcceptEdits,
    /// Everything is allowed except the built-in deny list.
    Bypass,
}

impl PermissionMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::AcceptEdits => "accept-edits",
            Self::Bypass => "bypass",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ApproverArg {
    AutoDeny,
    AutoAllow,
}

impl ApproverArg {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::AutoDeny => "auto-deny",
            Self::AutoAllow => "auto-allow",
        }
    }
}
