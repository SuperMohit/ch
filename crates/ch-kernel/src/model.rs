//! Model providers.

use std::pin::Pin;

use async_trait::async_trait;
use futures::Stream;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::tool::ToolSpec;
use crate::types::{Message, StopReason, Usage};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelRequest {
    /// Model id, as understood by the provider.
    pub model: String,
    /// System prompt blocks, in order. Providers join or send them as separate blocks.
    pub system: Vec<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub max_tokens: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Extended-thinking budget in tokens, when the provider supports it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_budget: Option<u32>,
    /// Free-form tag for telemetry, e.g. "agent" or "context.summarize".
    #[serde(default)]
    pub purpose: String,
    /// How many content blocks at the end of the last message were added for this call
    /// only (reminders, hook notes) and won't be in the next request. Providers that set
    /// prompt-cache breakpoints should put them before these blocks, or the next call
    /// can't reuse the cache.
    #[serde(default)]
    pub volatile_tail: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelResponse {
    /// The fully assembled assistant message.
    pub message: Message,
    pub stop_reason: StopReason,
    pub usage: Usage,
}

/// Events a provider streams back. Providers assemble tool-call JSON themselves and must
/// end a successful stream with exactly one [`ModelEvent::Completed`].
#[derive(Debug, Clone, PartialEq)]
pub enum ModelEvent {
    TextDelta(String),
    ThinkingDelta(String),
    /// A tool call started streaming (its input is not complete yet).
    ToolUseStart {
        id: String,
        name: String,
    },
    /// `len` more bytes of a tool call's input arrived. For progress display only: the
    /// provider still assembles the input and returns it in `Completed`.
    ToolInputDelta {
        id: String,
        len: usize,
    },
    Completed(ModelResponse),
}

#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("context window exceeded: {0}")]
    ContextOverflow(String),
    #[error("rate limited: {0}")]
    RateLimited(String),
    #[error("authentication failed: {0}")]
    Auth(String),
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("provider unavailable: {0}")]
    Unavailable(String),
    #[error("cancelled")]
    Cancelled,
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type ModelStream = Pin<Box<dyn Stream<Item = Result<ModelEvent, ModelError>> + Send>>;

#[async_trait]
pub trait ModelProvider: Send + Sync {
    /// Provider id used in config (`model.provider = "<id>"`).
    fn id(&self) -> &str;

    /// Starts a streamed model call. Retries for transient errors happen inside the
    /// provider; errors that reach the kernel are final.
    async fn stream(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
    ) -> Result<ModelStream, ModelError>;

    /// Context window for `model`, if the provider knows it. Config overrides this.
    fn context_window(&self, _model: &str) -> Option<u64> {
        None
    }

    /// The model used when `model.id` is unset. `None` makes `model.id` required.
    fn default_model(&self) -> Option<&str> {
        None
    }
}

/// Runs a request to completion, ignoring deltas. Useful for tool-free helper calls
/// such as context summaries.
pub async fn complete(
    provider: &dyn ModelProvider,
    request: ModelRequest,
    cancel: CancellationToken,
) -> Result<ModelResponse, ModelError> {
    use futures::StreamExt;
    let mut stream = provider.stream(request, cancel).await?;
    while let Some(ev) = stream.next().await {
        if let ModelEvent::Completed(resp) = ev? {
            return Ok(resp);
        }
    }
    Err(ModelError::Other(anyhow::anyhow!(
        "provider stream ended without a Completed event"
    )))
}
