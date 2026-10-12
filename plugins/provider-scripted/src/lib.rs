//! Scripted model provider for deterministic tests (provider id `scripted`).
//!
//! Replays a script of responses, one per model call, and records every request it
//! receives. Off by default; enable it with `plugins.enable = ["provider-scripted"]` and
//! `model.provider = "scripted"`.
//!
//! Config (`[plugins.provider-scripted]`):
//! - `script = "<path to JSON>"`: relative paths resolve against the workspace.
//! - `context_window = <u64>`: requests whose estimated size exceeds it fail with
//!   `ModelError::ContextOverflow` (without consuming a step).
//!
//! Script format: a JSON array of steps. A step is either a reply
//! `{"text": "...", "tool_calls": [{"name": "...", "input": {...}}], "thinking": "...",
//! "usage": {"input_tokens": n, "output_tokens": n}}` (all fields optional) or an error
//! `{"error": "context_overflow" | "rate_limited" | "unavailable"}`. Tool-use ids are
//! `call_<step>_<i>` (both 0-based). After the last step every call replies
//! `"(script exhausted)"`.

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::Context as _;
use ch_kernel::reexports::{CancellationToken, async_trait};
use ch_kernel::{
    ContentBlock, Message, ModelError, ModelEvent, ModelProvider, ModelRequest, ModelResponse,
    ModelStream, Plugin, PluginManifest, RegisterCx, Role, StopReason, Usage, estimate_tokens,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Text of the reply once the script has run out.
pub const EXHAUSTED: &str = "(script exhausted)";

/// One scripted tool call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScriptToolCall {
    pub name: String,
    #[serde(default = "empty_object")]
    pub input: Value,
}

fn empty_object() -> Value {
    Value::Object(Default::default())
}

/// Token counts reported for a step. Missing counts are estimated from the request and
/// the reply.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct ScriptUsage {
    #[serde(default)]
    pub input_tokens: Option<u64>,
    #[serde(default)]
    pub output_tokens: Option<u64>,
}

/// An error a step can return instead of a reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScriptError {
    ContextOverflow,
    RateLimited,
    Unavailable,
}

impl ScriptError {
    fn to_model_error(self) -> ModelError {
        match self {
            Self::ContextOverflow => {
                ModelError::ContextOverflow("scripted context overflow".into())
            }
            Self::RateLimited => ModelError::RateLimited("scripted rate limit".into()),
            Self::Unavailable => ModelError::Unavailable("scripted outage".into()),
        }
    }
}

/// One step of a script: a reply, or an error when `error` is set.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScriptStep {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ScriptToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<ScriptUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ScriptError>,
}

impl ScriptStep {
    /// A plain text reply.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: Some(text.into()),
            ..Default::default()
        }
    }

    /// A reply that calls one tool.
    pub fn tool(name: impl Into<String>, input: Value) -> Self {
        Self {
            tool_calls: vec![ScriptToolCall {
                name: name.into(),
                input,
            }],
            ..Default::default()
        }
    }

    /// An error step.
    pub fn error(error: ScriptError) -> Self {
        Self {
            error: Some(error),
            ..Default::default()
        }
    }
}

/// Parses a script (a JSON array of steps).
pub fn parse_script(json: &str) -> anyhow::Result<Vec<ScriptStep>> {
    serde_json::from_str(json).context(
        "invalid script: expected a JSON array of steps like \
         {\"text\": \"...\", \"tool_calls\": [{\"name\": \"...\", \"input\": {}}]} or \
         {\"error\": \"context_overflow\" | \"rate_limited\" | \"unavailable\"}",
    )
}

/// Loads a script file.
pub fn load_script(path: &Path) -> anyhow::Result<Vec<ScriptStep>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading script {}", path.display()))?;
    parse_script(&text).with_context(|| format!("in script {}", path.display()))
}

#[derive(Debug, Default)]
struct State {
    steps: Vec<ScriptStep>,
    next: usize,
    requests: Vec<ModelRequest>,
}

/// The `scripted` provider. Clones share the same script and request log.
#[derive(Debug, Clone, Default)]
pub struct ScriptedProvider {
    state: Arc<Mutex<State>>,
    context_window: Option<u64>,
}

impl ScriptedProvider {
    pub fn new(steps: Vec<ScriptStep>) -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                steps,
                ..Default::default()
            })),
            context_window: None,
        }
    }

    /// Reports `window` as the context window and rejects requests estimated to exceed it.
    pub fn with_context_window(mut self, window: u64) -> Self {
        self.context_window = Some(window);
        self
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // A panicking test thread must not hide the log from the rest of the test.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Every request received so far, in order.
    pub fn requests(&self) -> Vec<ModelRequest> {
        self.lock().requests.clone()
    }

    /// How many steps have been consumed.
    pub fn steps_used(&self) -> usize {
        self.lock().next
    }

    /// How many steps are left.
    pub fn steps_remaining(&self) -> usize {
        let st = self.lock();
        st.steps.len().saturating_sub(st.next)
    }
}

/// Estimated prompt size of `req`, in tokens.
pub fn estimate_request_tokens(req: &ModelRequest) -> u64 {
    let system: usize = req.system.iter().map(String::len).sum();
    let messages: usize = req.messages.iter().map(Message::char_len).sum();
    let tools: usize = req
        .tools
        .iter()
        .map(|t| t.name.len() + t.description.len() + t.input_schema.to_string().len())
        .sum();
    estimate_tokens(system + messages + tools)
}

/// Splits `text` into up to three deltas on char boundaries.
fn deltas(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return Vec::new();
    }
    let size = chars.len().div_ceil(3).max(1);
    chars
        .chunks(size)
        .map(|c| c.iter().collect::<String>())
        .collect()
}

fn reply(step_index: usize, step: &ScriptStep, prompt_tokens: u64) -> Vec<ModelEvent> {
    let mut events = Vec::new();
    let mut content = Vec::new();
    if let Some(thinking) = step.thinking.as_deref().filter(|t| !t.is_empty()) {
        events.extend(deltas(thinking).into_iter().map(ModelEvent::ThinkingDelta));
        content.push(ContentBlock::Thinking {
            thinking: thinking.to_string(),
            signature: None,
        });
    }
    if let Some(text) = step.text.as_deref().filter(|t| !t.is_empty()) {
        events.extend(deltas(text).into_iter().map(ModelEvent::TextDelta));
        content.push(ContentBlock::text(text));
    }
    for (i, call) in step.tool_calls.iter().enumerate() {
        let id = format!("call_{step_index}_{i}");
        events.push(ModelEvent::ToolUseStart {
            id: id.clone(),
            name: call.name.clone(),
        });
        content.push(ContentBlock::ToolUse {
            id,
            name: call.name.clone(),
            input: call.input.clone(),
        });
    }
    let message = Message {
        role: Role::Assistant,
        content,
    };
    let usage = step.usage.unwrap_or_default();
    let usage = Usage {
        input_tokens: usage.input_tokens.unwrap_or(prompt_tokens),
        output_tokens: usage
            .output_tokens
            .unwrap_or_else(|| estimate_tokens(message.char_len())),
        ..Default::default()
    };
    let stop_reason = if step.tool_calls.is_empty() {
        StopReason::EndTurn
    } else {
        StopReason::ToolUse
    };
    events.push(ModelEvent::Completed(ModelResponse {
        message,
        stop_reason,
        usage,
    }));
    events
}

#[async_trait]
impl ModelProvider for ScriptedProvider {
    fn id(&self) -> &str {
        "scripted"
    }

    async fn stream(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
    ) -> Result<ModelStream, ModelError> {
        if cancel.is_cancelled() {
            return Err(ModelError::Cancelled);
        }
        let prompt_tokens = estimate_request_tokens(&request);
        let events = {
            let mut st = self.lock();
            st.requests.push(request);
            if let Some(window) = self.context_window
                && prompt_tokens > window
            {
                return Err(ModelError::ContextOverflow(format!(
                    "request is ~{prompt_tokens} tokens, over the scripted window of {window}"
                )));
            }
            let index = st.next;
            match st.steps.get(index).cloned() {
                Some(step) => {
                    st.next += 1;
                    if let Some(err) = step.error {
                        return Err(err.to_model_error());
                    }
                    reply(index, &step, prompt_tokens)
                }
                None => reply(index, &ScriptStep::text(EXHAUSTED), prompt_tokens),
            }
        };
        Ok(Box::pin(futures::stream::iter(events.into_iter().map(Ok))))
    }

    fn context_window(&self, _model: &str) -> Option<u64> {
        self.context_window
    }
}

/// `[plugins.provider-scripted]`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct ScriptedConfig {
    /// Path to the JSON script; relative to the workspace.
    pub script: Option<String>,
    /// Context window to report and enforce.
    pub context_window: Option<u64>,
}

/// Registers the `scripted` provider, from config or from in-memory steps.
#[derive(Debug, Clone, Default)]
pub struct ScriptedPlugin {
    preset: Option<ScriptedProvider>,
}

impl ScriptedPlugin {
    /// A plugin that serves `steps` instead of reading a script file. Use
    /// [`ScriptedPlugin::provider`] to inspect the requests afterwards.
    pub fn from_steps(steps: Vec<ScriptStep>) -> Self {
        Self::from_provider(ScriptedProvider::new(steps))
    }

    /// A plugin that registers (a clone of) `provider`.
    pub fn from_provider(provider: ScriptedProvider) -> Self {
        Self {
            preset: Some(provider),
        }
    }

    /// The preset provider, sharing its script and request log with the registered one.
    pub fn provider(&self) -> Option<ScriptedProvider> {
        self.preset.clone()
    }
}

impl Plugin for ScriptedPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest::new(
            "provider-scripted",
            "Scripted model provider that replays canned responses (tests only)",
        )
        .provides(&["provider:scripted"])
        .default_enabled(false)
    }

    fn register(&self, cx: &mut RegisterCx<'_>) -> anyhow::Result<()> {
        let cfg: ScriptedConfig = cx.config()?;
        let mut provider = match (&self.preset, &cfg.script) {
            (Some(p), _) => p.clone(),
            (None, Some(path)) => {
                let path = cx.workspace().join(path);
                ScriptedProvider::new(
                    load_script(&path)
                        .context("plugins.provider-scripted.script must point at a JSON script")?,
                )
            }
            (None, None) => ScriptedProvider::new(Vec::new()),
        };
        if let Some(window) = cfg.context_window {
            provider = provider.with_context_window(window);
        }
        cx.add_provider(provider);
        Ok(())
    }
}

/// The plugin (reads its script from config), for the CLI's plugin list.
pub fn plugin() -> Arc<dyn Plugin> {
    Arc::new(ScriptedPlugin::default())
}
