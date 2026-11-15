//! The agent event stream. Frontends and telemetry sinks subscribe to it.
//!
//! Each event serializes as `{"type": "<snake_case name>", ...}`. The same JSON is
//! what `ch -p --output-format stream-json` prints and what external plugins receive
//! with `event/notify`, so names here are a public format. New names follow these
//! rules; a test at the bottom of this file lists every type name and checks the third:
//!
//! - Sessions, turns and tool calls are bracketed by `…Start` and `…End`.
//!   `ToolInputStart` and `ToolInputProgress` report a tool use the model is still
//!   writing; the `AssistantMessage` holding it ends that.
//! - An event that reports a change of state is in the past tense (`ApprovalRequested`,
//!   `ContextCompacted`, `ToolsChanged`). One that carries a thing is named for it
//!   (`Note`, `Usage`, `Warning`, `AssistantMessage`, `TextDelta`).
//! - `source` only means a tool source (an MCP server, or an external plugin's tools),
//!   as in `ToolSource`. Who emitted an event is `plugin`: a built-in plugin's id, or
//!   `ext:<name>` for an external plugin. A hook is named by `hook`; a built-in hook's
//!   id is its plugin's id.
//! - `name` is the event's own subject (the tool in `ToolCallStart`, the event in
//!   `Custom`); `tool` refers to a tool from an event about something else
//!   (`ApprovalRequested`, `HookDenied`).
//!
//! A *tool use* is the block the model wrote, kept in the history with its `id`. A *tool
//! call* is that request being handled, run or refused: `ToolCallStart` and
//! `ToolCallEnd` carry the tool use's `id`, and come in pairs.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::broadcast;

use crate::tool_source::SourceStatus;
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
    /// The model started writing a tool use; its input is still streaming. Large
    /// inputs (a whole file for `write_file`) can take minutes.
    ToolInputStart {
        id: String,
        name: String,
    },
    /// How much of a tool use's input has arrived so far. Sent at most a few times a
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
    /// A hook or plugin added a note (to a tool result or as a reminder), or an external
    /// plugin asked to show one.
    Note {
        /// The plugin that added it (a hook's id is its plugin's id), or `ext:<name>`
        /// for an external plugin.
        plugin: String,
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
    /// The model was told about tools added or withdrawn since its last call.
    ToolsChanged {
        added: Vec<String>,
        removed: Vec<String>,
    },
    /// A tool source (an MCP server, an external plugin) started, failed, exited or
    /// reported a new list of tools.
    ToolSource {
        source: String,
        status: SourceStatus,
    },
    /// Plugin-defined events (plan updates, diagnostics results, ...).
    Custom {
        /// The id of the plugin that emitted it (`ext:<name>` for an external plugin).
        plugin: String,
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::types::Message;

    /// One event of every kind.
    fn samples() -> Vec<AgentEvent> {
        let s = String::new;
        vec![
            AgentEvent::SessionStart {
                session_id: s(),
                model: s(),
                workspace: s(),
                plugins: vec![],
            },
            AgentEvent::TurnStart {
                turn: 1,
                prompt: s(),
            },
            AgentEvent::ModelRequest {
                turn: 1,
                step: 1,
                model: s(),
                purpose: s(),
                messages: 0,
                tokens_est: 0,
                context_window: 0,
            },
            AgentEvent::TextDelta { text: s() },
            AgentEvent::ThinkingDelta { text: s() },
            AgentEvent::AssistantMessage {
                turn: 1,
                step: 1,
                message: Message::assistant(""),
            },
            AgentEvent::ToolInputStart { id: s(), name: s() },
            AgentEvent::ToolInputProgress {
                id: s(),
                name: s(),
                bytes: 0,
            },
            AgentEvent::ToolCallStart {
                id: s(),
                name: s(),
                input: json!({}),
                summary: s(),
            },
            AgentEvent::ToolCallEnd {
                id: s(),
                name: s(),
                content: s(),
                is_error: false,
                duration_ms: 0,
                metadata: Value::Null,
            },
            AgentEvent::ApprovalRequested {
                tool: s(),
                summary: s(),
                reason: s(),
            },
            AgentEvent::ApprovalResolved {
                tool: s(),
                approved: true,
            },
            AgentEvent::Note {
                plugin: s(),
                text: s(),
            },
            AgentEvent::HookDenied {
                hook: s(),
                tool: None,
                reason: s(),
            },
            AgentEvent::ContextCompacted {
                kind: CompactionKind::Elided,
                tokens_before: 0,
                tokens_after: 0,
            },
            AgentEvent::Usage {
                usage: Usage::default(),
                cost: None,
                purpose: s(),
            },
            AgentEvent::TurnEnd {
                turn: 1,
                reason: TerminationReason::HookStop {
                    hook: s(),
                    reason: s(),
                },
                steps: 0,
            },
            AgentEvent::Warning { message: s() },
            AgentEvent::ToolsChanged {
                added: vec![],
                removed: vec![],
            },
            AgentEvent::ToolSource {
                source: s(),
                status: SourceStatus::Stopped,
            },
            AgentEvent::Custom {
                plugin: s(),
                name: s(),
                data: Value::Null,
            },
            AgentEvent::SessionEnd { session_id: s() },
        ]
    }

    /// Fails to compile when a variant is added, so `samples` and the list below are
    /// updated with it.
    fn covered(e: &AgentEvent) {
        match e {
            AgentEvent::SessionStart { .. }
            | AgentEvent::TurnStart { .. }
            | AgentEvent::ModelRequest { .. }
            | AgentEvent::TextDelta { .. }
            | AgentEvent::ThinkingDelta { .. }
            | AgentEvent::AssistantMessage { .. }
            | AgentEvent::ToolInputStart { .. }
            | AgentEvent::ToolInputProgress { .. }
            | AgentEvent::ToolCallStart { .. }
            | AgentEvent::ToolCallEnd { .. }
            | AgentEvent::ApprovalRequested { .. }
            | AgentEvent::ApprovalResolved { .. }
            | AgentEvent::Note { .. }
            | AgentEvent::HookDenied { .. }
            | AgentEvent::ContextCompacted { .. }
            | AgentEvent::Usage { .. }
            | AgentEvent::TurnEnd { .. }
            | AgentEvent::Warning { .. }
            | AgentEvent::ToolsChanged { .. }
            | AgentEvent::ToolSource { .. }
            | AgentEvent::Custom { .. }
            | AgentEvent::SessionEnd { .. } => {}
        }
    }

    /// Every key in `v`, at any depth, with the event type it appears in.
    fn keys(v: &Value, out: &mut Vec<String>) {
        match v {
            Value::Object(map) => {
                for (k, v) in map {
                    out.push(k.clone());
                    keys(v, out);
                }
            }
            Value::Array(items) => items.iter().for_each(|v| keys(v, out)),
            _ => {}
        }
    }

    /// The JSON is a public format (stream-json, `event/notify`): renaming a variant
    /// fails here, and so does a `source` that isn't a tool source.
    #[test]
    fn wire_names_follow_the_naming_rules() {
        let events = samples();
        events.iter().for_each(covered);
        let wire: Vec<Value> = events
            .into_iter()
            .map(|e| serde_json::to_value(e).unwrap())
            .collect();
        let types: Vec<&str> = wire.iter().map(|v| v["type"].as_str().unwrap()).collect();
        assert_eq!(
            types,
            [
                "session_start",
                "turn_start",
                "model_request",
                "text_delta",
                "thinking_delta",
                "assistant_message",
                "tool_input_start",
                "tool_input_progress",
                "tool_call_start",
                "tool_call_end",
                "approval_requested",
                "approval_resolved",
                "note",
                "hook_denied",
                "context_compacted",
                "usage",
                "turn_end",
                "warning",
                "tools_changed",
                "tool_source",
                "custom",
                "session_end",
            ]
        );
        for v in &wire {
            let mut found = Vec::new();
            keys(v, &mut found);
            let t = v["type"].as_str().unwrap();
            assert!(
                t == "tool_source" || !found.iter().any(|k| k == "source"),
                "`source` in {t}: {v}"
            );
            assert!(!found.iter().any(|k| k == "from"), "`from` in {t}: {v}");
        }
        let by_type = |t: &str| wire.iter().find(|v| v["type"] == t).unwrap();
        assert_eq!(by_type("note")["plugin"], "");
        assert_eq!(by_type("custom")["plugin"], "");
        assert_eq!(
            by_type("turn_end")["reason"],
            json!({"kind": "hook_stop", "hook": "", "reason": ""})
        );
        assert_eq!(by_type("tool_source")["source"], "");
    }
}
