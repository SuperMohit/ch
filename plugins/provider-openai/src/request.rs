//! Kernel request → chat-completions request body.

use ch_kernel::{ContentBlock, Message, ModelRequest, Role, ToolSpec};
use serde_json::{Map, Value, json};

/// Provider-level options that shape the request body.
#[derive(Debug, Clone, PartialEq)]
pub struct BodyOptions {
    /// Name of the output-cap field: `max_completion_tokens` (OpenAI) or `max_tokens`
    /// (most OpenAI-compatible servers).
    pub max_tokens_field: String,
    /// `reasoning_effort` for reasoning models; empty to omit.
    pub reasoning_effort: String,
}

impl Default for BodyOptions {
    fn default() -> Self {
        Self {
            max_tokens_field: "max_tokens".into(),
            reasoning_effort: String::new(),
        }
    }
}

/// Builds the JSON body for `POST /chat/completions` with streaming and usage reporting.
pub fn build_body(req: &ModelRequest, opts: &BodyOptions) -> Value {
    let mut body = Map::new();
    body.insert("model".into(), json!(req.model));
    body.insert("messages".into(), Value::Array(messages(req)));
    body.insert("stream".into(), json!(true));
    body.insert("stream_options".into(), json!({ "include_usage": true }));
    body.insert(opts.max_tokens_field.clone(), json!(req.max_tokens));
    if let Some(t) = req.temperature {
        body.insert("temperature".into(), json!(t));
    }
    if !opts.reasoning_effort.is_empty() {
        body.insert("reasoning_effort".into(), json!(opts.reasoning_effort));
    }
    if !req.tools.is_empty() {
        body.insert(
            "tools".into(),
            Value::Array(req.tools.iter().map(tool).collect()),
        );
    }
    Value::Object(body)
}

fn tool(spec: &ToolSpec) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": spec.name,
            "description": spec.description,
            "parameters": spec.input_schema,
        }
    })
}

fn messages(req: &ModelRequest) -> Vec<Value> {
    let mut out = Vec::new();
    let system: Vec<&str> = req
        .system
        .iter()
        .map(String::as_str)
        .filter(|s| !s.trim().is_empty())
        .collect();
    if !system.is_empty() {
        out.push(json!({ "role": "system", "content": system.join("\n\n") }));
    }
    for msg in &req.messages {
        match msg.role {
            Role::User => user(msg, &mut out),
            Role::Assistant => out.push(assistant(msg)),
        }
    }
    out
}

/// Tool results become `role: "tool"` messages (they must directly follow the assistant
/// message that called them); any text goes in a user message after them.
fn user(msg: &Message, out: &mut Vec<Value>) {
    let mut text = Vec::new();
    for block in &msg.content {
        match block {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                let content = if *is_error {
                    format!("Error: {content}")
                } else {
                    content.clone()
                };
                out.push(json!({
                    "role": "tool",
                    "tool_call_id": tool_use_id,
                    "content": content,
                }));
            }
            ContentBlock::Text { text: t } if !t.is_empty() => text.push(t.as_str()),
            _ => {}
        }
    }
    if !text.is_empty() {
        out.push(json!({ "role": "user", "content": text.join("\n\n") }));
    }
}

/// Text blocks are joined; tool_use blocks become `tool_calls`; thinking is dropped.
fn assistant(msg: &Message) -> Value {
    let text: String = msg
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");
    let calls: Vec<Value> = msg
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolUse { id, name, input } => Some(json!({
                "id": id,
                "type": "function",
                "function": { "name": name, "arguments": input.to_string() },
            })),
            _ => None,
        })
        .collect();
    let mut m = json!({ "role": "assistant" });
    if calls.is_empty() {
        m["content"] = json!(text);
    } else {
        m["content"] = if text.is_empty() {
            Value::Null
        } else {
            json!(text)
        };
        m["tool_calls"] = Value::Array(calls);
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_kernel_messages() {
        let req = ModelRequest {
            model: "gpt-test".into(),
            system: vec!["one".into(), " ".into(), "two".into()],
            messages: vec![
                Message::user("hi"),
                Message {
                    role: Role::Assistant,
                    content: vec![
                        ContentBlock::Thinking {
                            thinking: "secret".into(),
                            signature: None,
                        },
                        ContentBlock::text("Checking."),
                        ContentBlock::ToolUse {
                            id: "call_1".into(),
                            name: "read_file".into(),
                            input: json!({"path": "a.rs"}),
                        },
                        ContentBlock::ToolUse {
                            id: "call_2".into(),
                            name: "list_files".into(),
                            input: json!({}),
                        },
                    ],
                },
                Message {
                    role: Role::User,
                    content: vec![
                        ContentBlock::ToolResult {
                            tool_use_id: "call_1".into(),
                            content: "fn main() {}".into(),
                            is_error: false,
                        },
                        ContentBlock::ToolResult {
                            tool_use_id: "call_2".into(),
                            content: "not a directory".into(),
                            is_error: true,
                        },
                        ContentBlock::text("<system-reminder>x</system-reminder>"),
                    ],
                },
            ],
            tools: vec![ToolSpec {
                name: "read_file".into(),
                description: "Reads".into(),
                input_schema: json!({"type": "object"}),
                read_only: true,
            }],
            max_tokens: 100,
            temperature: Some(0.5),
            thinking_budget: Some(1000),
            purpose: String::new(),
            volatile_tail: 0,
        };
        let body = build_body(
            &req,
            &BodyOptions {
                max_tokens_field: "max_completion_tokens".into(),
                reasoning_effort: "low".into(),
            },
        );
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[0], json!({"role": "system", "content": "one\n\ntwo"}));
        assert_eq!(msgs[1], json!({"role": "user", "content": "hi"}));
        assert_eq!(msgs[2]["content"], "Checking.");
        assert_eq!(msgs[2]["tool_calls"][0]["function"]["name"], "read_file");
        assert_eq!(
            msgs[2]["tool_calls"][0]["function"]["arguments"],
            "{\"path\":\"a.rs\"}"
        );
        assert!(!msgs[2].to_string().contains("secret"));
        assert_eq!(msgs[3]["role"], "tool");
        assert_eq!(msgs[3]["tool_call_id"], "call_1");
        assert_eq!(msgs[4]["content"], "Error: not a directory");
        assert_eq!(msgs[5]["role"], "user");
        assert_eq!(msgs.len(), 6);

        assert_eq!(body["max_completion_tokens"], 100);
        assert!(body.get("max_tokens").is_none());
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert_eq!(body["reasoning_effort"], "low");
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn assistant_tool_calls_without_text_have_null_content() {
        let m = assistant(&Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "c".into(),
                name: "n".into(),
                input: json!({}),
            }],
        });
        assert!(m["content"].is_null());
        assert_eq!(assistant(&Message::assistant("done"))["content"], "done");
    }
}
