//! Context strategies decide what part of the history the model sees.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::cx::Cx;
use crate::events::AgentEvent;
use crate::history::History;
use crate::model::{
    ModelError, ModelProvider, ModelRequest, ModelResponse, complete, estimate_request_tokens,
};
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

/// Extra inputs a strategy may need: the agent's provider and request, and the
/// summarizer.
#[derive(Clone)]
pub struct ContextCx {
    pub cx: Cx,
    /// The agent's provider: the one `agent_request` goes to.
    pub provider: Arc<dyn ModelProvider>,
    /// The agent's request for this step without its messages: the model, system blocks,
    /// tools and settings it is sent with (`before_model` hooks can't change them). Its
    /// model is the current one, so it follows `/model`. A summary sent with these after
    /// the last view reads that view from the prompt cache.
    pub agent_request: ModelRequest,
    /// A separate provider and model for summaries, set only by `[model.summarizer]` (a
    /// `provider` without an `id` means that provider's default model). It can't read
    /// the agent's cache, so it needs the messages as a transcript. With `None`,
    /// summaries go to `provider` with `agent_request.model`.
    pub summarizer: Option<(Arc<dyn ModelProvider>, String)>,
    /// The model calls the strategy made itself; [`ContextCx::complete`] records them.
    pub usage: UsageLog,
}

impl ContextCx {
    /// Runs one model call for the strategy, such as a summary, to completion. It emits
    /// `ModelRequest`, and records the call's usage under `request.model` and
    /// `request.purpose`, so it counts in `/cost` and the saved session. `budget` gives
    /// the window and the calibration for the event. Cancelling the turn cancels the call.
    /// A call made by `/compact`, outside a turn, carries the last turn's number and
    /// step 0.
    pub async fn complete(
        &self,
        provider: &dyn ModelProvider,
        request: ModelRequest,
        budget: &Budget,
    ) -> Result<ModelResponse, ModelError> {
        self.cx.emit(AgentEvent::ModelRequest {
            turn: self.cx.turn,
            step: self.cx.step,
            model: request.model.clone(),
            purpose: request.purpose.clone(),
            messages: request.messages.len(),
            tokens_est: budget
                .calibrator
                .calibrate(estimate_request_tokens(&request)),
            context_window: budget.window,
        });
        let (model, purpose) = (request.model.clone(), request.purpose.clone());
        let cancel = self.cx.cancel.clone();
        let response = tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(ModelError::Cancelled),
            r = complete(provider, request, cancel.clone()) => r,
        }?;
        self.usage.record(CallUsage {
            model,
            purpose,
            usage: response.usage,
        });
        Ok(response)
    }
}

/// One model call a strategy made itself.
#[derive(Debug, Clone, PartialEq)]
pub struct CallUsage {
    pub model: String,
    /// Tags the call's `Usage` event, e.g. "context.summarize".
    pub purpose: String,
    pub usage: Usage,
}

/// The model calls a strategy made itself, such as summaries. When the strategy
/// returns, the session adds them to its totals (`/cost` and the saved session) and
/// emits `Usage` for each.
#[derive(Clone, Default)]
pub struct UsageLog(Arc<Mutex<Vec<CallUsage>>>);

impl UsageLog {
    /// Records a call made without [`ContextCx::complete`].
    pub fn record(&self, call: CallUsage) {
        self.0.lock().expect("usage lock").push(call);
    }

    /// Removes the calls recorded so far and returns them, oldest first.
    pub(crate) fn take(&self) -> Vec<CallUsage> {
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
    /// A short description, shown to the user after `/compact` only.
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
