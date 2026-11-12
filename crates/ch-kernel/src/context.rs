//! Context strategies decide what part of the history the model sees.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::cx::Cx;
use crate::history::History;
use crate::model::{ModelProvider, ModelRequest};
use crate::tokens::TokenCalibrator;
use crate::types::{Message, Usage};

/// The token budget for one model call.
#[derive(Debug, Clone, Copy)]
pub struct Budget {
    /// Model context window in tokens.
    pub window: u64,
    /// Tokens reserved for the model's output.
    pub reserved_output: u64,
    /// Calibrated tokens used by the system prompt, tool specs and this call's reminders.
    /// Reminders from earlier calls are in the history.
    pub fixed_tokens: u64,
    pub calibrator: TokenCalibrator,
}

impl Budget {
    /// Tokens available for messages.
    pub fn usable(&self) -> u64 {
        self.window
            .saturating_sub(self.reserved_output)
            .saturating_sub(self.fixed_tokens)
    }

    /// Calibrated size of `messages`.
    pub fn measure(&self, messages: &[Message]) -> u64 {
        let raw: u64 = messages
            .iter()
            .map(|m| crate::tokens::estimate_tokens(m.char_len()))
            .sum();
        self.calibrator.calibrate(raw)
    }
}

/// What a strategy hands to the model.
#[derive(Debug, Clone, Default)]
pub struct View {
    pub messages: Vec<Message>,
}

/// Extra inputs a strategy may need: the agent's request and the summarizer model.
#[derive(Clone)]
pub struct ContextCx {
    pub cx: Cx,
    /// The agent's request for this step without its messages: model, system blocks,
    /// tools and settings. A summary sent with the same ones, after the last view, reads
    /// that view from the prompt cache.
    pub agent_request: ModelRequest,
    /// `[model.summarizer]`, or else the agent's provider and current model.
    pub summarizer: Arc<dyn ModelProvider>,
    pub summarizer_model: String,
    /// Where the strategy records the model calls it makes itself.
    pub usage: UsageLog,
}

/// The model calls a strategy makes itself, such as summaries. When the strategy
/// returns, the session adds them to its totals (`/cost` and the saved session) and
/// emits `Usage` for each.
#[derive(Clone, Default)]
pub struct UsageLog(Arc<Mutex<Vec<(String, String, Usage)>>>);

impl UsageLog {
    /// Records one call to `model`. `purpose` tags its `Usage` event, e.g.
    /// "context.summarize".
    pub fn record(&self, model: &str, purpose: &str, usage: Usage) {
        self.0
            .lock()
            .expect("usage lock")
            .push((model.to_string(), purpose.to_string(), usage));
    }

    /// Removes the calls recorded so far and returns them, oldest first, as
    /// `(model, purpose, usage)`.
    pub fn take(&self) -> Vec<(String, String, Usage)> {
        std::mem::take(&mut *self.0.lock().expect("usage lock"))
    }
}

/// Why a strategy is asked to compact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactReason {
    /// The user ran `/compact`.
    Manual,
    /// The provider rejected the request as too long. The session sends it again, with
    /// a new view, only if the compaction changed something.
    Overflow,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactRequest {
    pub reason: CompactReason,
    /// What the user asked the summary to focus on (`/compact <instructions>`).
    pub instructions: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactOutcome {
    /// The next view differs from the last one.
    pub changed: bool,
    /// A short description for the user.
    pub message: String,
}

#[async_trait]
pub trait ContextStrategy: Send + Sync {
    fn id(&self) -> &str;

    /// Produces the messages for the next model call. Must keep every `tool_use` paired
    /// with its `tool_result`, start with a user message, and end with the latest user
    /// message unchanged.
    ///
    /// Between compactions, each view must be the previous one with the new messages
    /// added: changing an earlier message invalidates the model's signed thinking from
    /// that point on, and the prompt cache. Rewrite earlier messages only when
    /// compacting, rarely and in batches.
    async fn view(&self, history: &History, budget: Budget, cx: &ContextCx)
    -> anyhow::Result<View>;

    /// Compacts now, on `/compact` or after an overflow, so that the next view is
    /// smaller. Says whether anything changed, and what.
    async fn compact(
        &self,
        _history: &History,
        _budget: Budget,
        _request: &CompactRequest,
        _cx: &ContextCx,
    ) -> anyhow::Result<CompactOutcome> {
        Ok(CompactOutcome {
            changed: false,
            message: "this context strategy does not support manual compaction".into(),
        })
    }
}

/// Sends the whole history. Used when no context plugin is enabled.
pub struct PassthroughContext;

#[async_trait]
impl ContextStrategy for PassthroughContext {
    fn id(&self) -> &str {
        "passthrough"
    }

    async fn view(
        &self,
        history: &History,
        _budget: Budget,
        _cx: &ContextCx,
    ) -> anyhow::Result<View> {
        Ok(View {
            messages: history.messages(),
        })
    }
}
