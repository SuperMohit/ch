//! Kernel request → Messages API request body.

use ch_kernel::{ContentBlock, Message, ModelRequest, Role, ToolSpec};
use serde_json::{Map, Value, json};

use crate::models::{ModelCaps, ThinkingStyle, caps};

/// Minimum `budget_tokens` the API accepts for budget-style thinking.
const MIN_THINKING_BUDGET: u32 = 1024;

/// Provider-level options that shape the request body.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BodyOptions {
    /// Set `eager_input_streaming: true` on every tool.
    pub eager_input_streaming: bool,
    /// `thinking.display` for models that accept it; empty means "don't send".
    pub thinking_display: String,
    /// `output_config.effort` for adaptive-thinking models; empty means "don't send".
    pub effort: String,
    /// `fallbacks` value for models that support server-side refusal fallback; empty
    /// means off.
    pub fallbacks: String,
}

impl BodyOptions {
    /// Whether the request for `model` uses the refusal-fallback beta.
    pub fn uses_fallbacks(&self, model: &str) -> bool {
        !self.fallbacks.is_empty() && caps(model).refusal_fallback
    }
}

/// Builds the JSON body for `POST /v1/messages` with `stream: true`.
pub fn build_body(req: &ModelRequest, opts: &BodyOptions) -> Value {
    let caps = caps(&req.model);
    let mut body = Map::new();
    body.insert("model".into(), json!(req.model));
    body.insert("max_tokens".into(), json!(req.max_tokens));
    body.insert("stream".into(), json!(true));

    let system = system_blocks(&req.system);
    if !system.is_empty() {
        body.insert("system".into(), Value::Array(system));
    }
    body.insert(
        "messages".into(),
        Value::Array(messages(&req.messages, req.volatile_tail)),
    );
    if !req.tools.is_empty() {
        let tools = req
            .tools
            .iter()
            .map(|t| tool(t, opts.eager_input_streaming))
            .collect();
        body.insert("tools".into(), Value::Array(tools));
    }

    let thinking = thinking(req, &caps, opts);
    let thinking_on = thinking.is_some() || caps.always_thinks;
    if let Some(t) = thinking {
        body.insert("thinking".into(), t);
    }
    if let Some(temp) = req.temperature
        && caps.sampling
        && !thinking_on
    {
        body.insert("temperature".into(), json!(temp));
    }
    if caps.thinking == ThinkingStyle::Adaptive && !opts.effort.is_empty() {
        body.insert("output_config".into(), json!({ "effort": opts.effort }));
    }
    if opts.uses_fallbacks(&req.model) {
        body.insert("fallbacks".into(), json!(opts.fallbacks));
    }
    Value::Object(body)
}

fn thinking(req: &ModelRequest, caps: &ModelCaps, opts: &BodyOptions) -> Option<Value> {
    let adaptive = || {
        let mut t = json!({ "type": "adaptive" });
        if caps.thinking_display && !opts.thinking_display.is_empty() {
            t["display"] = json!(opts.thinking_display);
        }
        t
    };
    match (req.thinking_budget, caps.thinking) {
        (Some(_), ThinkingStyle::Adaptive) => Some(adaptive()),
        (Some(budget), ThinkingStyle::Budget) => {
            // budget_tokens must be >= 1024 and < max_tokens.
            if req.max_tokens <= MIN_THINKING_BUDGET {
                return None;
            }
            let budget = budget.clamp(MIN_THINKING_BUDGET, req.max_tokens - 1);
            Some(json!({ "type": "enabled", "budget_tokens": budget }))
        }
        // Models that always think: send the adaptive config only to pick the display.
        (None, _) if caps.always_thinks && !opts.thinking_display.is_empty() => Some(adaptive()),
        (None, _) => None,
    }
}

fn ephemeral() -> Value {
    json!({ "type": "ephemeral" })
}

fn system_blocks(system: &[String]) -> Vec<Value> {
    let mut blocks: Vec<Value> = system
        .iter()
        .filter(|s| !s.trim().is_empty())
        .map(|s| json!({ "type": "text", "text": s }))
        .collect();
    if let Some(last) = blocks.last_mut() {
        last["cache_control"] = ephemeral();
    }
    blocks
}

fn tool(spec: &ToolSpec, eager: bool) -> Value {
    let mut t = json!({
        "name": spec.name,
        "description": spec.description,
        "input_schema": spec.input_schema,
    });
    if eager {
        t["eager_input_streaming"] = json!(true);
    }
    t
}

fn messages(history: &[Message], volatile_tail: usize) -> Vec<Value> {
    let mut out: Vec<Value> = history.iter().map(message).collect();
    // Cache breakpoint on the last block that the next request will also contain: skip
    // the `volatile_tail` per-call blocks (reminders) at the end, which are never empty
    // and so are never dropped by `message`, and blocks that can't carry one (thinking).
    let mut skip = volatile_tail;
    for msg in out.iter_mut().rev() {
        let Some(blocks) = msg.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        if skip >= blocks.len() {
            skip -= blocks.len();
            continue;
        }
        let keep = blocks.len() - skip;
        if let Some(block) = blocks[..keep].iter_mut().rev().find(|b| {
            !matches!(
                b["type"].as_str(),
                Some("thinking") | Some("redacted_thinking")
            )
        }) {
            block["cache_control"] = ephemeral();
        }
        break;
    }
    out
}

fn message(msg: &Message) -> Value {
    let role = match msg.role {
        Role::User => "user",
        Role::Assistant => "assistant",
    };
    let mut content: Vec<Value> = msg.content.iter().filter_map(block).collect();
    if content.is_empty() {
        // The API rejects empty content; keep the turn so roles still alternate.
        content.push(json!({ "type": "text", "text": "(empty)" }));
    }
    json!({ "role": role, "content": content })
}

fn block(b: &ContentBlock) -> Option<Value> {
    Some(match b {
        // Empty text blocks are rejected by the API.
        ContentBlock::Text { text } if text.is_empty() => return None,
        ContentBlock::Text { text } => json!({ "type": "text", "text": text }),
        ContentBlock::Thinking {
            thinking,
            signature,
        } => {
            // Unsigned thinking (from another provider) can't be replayed.
            let sig = signature.as_deref()?;
            json!({ "type": "thinking", "thinking": thinking, "signature": sig })
        }
        ContentBlock::RedactedThinking { data } => {
            json!({ "type": "redacted_thinking", "data": data })
        }
        ContentBlock::ToolUse { id, name, input } => {
            let input = if input.is_object() {
                input.clone()
            } else {
                json!({})
            };
            json!({ "type": "tool_use", "id": id, "name": name, "input": input })
        }
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => {
            let mut v = json!({
                "type": "tool_result",
                "tool_use_id": tool_use_id,
                "content": content,
            });
            if *is_error {
                v["is_error"] = json!(true);
            }
            v
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(model: &str) -> ModelRequest {
        ModelRequest {
            model: model.into(),
            system: vec!["base".into(), "".into(), "project".into()],
            messages: vec![
                Message::user("hi"),
                Message {
                    role: Role::Assistant,
                    content: vec![
                        ContentBlock::Thinking {
                            thinking: "hmm".into(),
                            signature: Some("sig1".into()),
                        },
                        ContentBlock::RedactedThinking {
                            data: "opaque".into(),
                        },
                        ContentBlock::Thinking {
                            thinking: "unsigned".into(),
                            signature: None,
                        },
                        ContentBlock::text(""),
                        ContentBlock::ToolUse {
                            id: "toolu_1".into(),
                            name: "read_file".into(),
                            input: json!({"path": "a"}),
                        },
                    ],
                },
                Message {
                    role: Role::User,
                    content: vec![ContentBlock::ToolResult {
                        tool_use_id: "toolu_1".into(),
                        content: "boom".into(),
                        is_error: true,
                    }],
                },
            ],
            tools: vec![ToolSpec {
                name: "read_file".into(),
                description: "Reads".into(),
                input_schema: json!({"type": "object"}),
                read_only: true,
            }],
            max_tokens: 4096,
            temperature: Some(0.2),
            thinking_budget: None,
            purpose: "agent".into(),
            volatile_tail: 0,
        }
    }

    fn opts() -> BodyOptions {
        BodyOptions {
            eager_input_streaming: true,
            thinking_display: "summarized".into(),
            effort: "high".into(),
            fallbacks: "default".into(),
        }
    }

    #[test]
    fn maps_messages_system_tools_and_cache_breakpoints() {
        let body = build_body(&req("claude-sonnet-4-6"), &opts());
        let system = body["system"].as_array().unwrap();
        assert_eq!(system.len(), 2);
        assert!(system[0].get("cache_control").is_none());
        assert_eq!(system[1]["cache_control"]["type"], "ephemeral");

        let msgs = body["messages"].as_array().unwrap();
        let assistant = msgs[1]["content"].as_array().unwrap();
        assert_eq!(assistant.len(), 3, "{assistant:?}");
        assert_eq!(assistant[0]["type"], "thinking");
        assert_eq!(assistant[0]["signature"], "sig1");
        assert_eq!(assistant[1]["type"], "redacted_thinking");
        assert_eq!(assistant[1]["data"], "opaque");
        assert_eq!(assistant[2]["type"], "tool_use");
        assert!(assistant[2].get("cache_control").is_none());

        let last = &msgs[2]["content"][0];
        assert_eq!(last["type"], "tool_result");
        assert_eq!(last["is_error"], true);
        assert_eq!(last["cache_control"]["type"], "ephemeral");

        assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
        assert_eq!(body["tools"][0]["eager_input_streaming"], true);
        assert_eq!(body["stream"], true);
        // Sonnet 4.6 accepts temperature when thinking is off.
        assert!((body["temperature"].as_f64().unwrap() - 0.2).abs() < 1e-6);
        assert!(body.get("thinking").is_none());
        assert_eq!(body["output_config"]["effort"], "high");
        assert!(body.get("fallbacks").is_none());
    }

    #[test]
    fn a_stored_malformed_call_is_sent_back_unchanged() {
        let raw = json!({"_invalid_json": "{\"pa"});
        let call = ContentBlock::ToolUse {
            id: "toolu_1".into(),
            name: "read_file".into(),
            input: raw.clone(),
        };
        assert_eq!(block(&call).unwrap()["input"], raw);
    }

    #[test]
    fn cache_breakpoint_skips_per_call_reminders() {
        let mut r = req("claude-opus-5-5");
        r.messages = vec![
            Message::user("task"),
            Message::assistant("working"),
            Message {
                role: Role::User,
                content: vec![
                    ContentBlock::text("result"),
                    ContentBlock::text("<system-reminder>a</system-reminder>"),
                    ContentBlock::text("<system-reminder>b</system-reminder>"),
                ],
            },
        ];
        let marked = |r: &ModelRequest| -> Vec<String> {
            let body = build_body(r, &BodyOptions::default());
            body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|m| m["content"].as_array().unwrap().clone())
                .filter(|b| b.get("cache_control").is_some())
                .map(|b| b["text"].as_str().unwrap().to_string())
                .collect()
        };
        r.volatile_tail = 0;
        assert_eq!(marked(&r), ["<system-reminder>b</system-reminder>"]);
        r.volatile_tail = 2;
        assert_eq!(marked(&r), ["result"]);
        // A message made only of reminders: the breakpoint moves to the one before.
        r.volatile_tail = 3;
        assert_eq!(marked(&r), ["working"]);
        r.volatile_tail = 99;
        assert!(marked(&r).is_empty());
    }

    #[test]
    fn modern_models_drop_sampling_and_use_adaptive_thinking() {
        let mut r = req("claude-opus-5-5");
        r.thinking_budget = Some(8000);
        let body = build_body(&r, &opts());
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["thinking"]["display"], "summarized");
        assert!(body["thinking"].get("budget_tokens").is_none());
        assert!(body.get("temperature").is_none());
        assert_eq!(body["fallbacks"], "default");

        // Always-thinking model without a budget still gets the display setting.
        r.thinking_budget = None;
        let body = build_body(&r, &opts());
        assert_eq!(body["thinking"]["type"], "adaptive");

        // Opus 4.8 does not think unless asked.
        let body = build_body(&req("claude-opus-4-8"), &opts());
        assert!(body.get("thinking").is_none());
        assert!(body.get("temperature").is_none());
    }

    #[test]
    fn legacy_models_use_budget_tokens() {
        let mut r = req("claude-haiku-4-5");
        r.thinking_budget = Some(500);
        let body = build_body(&r, &opts());
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 1024);
        assert!(body.get("temperature").is_none());
        assert!(body.get("output_config").is_none());

        r.thinking_budget = Some(100_000);
        let body = build_body(&r, &opts());
        assert_eq!(body["thinking"]["budget_tokens"], 4095);

        r.thinking_budget = None;
        let body = build_body(&r, &opts());
        assert!(body.get("thinking").is_none());
        assert!(body.get("temperature").is_some());
    }

    #[test]
    fn empty_messages_keep_a_placeholder() {
        let mut r = req("claude-opus-5-5");
        r.messages = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::text("")],
        }];
        let body = build_body(&r, &BodyOptions::default());
        assert_eq!(body["messages"][0]["content"][0]["text"], "(empty)");
        assert!(body.get("thinking").is_none());
        assert!(body.get("output_config").is_none());
        assert!(body["tools"][0].get("eager_input_streaming").is_none());
    }
}
