//! A plugin's hooks, prompt contributions, commands and event subscriptions, as kernel
//! adapters registered before the plugin runs. Each looks at what the plugin declared
//! at `initialize` when it is called.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use async_trait::async_trait;
use ch_kernel::reexports::CancellationToken;
use ch_kernel::util::{printable, terminal_text};
use ch_kernel::{
    AgentEvent, CmdCx, Command, Cx, EventSink, Hook, HookDecision, HookEvent, HookPoint, Outcome,
    PromptContributor,
};
use ch_plugin_protocol::{
    CommandRunParams, CommandRunResult, EventNotifyParams, HookInvokeParams, PromptSystemResult,
    PromptTurnParams, PromptTurnResult, method,
};
use ch_plugin_sdk::glob;
use serde_json::{Value, json};

use crate::conn::CallError;

use crate::manifest::{CommandDecl, OnError};
use crate::plugin::Shared;

const ALL_POINTS: &[HookPoint] = &[
    HookPoint::SessionStart,
    HookPoint::UserPrompt,
    HookPoint::BeforeModel,
    HookPoint::AfterModel,
    HookPoint::PreTool,
    HookPoint::PostTool,
    HookPoint::TurnEnd,
    HookPoint::SessionEnd,
];

/// All of a plugin's hooks. Registered at every point; a point the plugin didn't
/// declare (or a tool its globs don't match) costs no call.
pub struct ExtHook {
    shared: Arc<Shared>,
    id: String,
}

impl ExtHook {
    pub fn new(shared: Arc<Shared>) -> Self {
        Self {
            id: shared.m.source_id(),
            shared,
        }
    }
}

#[async_trait]
impl Hook for ExtHook {
    fn id(&self) -> &str {
        &self.id
    }

    fn points(&self) -> &[HookPoint] {
        ALL_POINTS
    }

    fn priority(&self) -> i32 {
        self.shared.m.hook_priority()
    }

    async fn on(&self, event: &HookEvent, cx: &Cx) -> HookDecision {
        let s = &self.shared;
        let point = event.point();
        let tool = event.tool_call().map(|c| c.name.as_str());
        let declared = s.caps().is_some_and(|caps| {
            caps.hooks.iter().any(|h| {
                h.point == point.as_str()
                    && (h.tools.is_empty()
                        || tool.is_some_and(|t| h.tools.iter().any(|g| glob(g, t))))
            })
        });
        // What the manifest promises to check, whether or not the plugin got as far as
        // saying so.
        let guarded = point == HookPoint::PreTool
            && tool.is_some_and(|t| s.m.guards.iter().any(|g| glob(g, t)));
        if !declared && !guarded {
            return HookDecision::Continue;
        }
        let failed = |why: &str| {
            if point == HookPoint::PreTool && s.m.on_error == OnError::Deny {
                HookDecision::deny(format!(
                    "The plugin `{}` checks this call, and its check didn't run ({why}), so \
                     the call wasn't run. Tell the user; don't retry until the plugin works.",
                    s.m.name
                ))
            } else {
                tracing::warn!(plugin = %s.m.name, point = point.as_str(), "hook failed: {why}");
                HookDecision::Continue
            }
        };
        if s.conn().is_none() {
            return failed("the plugin isn't running");
        }
        if !declared {
            // Running, and it didn't ask to see this call.
            return HookDecision::Continue;
        }
        let Ok(event_json) = serde_json::to_value(event) else {
            return failed("the event couldn't be encoded");
        };
        let params = HookInvokeParams {
            point: point.as_str().into(),
            event: event_json,
        };
        match s
            .call(method::HOOK_INVOKE, params, s.hook_timeout(), &cx.cancel)
            .await
        {
            Ok(v) => match serde_json::from_value::<HookDecision>(v) {
                Ok(d) => redact_decision(d, s),
                Err(_) => failed("its answer isn't a hook decision"),
            },
            // The turn is ending anyway; nothing failed.
            Err(CallError::Cancelled) if point == HookPoint::PreTool => {
                HookDecision::deny("cancelled: the user interrupted the turn")
            }
            Err(CallError::Cancelled) => HookDecision::Continue,
            Err(e) => failed(&e.to_string()),
        }
    }
}

/// The plugin's text reaches the model and the terminal: redacted. An `ask` loses its
/// key, so "always allow" can't be pointed at a key the permission check uses (or
/// saved as a permission rule); it asks every time.
fn redact_decision(d: HookDecision, s: &Shared) -> HookDecision {
    match d {
        HookDecision::Deny { reason } => HookDecision::Deny {
            reason: s.redact(&reason),
        },
        HookDecision::Ask { reason, .. } => HookDecision::Ask {
            reason: s.redact(&reason),
            key: None,
        },
        HookDecision::Stop { reason } => HookDecision::Stop {
            reason: s.redact(&reason),
        },
        HookDecision::AppendNote { text } => HookDecision::AppendNote {
            text: s.redact(&text),
        },
        HookDecision::Rewrite { input } => HookDecision::Rewrite {
            input: redact_value(input, s),
        },
        HookDecision::Continue => HookDecision::Continue,
    }
}

fn redact_value(v: Value, s: &Shared) -> Value {
    match v {
        Value::String(t) => Value::String(s.redact(&t)),
        Value::Array(items) => {
            Value::Array(items.into_iter().map(|i| redact_value(i, s)).collect())
        }
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(k, v)| (k, redact_value(v, s)))
                .collect(),
        ),
        other => other,
    }
}

/// What asking for the system blocks gave.
enum System {
    /// The plugin isn't running (yet): ask again later.
    NotRunning,
    /// It runs but didn't answer usefully: don't ask again this conversation.
    Failed,
    Blocks(Vec<String>),
}

/// System blocks and per-call reminders.
pub struct ExtPrompt {
    shared: Arc<Shared>,
    id: String,
}

impl ExtPrompt {
    pub fn new(shared: Arc<Shared>) -> Self {
        Self {
            id: shared.m.source_id(),
            shared,
        }
    }

    async fn system(&self, cx: &Cx) -> System {
        let s = &self.shared;
        if s.conn().is_none() {
            return System::NotRunning;
        }
        if !s.caps().is_some_and(|c| c.prompt.system) {
            return System::Blocks(Vec::new());
        }
        let answer = s
            .call(
                method::PROMPT_SYSTEM,
                json!({}),
                s.hook_timeout(),
                &cx.cancel,
            )
            .await;
        match answer.map(serde_json::from_value::<PromptSystemResult>) {
            // Labelled, so the model knows where the text comes from.
            Ok(Ok(r)) => System::Blocks(
                (r.blocks.iter())
                    .map(|b| format!("From the plugin `{}`:\n{}", s.m.name, s.redact(b)))
                    .collect(),
            ),
            Ok(Err(_)) => {
                tracing::warn!(plugin = %s.m.name, "prompt/system: an answer ch can't read");
                System::Failed
            }
            Err(e) => {
                tracing::warn!(plugin = %s.m.name, "prompt/system failed: {e}");
                System::Failed
            }
        }
    }
}

#[async_trait]
impl PromptContributor for ExtPrompt {
    fn id(&self) -> &str {
        &self.id
    }

    /// After built-in feature blocks.
    fn priority(&self) -> i32 {
        300
    }

    async fn system_blocks(&self, cx: &Cx) -> Vec<String> {
        match self.system(cx).await {
            // Not running yet: the blocks come later, once, as a reminder (ADR 0006 §5).
            System::NotRunning => {
                self.shared.system_delivered.store(false, Ordering::SeqCst);
                Vec::new()
            }
            System::Failed => {
                self.shared.system_delivered.store(true, Ordering::SeqCst);
                Vec::new()
            }
            System::Blocks(blocks) => {
                self.shared.system_delivered.store(true, Ordering::SeqCst);
                blocks
            }
        }
    }

    async fn turn_reminders(&self, cx: &Cx) -> Vec<String> {
        let s = &self.shared;
        let Some(caps) = s.caps() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        if caps.prompt.system && !s.system_delivered.load(Ordering::SeqCst) {
            match self.system(cx).await {
                System::NotRunning => {}
                System::Failed => s.system_delivered.store(true, Ordering::SeqCst),
                System::Blocks(blocks) => {
                    s.system_delivered.store(true, Ordering::SeqCst);
                    out.extend(blocks);
                }
            }
        }
        if caps.prompt.turn {
            let params = PromptTurnParams {
                turn: cx.turn,
                step: cx.step,
            };
            match s
                .call(method::PROMPT_TURN, params, s.hook_timeout(), &cx.cancel)
                .await
                .map(serde_json::from_value::<PromptTurnResult>)
            {
                Ok(Ok(r)) => out.extend(r.reminders.iter().map(|t| s.redact(t))),
                Ok(Err(e)) => tracing::warn!(plugin = %s.m.name, "prompt/turn answer: {e}"),
                Err(e) => tracing::warn!(plugin = %s.m.name, "prompt/turn failed: {e}"),
            }
        }
        out
    }
}

/// A slash command named in `plugin.toml`.
pub struct ExtCommand {
    shared: Arc<Shared>,
    decl: CommandDecl,
}

impl ExtCommand {
    pub fn new(shared: Arc<Shared>, decl: CommandDecl) -> Self {
        Self { shared, decl }
    }
}

#[async_trait]
impl Command for ExtCommand {
    fn name(&self) -> &str {
        &self.decl.name
    }

    fn description(&self) -> &str {
        &self.decl.description
    }

    fn usage(&self) -> &str {
        &self.decl.usage
    }

    /// A plugin can't take a built-in command's name.
    fn yields_on_conflict(&self) -> bool {
        true
    }

    async fn run(&self, args: &str, _cx: &mut CmdCx<'_>) -> anyhow::Result<Outcome> {
        let s = &self.shared;
        let params = CommandRunParams {
            name: self.decl.name.clone(),
            args: args.into(),
        };
        let cancel = CancellationToken::new();
        let r = match s
            .call(method::COMMAND_RUN, params, s.call_timeout(), &cancel)
            .await
        {
            Ok(v) => match serde_json::from_value::<CommandRunResult>(v) {
                Ok(r) => r,
                Err(_) => {
                    return Ok(Outcome::Message(format!(
                        "`/{}` (plugin `{}`): its answer isn't one ch can read",
                        self.decl.name, s.m.name
                    )));
                }
            },
            Err(e) => {
                return Ok(Outcome::Message(format!(
                    "`/{}` (plugin `{}`): {}",
                    self.decl.name,
                    s.m.name,
                    printable(&s.redact(&e.to_string()))
                )));
            }
        };
        Ok(match (r.prompt, r.message) {
            (Some(prompt), _) => Outcome::Prompt(s.redact(&prompt)),
            (None, Some(message)) => Outcome::Message(terminal_text(&s.redact(&message))),
            (None, None) => Outcome::Done,
        })
    }
}

/// Forwards the events the plugin subscribed to.
pub struct ExtSink {
    shared: Arc<Shared>,
    id: String,
}

impl ExtSink {
    pub fn new(shared: Arc<Shared>) -> Self {
        Self {
            id: shared.m.source_id(),
            shared,
        }
    }
}

#[async_trait]
impl EventSink for ExtSink {
    fn id(&self) -> &str {
        &self.id
    }

    async fn handle(&self, event: &AgentEvent) {
        let s = &self.shared;
        let Some(caps) = s.caps().filter(|c| !c.events.is_empty()) else {
            return;
        };
        // Deltas would flood the pipe; they are never sent.
        if matches!(
            event,
            AgentEvent::TextDelta { .. }
                | AgentEvent::ThinkingDelta { .. }
                | AgentEvent::ToolInputProgress { .. }
        ) {
            return;
        }
        let Ok(value) = serde_json::to_value(event) else {
            return;
        };
        let kind = value["type"].as_str().unwrap_or_default();
        if !caps.events.iter().any(|e| e == "*" || e == kind) {
            return;
        }
        if let Some(conn) = s.conn() {
            conn.notify(method::EVENT_NOTIFY, EventNotifyParams { event: value });
        }
    }
}

/// Shows what went wrong while loading plugins, once the session can show it.
pub struct LoadWarnings(pub Vec<String>);

#[async_trait]
impl Hook for LoadWarnings {
    fn id(&self) -> &str {
        "external-host"
    }

    fn points(&self) -> &[HookPoint] {
        &[HookPoint::SessionStart]
    }

    async fn on(&self, _event: &HookEvent, cx: &Cx) -> HookDecision {
        for message in &self.0 {
            cx.emit(AgentEvent::Warning {
                message: message.clone(),
            });
        }
        HookDecision::Continue
    }
}
