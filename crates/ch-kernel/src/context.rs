//! Context strategies decide what part of the history the model sees.

use std::sync::Arc;

use async_trait::async_trait;

use crate::cx::Cx;
use crate::history::History;
use crate::model::ModelProvider;
use crate::tokens::TokenCalibrator;
use crate::types::Message;

/// The token budget for one model call.
#[derive(Debug, Clone, Copy)]
pub struct Budget {
    /// Model context window in tokens.
    pub window: u64,
    /// Tokens reserved for the model's output.
    pub reserved_output: u64,
    /// Calibrated tokens used by the system prompt, tool specs and reminders.
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

/// Extra inputs a strategy may need: the summarizer model.
#[derive(Clone)]
pub struct ContextCx {
    pub cx: Cx,
    pub summarizer: Option<Arc<dyn ModelProvider>>,
    pub summarizer_model: String,
}

#[async_trait]
pub trait ContextStrategy: Send + Sync {
    fn id(&self) -> &str;

    /// Produces the messages for the next model call. Must keep every `tool_use` paired
    /// with its `tool_result`, start with a user message, and end with the latest user
    /// message unchanged.
    async fn view(&self, history: &History, budget: Budget, cx: &ContextCx)
    -> anyhow::Result<View>;

    /// Manual compaction (`/compact`). Returns a short description of what happened.
    async fn compact(
        &self,
        _history: &History,
        _budget: Budget,
        _cx: &ContextCx,
    ) -> anyhow::Result<String> {
        Ok("this context strategy does not support manual compaction".into())
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
