//! Kernel request → Messages API request body.

use ch_kernel::util::system_reminder;
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
    /// `thinking.block_binding.prefix_mismatch_behavior`; empty means "don't send".
    pub prefix_mismatch_behavior: String,
}

impl BodyOptions {
    /// Whether the request for `model` uses the refusal-fallback beta.
    pub fn uses_fallbacks(&self, model: &str) -> bool {
        !self.fallbacks.is_empty() && caps(model).refusal_fallback
    }

    /// The betas `body` needs.
    pub fn betas(&self, body: &Value) -> Vec<&'static str> {
        let mut betas = Vec::new();
        if body.get("fallbacks").is_some() {
            betas.push(crate::FALLBACK_BETA);
        }
        if body.pointer("/thinking/block_binding").is_some() {
            betas.push(crate::BINDING_BETA);
        }
        if changes_tools(body) {
            betas.push(crate::TOOL_CHANGES_BETA);
        }
        betas
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
    let native = req.tool_ledger.is_some();
    body.insert(
        "messages".into(),
        Value::Array(messages(&req.messages, native)),
    );
    let tools: Vec<Value> = match &req.tool_ledger {
        // Every definition sent so far, in order: changing or dropping one would
        // invalidate the model's earlier reasoning. Tools added later are deferred and
        // announced in the messages.
        Some(ledger) => (ledger.iter())
            .map(|e| {
                let mut t = tool(&e.spec, opts.eager_input_streaming);
                if e.deferred {
                    t["defer_loading"] = json!(true);
                }
                t
            })
            .collect(),
        None => (req.tools.iter())
            .map(|t| tool(t, opts.eager_input_streaming))
            .collect(),
    };
    if !tools.is_empty() {
        body.insert("tools".into(), Value::Array(tools));
    }

    let mut thinking = thinking(req, &caps, opts);
    // The binding control goes on the thinking config; a model that always thinks gets
    // its default one to carry it.
    if thinking.is_none() && caps.always_thinks && !opts.prefix_mismatch_behavior.is_empty() {
        thinking = Some(json!({ "type": "adaptive" }));
    }
    let thinking_on = thinking.is_some() || caps.always_thinks;
    if let Some(mut t) = thinking {
        if !opts.prefix_mismatch_behavior.is_empty() {
            t["block_binding"] =
                json!({ "prefix_mismatch_behavior": opts.prefix_mismatch_behavior });
        }
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
    let body = Value::Object(body);
    // Claude Sonnet 5.5's server-side fallback answers some refusals with Claude Sonnet
    // 5, which takes neither tool changes nor system messages.
    let fallback_ok =
        !(changes_tools(&body) && crate::models::fallback_lacks_tool_changes(&req.model));
    let mut body = body;
    if opts.uses_fallbacks(&req.model) && fallback_ok {
        body["fallbacks"] = json!(opts.fallbacks);
    }
    body
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

/// Whether `body` adds or withdraws tools mid-conversation.
fn changes_tools(body: &Value) -> bool {
    let deferred =
        (body["tools"].as_array().into_iter().flatten()).any(|t| t["defer_loading"] == json!(true));
    let announced =
        (body["messages"].as_array().into_iter().flatten()).any(|m| m["role"] == "system");
    deferred || announced
}

/// The conversation. With `native` tool changes, the `ToolsChanged` blocks of each run of
/// consecutive user messages become one system message after the run, netted, so it
/// always sits just before an assistant message or last, as the API requires, even when
/// a call failed after a change was stored and another user message followed.
fn messages(history: &[Message], native: bool) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    let mut changes: Vec<&ContentBlock> = Vec::new();
    for msg in history {
        if msg.role == Role::Assistant {
            flush_changes(&mut out, &mut changes);
        }
        out.push(message(msg, native));
        if native && msg.role == Role::User {
            changes.extend(
                (msg.content.iter()).filter(|b| matches!(b, ContentBlock::ToolsChanged { .. })),
            );
        }
    }
    // Cache breakpoint on the last block that can carry one (not thinking), and never on
    // a tool-change message. The kernel only ever appends to the conversation, so the
    // next request starts with all of it.
    if let Some(block) = out
        .last_mut()
        .and_then(|msg| msg.get_mut("content"))
        .and_then(Value::as_array_mut)
        .and_then(|blocks| {
            blocks.iter_mut().rev().find(|b| {
                !matches!(
                    b["type"].as_str(),
                    Some("thinking") | Some("redacted_thinking")
                )
            })
        })
    {
        block["cache_control"] = ephemeral();
    }
    flush_changes(&mut out, &mut changes);
    out
}

/// Appends one system message announcing the net effect of `changes`, if any.
fn flush_changes(out: &mut Vec<Value>, changes: &mut Vec<&ContentBlock>) {
    if let Some(ContentBlock::ToolsChanged { added, removed }) =
        ch_kernel::ledger::net(changes.drain(..))
    {
        let reference = |name: &String| json!({ "type": "tool_reference", "name": name });
        let content: Vec<Value> = (added.iter())
            .map(|n| json!({ "type": "tool_addition", "tool": reference(n) }))
            .chain(
                (removed.iter()).map(|n| json!({ "type": "tool_removal", "tool": reference(n) })),
            )
            .collect();
        out.push(json!({ "role": "system", "content": content }));
    }
}

fn message(msg: &Message, native: bool) -> Value {
    let role = match msg.role {
        Role::User => "user",
        Role::Assistant => "assistant",
    };
    let mut content: Vec<Value> = (msg.content.iter())
        .filter_map(|b| match b {
            // Rendered as a system message after the run of user messages.
            ContentBlock::ToolsChanged { .. } if native => None,
            ContentBlock::ToolsChanged { added, removed } => Some(json!({
                "type": "text",
                "text": system_reminder(&ch_kernel::ledger::note(added, removed)),
            })),
            b => block(b),
        })
        .collect();
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
        // Handled by `message`.
        ContentBlock::ToolsChanged { .. } => return None,
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
            tool_ledger: None,
        }
    }

    fn opts() -> BodyOptions {
        BodyOptions {
            eager_input_streaming: true,
            thinking_display: "summarized".into(),
            effort: "high".into(),
            fallbacks: "default".into(),
            prefix_mismatch_behavior: String::new(),
        }
    }

    #[test]
    fn block_binding_goes_on_the_thinking_config_with_its_beta() {
        let binding = |model: &str, o: &BodyOptions| {
            let mut r = req(model);
            r.thinking_budget = None;
            let body = build_body(&r, o);
            let sent = body["thinking"]["block_binding"]["prefix_mismatch_behavior"].clone();
            (sent, o.betas(&body).contains(&crate::BINDING_BETA))
        };
        let mut o = opts();
        assert_eq!(binding("claude-opus-5-5", &o), (Value::Null, false));
        o.prefix_mismatch_behavior = "error".into();
        assert_eq!(binding("claude-opus-5-5", &o), (json!("error"), true));
        // With no display setting the model's default thinking config carries it.
        o.thinking_display = String::new();
        assert_eq!(binding("claude-opus-5-5", &o), (json!("error"), true));
        // A model that isn't thinking has nothing to bind: no field and no beta.
        assert_eq!(binding("claude-sonnet-4-5", &o), (Value::Null, false));
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
    fn cache_breakpoint_is_on_the_last_block() {
        let mut r = req("claude-opus-5-5");
        r.messages = vec![
            Message::user("task"),
            Message::assistant("working"),
            Message {
                role: Role::User,
                content: vec![
                    ContentBlock::text("result"),
                    ContentBlock::text("<system-reminder>a</system-reminder>"),
                ],
            },
        ];
        let body = build_body(&r, &BodyOptions::default());
        let marked: Vec<&str> = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|m| m["content"].as_array().unwrap())
            .filter(|b| b.get("cache_control").is_some())
            .map(|b| b["text"].as_str().unwrap())
            .collect();
        // Reminders stay in the history, so the next request repeats them: the
        // breakpoint can cover them.
        assert_eq!(marked, ["<system-reminder>a</system-reminder>"]);
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

    fn changed(added: &[&str], removed: &[&str]) -> ContentBlock {
        ContentBlock::ToolsChanged {
            added: added.iter().map(|s| s.to_string()).collect(),
            removed: removed.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn entry(name: &str, deferred: bool, active: bool) -> ch_kernel::LedgerEntry {
        ch_kernel::LedgerEntry {
            spec: ToolSpec {
                name: name.into(),
                description: "d".into(),
                input_schema: json!({"type": "object"}),
                read_only: false,
            },
            source: None,
            deferred,
            active,
        }
    }

    fn user(blocks: Vec<ContentBlock>) -> Message {
        Message {
            role: Role::User,
            content: blocks,
        }
    }

    /// A conversation where a tool was added and another withdrawn, sent natively.
    fn native() -> ModelRequest {
        let mut r = req("claude-opus-5-5");
        r.tool_ledger = Some(vec![
            entry("read_file", false, true),
            entry("mcp__gh__old", false, false),
            entry("mcp__gh__new", true, true),
        ]);
        r.messages = vec![
            Message::user("task"),
            Message::assistant("working"),
            user(vec![
                ContentBlock::text("result"),
                changed(&["mcp__gh__new"], &["mcp__gh__old"]),
                ContentBlock::text("<system-reminder>r</system-reminder>"),
            ]),
        ];
        r
    }

    #[test]
    fn native_tool_changes_defer_new_tools_and_announce_them_after_the_user_turn() {
        let o = BodyOptions::default();
        let body = build_body(&native(), &o);
        let tools = body["tools"].as_array().unwrap();
        let shape: Vec<(&str, bool)> = (tools.iter())
            .map(|t| {
                (
                    t["name"].as_str().unwrap(),
                    t["defer_loading"] == json!(true),
                )
            })
            .collect();
        assert_eq!(
            shape,
            [
                ("read_file", false),
                ("mcp__gh__old", false),
                ("mcp__gh__new", true)
            ]
        );
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[2]["role"], "user");
        let user_blocks = msgs[2]["content"].as_array().unwrap();
        assert_eq!(
            user_blocks.len(),
            2,
            "the change isn't a user block: {user_blocks:?}"
        );
        assert_eq!(
            msgs[3],
            json!({"role": "system", "content": [
                {"type": "tool_addition", "tool": {"type": "tool_reference", "name": "mcp__gh__new"}},
                {"type": "tool_removal", "tool": {"type": "tool_reference", "name": "mcp__gh__old"}},
            ]})
        );
        // The breakpoint stays on the user message: the system message is the tail.
        assert_eq!(user_blocks[1]["cache_control"]["type"], "ephemeral");
        assert!(msgs[3]["content"][0].get("cache_control").is_none());
        assert!(o.betas(&body).contains(&crate::TOOL_CHANGES_BETA));
    }

    #[test]
    fn changes_from_a_failed_call_move_to_the_end_of_the_user_turns_netted() {
        let mut r = native();
        // The call carrying the change failed, and the next turn's prompt followed.
        r.messages.push(user(vec![
            ContentBlock::text("next prompt"),
            changed(&["mcp__gh__old"], &["mcp__gh__other"]),
        ]));
        let body = build_body(&r, &BodyOptions::default());
        let msgs = body["messages"].as_array().unwrap();
        let roles: Vec<&str> = msgs.iter().map(|m| m["role"].as_str().unwrap()).collect();
        assert_eq!(roles, ["user", "assistant", "user", "user", "system"]);
        // `old` was withdrawn then added back: no change. The rest is kept.
        let names: Vec<(&str, &str)> = (msgs[4]["content"].as_array().unwrap().iter())
            .map(|b| {
                (
                    b["type"].as_str().unwrap(),
                    b["tool"]["name"].as_str().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            names,
            [
                ("tool_addition", "mcp__gh__new"),
                ("tool_removal", "mcp__gh__other")
            ]
        );

        // An addition withdrawn again before any reply leaves nothing to announce.
        let mut r = native();
        r.messages[2].content.push(changed(&[], &["mcp__gh__new"]));
        r.messages[2].content.push(changed(&["mcp__gh__old"], &[]));
        let body = build_body(&r, &BodyOptions::default());
        let roles: Vec<&str> = (body["messages"].as_array().unwrap().iter())
            .map(|m| m["role"].as_str().unwrap())
            .collect();
        assert_eq!(roles, ["user", "assistant", "user"]);
    }

    #[test]
    fn a_change_is_announced_where_it_happened_once_a_reply_follows() {
        let mut r = native();
        r.messages.push(Message::assistant("using the new tool"));
        r.messages.push(user(vec![ContentBlock::text("thanks")]));
        let body = build_body(&r, &BodyOptions::default());
        let roles: Vec<&str> = (body["messages"].as_array().unwrap().iter())
            .map(|m| m["role"].as_str().unwrap())
            .collect();
        assert_eq!(
            roles,
            ["user", "assistant", "user", "system", "assistant", "user"]
        );
    }

    #[test]
    fn without_a_ledger_a_change_is_a_reminder_and_the_tools_are_the_current_ones() {
        let mut r = native();
        r.tool_ledger = None;
        let o = BodyOptions::default();
        let body = build_body(&r, &o);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        let text = msgs[2]["content"][1]["text"].as_str().unwrap();
        assert!(text.starts_with("<system-reminder>"), "{text}");
        assert!(
            text.contains("Tools now available: mcp__gh__new."),
            "{text}"
        );
        assert!(text.contains("no longer available (don't call them): mcp__gh__old"));
        assert_eq!(body["tools"].as_array().unwrap().len(), 1);
        assert!(!o.betas(&body).contains(&crate::TOOL_CHANGES_BETA));
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
