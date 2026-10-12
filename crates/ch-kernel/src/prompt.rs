//! Prompt contributors: system prompt blocks and per-step reminders.

use async_trait::async_trait;

use crate::cx::Cx;

#[async_trait]
pub trait PromptContributor: Send + Sync {
    fn id(&self) -> &str;

    /// Lower comes first in the system prompt. `prompt-base` uses 0, project
    /// instructions 100, feature blocks (planning, ...) 200.
    fn priority(&self) -> i32 {
        100
    }

    /// Blocks for the system prompt. Called before every model call, so keep them
    /// stable across calls to benefit from prompt caching.
    async fn system_blocks(&self, _cx: &Cx) -> Vec<String> {
        Vec::new()
    }

    /// Reminders added to the end of the latest user message for this model call only.
    /// They are never stored in history. The kernel wraps each in `<system-reminder>`.
    async fn turn_reminders(&self, _cx: &Cx) -> Vec<String> {
        Vec::new()
    }
}
