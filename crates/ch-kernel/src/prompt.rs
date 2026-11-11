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

    /// Reminders for this model call. The kernel wraps each in `<system-reminder>` and
    /// adds it to the end of the latest user message, in the history as well as the
    /// request, so later requests repeat it unchanged (signed thinking and the prompt
    /// cache both need the conversation to only grow). Return one only when it says
    /// something new, such as once per turn.
    async fn turn_reminders(&self, _cx: &Cx) -> Vec<String> {
        Vec::new()
    }
}
