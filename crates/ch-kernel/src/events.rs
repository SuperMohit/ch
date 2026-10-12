//! The agent event stream. Frontends and telemetry sinks subscribe to it.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::broadcast;

use crate::types::{Message, TerminationReason, Usage};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionKind {
    Elided,
    Summarized,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    SessionStart {
        session_id: String,
        model: String,
        workspace: String,
        plugins: Vec<String>,
    },
    TurnStart {
        turn: u32,
        prompt: String,
    },
    /// A model call is about to be made.
    ModelRequest {
        turn: u32,
        step: u32,
        model: String,
        purpose: String,
        messages: usize,
        /// Calibrated estimate of the prompt size.
        tokens_est: u64,
        context_window: u64,
    },
    TextDelta {
        text: String,
    },
    ThinkingDelta {
        text: String,
    },
    /// The assembled assistant message for one model call.
    AssistantMessage {
        turn: u32,
        step: u32,
        message: Message,
    },
    /// The model started writing a tool call; its input is still streaming. Large
    /// inputs (a whole file for `write_file`) can take minutes.
    ToolInputStarted {
        id: String,
        name: String,
    },
    /// How much of a tool call's input has arrived so far. Sent at most a few times a
    /// second while the input streams.
    ToolInputProgress {
        id: String,
        name: String,
        bytes: u64,
    },
    ToolCallStart {
        id: String,
        name: String,
        input: Value,
        /// One-line human summary from `Tool::summarize`.
        summary: String,
    },
    ToolCallEnd {
        id: String,
        name: String,
        content: String,
        is_error: bool,
        duration_ms: u64,
        #[serde(default, skip_serializing_if = "Value::is_null")]
        metadata: Value,
    },
    ApprovalRequested {
        tool: String,
        summary: String,
        reason: String,
    },
    ApprovalResolved {
        tool: String,
        approved: bool,
    },
    /// A hook or plugin added a note (to a tool result or as a reminder).
    Note {
        source: String,
        text: String,
    },
    HookDenied {
        hook: String,
        tool: Option<String>,
        reason: String,
    },
    ContextCompacted {
        kind: CompactionKind,
        tokens_before: u64,
        tokens_after: u64,
    },
    Usage {
        usage: Usage,
        /// Cost of this call in USD, when pricing is configured.
        cost: Option<f64>,
        purpose: String,
    },
    TurnEnd {
        turn: u32,
        reason: TerminationReason,
        steps: u32,
    },
    /// Something went wrong that the user should know about but that didn't end the turn.
    Warning {
        message: String,
    },
    /// Plugin-defined events (plan updates, diagnostics results, ...).
    Custom {
        source: String,
        name: String,
        data: Value,
    },
    SessionEnd {
        session_id: String,
    },
}

/// Broadcast bus. Cloning shares the channel.
#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<AgentEvent>,
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl EventBus {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(4096);
        Self { tx }
    }

    /// Sends an event. Having no subscribers is fine.
    pub fn emit(&self, event: AgentEvent) {
        let _ = self.tx.send(event);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<AgentEvent> {
        self.tx.subscribe()
    }
}

/// Receives every agent event, e.g. to send telemetry. Must never fail the agent:
/// log and swallow errors.
#[async_trait]
pub trait EventSink: Send + Sync {
    fn id(&self) -> &str;
    async fn handle(&self, event: &AgentEvent);
    /// Called after `SessionEnd`, before the process exits.
    async fn flush(&self) {}
}
