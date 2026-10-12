//! Frontends drive a session: REPL, headless print mode, TUI.

use async_trait::async_trait;

use crate::session::Session;

#[async_trait]
pub trait Frontend: Send + Sync {
    fn id(&self) -> &str;
    /// Runs until the user exits. Returns the process exit code.
    async fn run(&self, session: Session) -> anyhow::Result<i32>;
}
