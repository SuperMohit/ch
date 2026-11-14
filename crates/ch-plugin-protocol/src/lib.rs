//! The protocol between ch and external plugins (roadmap 7.1, ADR 0006).
//!
//! An external plugin is a program ch starts with the session. They talk JSON-RPC 2.0
//! over the plugin's stdin and stdout, one message per line (UTF-8 JSON, no embedded
//! newlines). stderr is free for the plugin's own logging; ch keeps its tail for error
//! messages. A line that isn't JSON-RPC is ignored, and a message over
//! [`MAX_MESSAGE`] bytes is refused.
//!
//! ch → plugin requests:
//!
//! | Method | Params | Result |
//! |---|---|---|
//! | `initialize` | [`InitializeParams`] | [`InitializeResult`]: tools, hooks, prompt contributions, event subscriptions |
//! | `tool/call` | [`ToolCallParams`] | [`ToolCallResult`] |
//! | `hook/invoke` | [`HookInvokeParams`] | [`HookDecision`] |
//! | `prompt/system` | `{}` | [`PromptSystemResult`] |
//! | `prompt/turn` | [`PromptTurnParams`] | [`PromptTurnResult`] |
//! | `command/run` | [`CommandRunParams`] | [`CommandRunResult`] |
//! | `shutdown` | `{}` | `null`; the plugin should then exit |
//!
//! ch → plugin notifications: `event/notify` ([`EventNotifyParams`]) for subscribed
//! events, and `$/cancel` ([`CancelParams`]) when a request is abandoned (a timeout, an
//! interrupted turn).
//!
//! plugin → ch notifications: `log` ([`LogParams`]), `ui/notify` ([`NotifyParams`]),
//! and `tools/changed` ([`ToolsChangedParams`]) to replace its tools mid-session.
//!
//! Commands are declared in `plugin.toml`, not at `initialize`: ch needs their names
//! before any plugin runs.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The protocol version this crate describes. ch refuses a plugin that answers
/// `initialize` with another one.
pub const PROTOCOL_VERSION: u32 = 1;

/// Largest message either side reads.
pub const MAX_MESSAGE: usize = 16 * 1024 * 1024;

pub mod method {
    pub const INITIALIZE: &str = "initialize";
    pub const TOOL_CALL: &str = "tool/call";
    pub const HOOK_INVOKE: &str = "hook/invoke";
    pub const PROMPT_SYSTEM: &str = "prompt/system";
    pub const PROMPT_TURN: &str = "prompt/turn";
    pub const COMMAND_RUN: &str = "command/run";
    pub const SHUTDOWN: &str = "shutdown";
    pub const EVENT_NOTIFY: &str = "event/notify";
    pub const CANCEL: &str = "$/cancel";
    pub const LOG: &str = "log";
    pub const UI_NOTIFY: &str = "ui/notify";
    pub const TOOLS_CHANGED: &str = "tools/changed";
}

/// JSON-RPC error codes.
pub mod code {
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL_ERROR: i64 = -32603;
}

/// A request id: ch always sends numbers.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Id {
    Number(i64),
    String(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }
}

/// One line of the protocol.
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    Request {
        id: Id,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
        params: Value,
    },
    Response {
        id: Id,
        result: Result<Value, RpcError>,
    },
}

/// The wire shape, before it is sorted into a [`Message`].
#[derive(Serialize, Deserialize)]
struct Wire {
    jsonrpc: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    params: Option<Value>,
    /// `Some(Value::Null)` is a present `null`; see [`present`].
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<RpcError>,
}

/// Reads a field that is present as `Some`, even when its value is `null`: a response
/// whose result is `null` is still a response.
fn present<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(d).map(Some)
}

impl Message {
    pub fn request(id: i64, method: &str, params: impl Serialize) -> Self {
        Self::Request {
            id: Id::Number(id),
            method: method.into(),
            params: serde_json::to_value(params).unwrap_or(Value::Null),
        }
    }

    pub fn notification(method: &str, params: impl Serialize) -> Self {
        Self::Notification {
            method: method.into(),
            params: serde_json::to_value(params).unwrap_or(Value::Null),
        }
    }

    pub fn result(id: Id, result: impl Serialize) -> Self {
        Self::Response {
            id,
            result: Ok(serde_json::to_value(result).unwrap_or(Value::Null)),
        }
    }

    pub fn error(id: Id, error: RpcError) -> Self {
        Self::Response {
            id,
            result: Err(error),
        }
    }

    /// Parses one line. `Err` for anything that isn't a JSON-RPC 2.0 message.
    pub fn parse(line: &[u8]) -> Result<Self, String> {
        let wire: Wire = serde_json::from_slice(line).map_err(|e| e.to_string())?;
        if wire.jsonrpc != "2.0" {
            return Err(format!("unsupported jsonrpc version `{}`", wire.jsonrpc));
        }
        let params = wire.params.unwrap_or(Value::Null);
        match (wire.id, wire.method, wire.result, wire.error) {
            (Some(id), Some(method), None, None) => Ok(Self::Request { id, method, params }),
            (None, Some(method), None, None) => Ok(Self::Notification { method, params }),
            (Some(id), None, Some(result), None) => Ok(Self::Response {
                id,
                result: Ok(result),
            }),
            (Some(id), None, None, Some(error)) => Ok(Self::Response {
                id,
                result: Err(error),
            }),
            _ => Err("not a request, notification or response".into()),
        }
    }

    /// The message as one line, ending in `\n`.
    pub fn to_line(&self) -> Vec<u8> {
        let mut wire = Wire {
            jsonrpc: "2.0".into(),
            id: None,
            method: None,
            params: None,
            result: None,
            error: None,
        };
        match self.clone() {
            Self::Request { id, method, params } => {
                wire.id = Some(id);
                wire.method = Some(method);
                wire.params = Some(params);
            }
            Self::Notification { method, params } => {
                wire.method = Some(method);
                wire.params = Some(params);
            }
            Self::Response { id, result } => {
                wire.id = Some(id);
                match result {
                    Ok(v) => wire.result = Some(v),
                    Err(e) => wire.error = Some(e),
                }
            }
        }
        let mut line = serde_json::to_vec(&wire).unwrap_or_default();
        line.push(b'\n');
        line
    }
}

// ---------- initialize ----------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InitializeParams {
    pub protocol: u32,
    /// ch's version.
    pub ch_version: String,
    /// The plugin's name from its `plugin.toml`.
    pub plugin: String,
    /// The workspace root (also in `CH_WORKSPACE`).
    pub workspace: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct InitializeResult {
    pub protocol: u32,
    pub tools: Vec<ToolDef>,
    pub hooks: Vec<HookDecl>,
    pub prompt: PromptCaps,
    /// Event type names to receive as `event/notify` (`tool_call_end`, `turn_end`, ...,
    /// the `type` of the kernel's `AgentEvent`), or `"*"` for all.
    pub events: Vec<String>,
}

/// A tool. ch names it `ext__<plugin>__<name>`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// JSON Schema for the input; must be `{"type": "object", ...}`.
    pub input_schema: Value,
}

/// A hook point the plugin wants, such as `pre_tool`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HookDecl {
    /// `session_start`, `user_prompt`, `before_model`, `after_model`, `pre_tool`,
    /// `post_tool`, `turn_end` or `session_end`.
    pub point: String,
    /// For `pre_tool` and `post_tool`: tool name globs (`bash`, `edit_*`). Empty means
    /// every tool.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PromptCaps {
    /// ch asks for system prompt blocks with `prompt/system`, once per conversation.
    pub system: bool,
    /// ch asks for reminders with `prompt/turn` at each model call.
    pub turn: bool,
}

// ---------- calls ----------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallParams {
    /// The plugin's own name for the tool (without `ext__<plugin>__`).
    pub name: String,
    pub input: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolCallResult {
    /// Text for the model.
    pub content: String,
    #[serde(default)]
    pub is_error: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HookInvokeParams {
    pub point: String,
    /// The kernel's hook event, as JSON (`{"point": "pre_tool", "call": {...}, ...}`).
    pub event: Value,
}

/// What a hook decides; the same shape as the kernel's `HookDecision`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum HookDecision {
    Continue,
    Deny {
        reason: String,
    },
    Ask {
        reason: String,
        #[serde(default)]
        key: Option<String>,
    },
    Rewrite {
        input: Value,
    },
    AppendNote {
        text: String,
    },
    Stop {
        reason: String,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PromptSystemResult {
    pub blocks: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromptTurnParams {
    pub turn: u32,
    pub step: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PromptTurnResult {
    pub reminders: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommandRunParams {
    pub name: String,
    pub args: String,
}

/// Show `message` to the user, or send `prompt` to the model as the next user turn.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CommandRunResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
}

// ---------- notifications ----------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventNotifyParams {
    /// The kernel's event, as JSON (`{"type": "tool_call_end", ...}`).
    pub event: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CancelParams {
    pub id: Id,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogParams {
    /// `error`, `warn`, `info` or `debug`.
    #[serde(default = "info")]
    pub level: String,
    pub message: String,
}

fn info() -> String {
    "info".into()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NotifyParams {
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolsChangedParams {
    pub tools: Vec<ToolDef>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn messages_round_trip() {
        for m in [
            Message::request(1, method::TOOL_CALL, json!({"name": "x", "input": {}})),
            Message::notification(method::LOG, json!({"message": "hi"})),
            Message::result(Id::Number(1), json!({"content": "ok"})),
            Message::result(Id::String("a".into()), Value::Null),
            Message::error(Id::Number(2), RpcError::new(code::METHOD_NOT_FOUND, "no")),
        ] {
            let line = m.to_line();
            assert_eq!(*line.last().unwrap(), b'\n');
            assert_eq!(line.iter().filter(|b| **b == b'\n').count(), 1);
            assert_eq!(Message::parse(&line[..line.len() - 1]).unwrap(), m, "{m:?}");
        }
    }

    #[test]
    fn non_messages_are_refused() {
        for bad in [
            &b"hello"[..],
            br#"{"jsonrpc":"1.0","id":1,"method":"x"}"#,
            br#"{"jsonrpc":"2.0","id":1}"#,
            br#"{"jsonrpc":"2.0","id":1,"result":1,"error":{"code":1,"message":"x"}}"#,
        ] {
            assert!(
                Message::parse(bad).is_err(),
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    #[test]
    fn hook_decisions_match_the_kernel_shape() {
        let d: HookDecision =
            serde_json::from_value(json!({"decision": "deny", "reason": "r"})).unwrap();
        assert_eq!(d, HookDecision::Deny { reason: "r".into() });
        assert_eq!(
            serde_json::to_value(HookDecision::AppendNote { text: "t".into() }).unwrap(),
            json!({"decision": "append_note", "text": "t"})
        );
    }
}
