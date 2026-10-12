//! Slash commands.

use async_trait::async_trait;

use crate::session::Session;

/// What the frontend should do after a command runs.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// Nothing to show.
    Done,
    /// Show this text to the user (markdown).
    Message(String),
    /// Send this text to the model as the next user prompt.
    Prompt(String),
    /// Exit the frontend.
    Exit,
}

pub struct CmdCx<'a> {
    pub session: &'a mut Session,
}

#[async_trait]
pub trait Command: Send + Sync {
    /// Name without the slash.
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    /// Short usage hint, e.g. `[model-id]`.
    fn usage(&self) -> &str {
        ""
    }
    /// When another plugin registers a command with the same name, a command that
    /// yields is dropped (with a warning) instead of failing startup. User-defined
    /// commands (markdown files, ...) should yield.
    fn yields_on_conflict(&self) -> bool {
        false
    }
    async fn run(&self, args: &str, cx: &mut CmdCx<'_>) -> anyhow::Result<Outcome>;
}
