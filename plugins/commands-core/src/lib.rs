//! Core slash commands: `/help`, `/clear`, `/compact`, `/model`, `/cost`, `/tools`,
//! `/plugins`, `/config`, `/resume`, `/exit` and `/quit`.
//!
//! Every command acts on the [`ch_kernel::Session`] it is given and answers with an
//! [`ch_kernel::Outcome::Message`] in markdown (or [`ch_kernel::Outcome::Exit`]).

pub mod commands;
pub mod format;

use std::sync::Arc;

use ch_kernel::{Plugin, PluginManifest, RegisterCx};

/// Plugin id.
pub const ID: &str = "commands-core";

/// Names of the commands this plugin registers. Other command plugins (such as
/// `commands-markdown`) skip these to avoid collisions.
pub const COMMAND_NAMES: &[&str] = &[
    "help", "clear", "compact", "model", "cost", "tools", "plugins", "config", "resume", "exit",
    "quit",
];

/// Registers the core slash commands.
pub struct CommandsCorePlugin;

/// Returns the plugin as a trait object.
pub fn plugin() -> Arc<dyn Plugin> {
    Arc::new(CommandsCorePlugin)
}

impl Plugin for CommandsCorePlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest::new(ID, "Core slash commands (/help, /model, /resume, ...)")
            .provides(&["commands"])
    }

    fn register(&self, cx: &mut RegisterCx<'_>) -> anyhow::Result<()> {
        use commands::*;
        cx.add_command(Help);
        cx.add_command(Clear);
        cx.add_command(Compact);
        cx.add_command(Model);
        cx.add_command(Cost);
        cx.add_command(Tools);
        cx.add_command(Plugins);
        cx.add_command(ConfigCmd);
        cx.add_command(Resume);
        cx.add_command(Exit::new("exit"));
        cx.add_command(Exit::new("quit"));
        Ok(())
    }
}
