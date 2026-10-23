//! A conversation with the agent and the turn pipeline that drives it.

mod builder;
mod cancel;
mod model_call;
mod tools;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::approver::Approver;
use crate::command::{CmdCx, Outcome};
use crate::config::{Config, KernelConfig};
use crate::context::{ContextCx, ContextStrategy};
use crate::cx::Cx;
use crate::events::{AgentEvent, EventBus};
use crate::executor::Executor;
use crate::history::History;
use crate::hook::{Hook, HookEvent, HookOutcome, Verdict, run_hooks};
use crate::model::ModelProvider;
use crate::registry::Registry;
use crate::store::{SessionLock, SessionMeta, SessionSnapshot, SessionStore};
use crate::tokens::TokenCalibrator;
use crate::tool::{Tool, ToolOutput, ToolSpec};
use crate::types::{ContentBlock, Message, Role, StopReason, TerminationReason, ToolCall, Usage};
use crate::util::{system_reminder, truncate_middle};

pub use builder::SessionBuilder;
pub use cancel::CancelHandle;

use builder::restored_model;
use tools::tool_result;

#[derive(Debug, Clone, PartialEq)]
pub struct TurnResult {
    pub reason: TerminationReason,
    /// Text of the last assistant message.
    pub final_text: String,
    /// Model calls made in this turn.
    pub steps: u32,
}

/// Responses in a row whose tool calls were cut off by the output limit before the turn
/// stops. Each one is retried, so without a cap a model that keeps sending too-large
/// calls would use up `agent.max_steps`.
const MAX_CUT_OFF_TOOL_CALLS: u32 = 3;

pub struct Session {
    id: String,
    registry: Arc<Registry>,
    config: Arc<Config>,
    kcfg: KernelConfig,
    workspace: Arc<PathBuf>,
    history: History,
    events: EventBus,
    provider: Arc<dyn ModelProvider>,
    model: String,
    summarizer: Arc<dyn ModelProvider>,
    summarizer_model: String,
    context: Arc<dyn ContextStrategy>,
    executor: Option<Arc<dyn Executor>>,
    approver: Option<Arc<dyn Approver>>,
    tools: HashMap<String, Arc<dyn Tool>>,
    tool_specs: Arc<Vec<ToolSpec>>,
    hooks: Vec<Arc<dyn Hook>>,
    cancel: CancelHandle,
    turn: u32,
    usage: Usage,
    cost: f64,
    calibrator: TokenCalibrator,
    always_allow: HashSet<String>,
    store: Option<SessionStore>,
    _lock: Option<SessionLock>,
    created_at: chrono::DateTime<Utc>,
    sink_tasks: Vec<JoinHandle<()>>,
    started: bool,
    ended: bool,
}

impl Session {
    pub fn builder(registry: Arc<Registry>, config: Arc<Config>) -> SessionBuilder {
        SessionBuilder::new(registry, config)
    }

    // ----- accessors -----

    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn registry(&self) -> &Arc<Registry> {
        &self.registry
    }
    pub fn config(&self) -> &Arc<Config> {
        &self.config
    }
    pub fn kernel_config(&self) -> &KernelConfig {
        &self.kcfg
    }
    pub fn workspace(&self) -> &PathBuf {
        &self.workspace
    }
    pub fn history(&self) -> &History {
        &self.history
    }
    pub fn events(&self) -> &EventBus {
        &self.events
    }
    pub fn model(&self) -> &str {
        &self.model
    }
    pub fn set_model(&mut self, model: impl Into<String>) {
        self.model = model.into();
    }
    pub fn provider_id(&self) -> &str {
        self.provider.id()
    }
    pub fn usage(&self) -> Usage {
        self.usage
    }
    /// Total cost in USD (0 when pricing isn't configured).
    pub fn cost(&self) -> f64 {
        self.cost
    }
    pub fn turn(&self) -> u32 {
        self.turn
    }
    pub fn tool_specs(&self) -> &[ToolSpec] {
        &self.tool_specs
    }
    pub fn cancel_handle(&self) -> CancelHandle {
        self.cancel.clone()
    }
    pub fn store(&self) -> Option<&SessionStore> {
        self.store.as_ref()
    }

    pub fn context_window(&self) -> u64 {
        self.for_configured_model(self.kcfg.model.context_window)
            .or_else(|| self.provider.context_window(&self.model))
            .unwrap_or(self.kcfg.agent.default_context_window)
    }

    fn for_configured_model<T>(&self, setting: Option<T>) -> Option<T> {
        setting.filter(|_| self.model == self.kcfg.model.id)
    }

    /// Starts a new conversation under a new id (`/clear`), leaving the old one saved as
    /// it was. Usage totals are kept.
    pub fn clear(&mut self) -> anyhow::Result<()> {
        let id = builder::new_session_id();
        if let Some(store) = &self.store {
            self._lock = Some(store.lock(&id)?);
        }
        self.id = id;
        self.created_at = Utc::now();
        self.turn = 0;
        self.history.clear();
        Ok(())
    }

    pub fn snapshot(&self) -> SessionSnapshot {
        let title = self
            .history
            .first_user_text()
            .map(|t| truncate_middle(t.lines().next().unwrap_or(""), 80))
            .unwrap_or_default();
        SessionSnapshot {
            meta: SessionMeta {
                id: self.id.clone(),
                created_at: self.created_at,
                updated_at: Utc::now(),
                workspace: self.workspace.to_string_lossy().to_string(),
                model: self.model.clone(),
                provider: Some(self.provider.id().to_string()),
                title,
                turns: self.turn,
            },
            history: self.history.clone(),
            usage: self.usage,
            cost: self.cost,
        }
    }

    /// Replaces this session's conversation with a saved one (`/resume`).
    /// Fails, leaving this session as it was, when another process has `snap` open.
    pub fn restore(&mut self, snap: SessionSnapshot) -> anyhow::Result<()> {
        if let Some(store) = &self.store {
            self._lock = Some(store.lock(&snap.meta.id)?);
        }
        self.model = restored_model(&snap.meta, self.provider.id(), &self.kcfg.model.id);
        self.id = snap.meta.id;
        self.created_at = snap.meta.created_at;
        self.turn = snap.meta.turns;
        self.history = snap.history;
        self.usage = snap.usage;
        self.cost = snap.cost;
        Ok(())
    }

    pub fn save(&self) -> anyhow::Result<()> {
        if let Some(store) = &self.store
            && !self.history.is_empty()
        {
            store.save(&self.snapshot())?;
        }
        Ok(())
    }

    fn cx(&self, step: u32, cancel: CancellationToken) -> Cx {
        Cx {
            session_id: Arc::from(self.id.as_str()),
            workspace: self.workspace.clone(),
            services: self.registry.services.clone(),
            events: self.events.clone(),
            config: self.config.clone(),
            executor: self.executor.clone(),
            tools: self.tool_specs.clone(),
            turn: self.turn,
            step,
            cancel,
            tool_use_id: None,
            session_dir: self.store.as_ref().map(|s| Arc::new(s.aux_dir(&self.id))),
        }
    }

    fn context_cx(&self, cx: Cx) -> ContextCx {
        ContextCx {
            cx,
            summarizer: self.summarizer.clone(),
            summarizer_model: self.summarizer_model.clone(),
        }
    }

    // ----- lifecycle -----

    /// Starts sink tasks, emits `SessionStart` and runs session_start hooks. Called
    /// automatically by the first `run_turn`.
    pub async fn start(&mut self) {
        if self.started {
            return;
        }
        self.started = true;
        for (_, sink) in &self.registry.sinks {
            let sink = sink.clone();
            let mut rx = self.events.subscribe();
            self.sink_tasks.push(tokio::spawn(async move {
                loop {
                    match rx.recv().await {
                        Ok(ev) => {
                            let end = matches!(ev, AgentEvent::SessionEnd { .. });
                            sink.handle(&ev).await;
                            if end {
                                break;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!(sink = sink.id(), n, "event sink lagged");
                        }
                        Err(_) => break,
                    }
                }
                sink.flush().await;
            }));
        }
        self.events.emit(AgentEvent::SessionStart {
            session_id: self.id.clone(),
            model: self.model.clone(),
            workspace: self.workspace.to_string_lossy().to_string(),
            plugins: self.registry.enabled_plugin_ids(),
        });
        let cx = self.cx(0, CancellationToken::new());
        let mut ev = HookEvent::SessionStart;
        let _ = run_hooks(&self.hooks, &mut ev, &cx).await;
    }

    /// Runs session_end hooks, saves, emits `SessionEnd` and waits (bounded) for sinks
    /// to flush.
    pub async fn end(&mut self) {
        if self.ended || !self.started {
            self.ended = true;
            return;
        }
        self.ended = true;
        let cx = self.cx(0, CancellationToken::new());
        let mut ev = HookEvent::SessionEnd;
        let _ = run_hooks(&self.hooks, &mut ev, &cx).await;
        if let Err(e) = self.save() {
            tracing::warn!("saving session failed: {e:#}");
        }
        self.events.emit(AgentEvent::SessionEnd {
            session_id: self.id.clone(),
        });
        for task in self.sink_tasks.drain(..) {
            let _ = tokio::time::timeout(Duration::from_secs(10), task).await;
        }
    }

    /// Runs a slash command line such as `/model foo`. Returns `None` if `line` isn't a
    /// command.
    pub async fn run_command(&mut self, line: &str) -> Option<anyhow::Result<Outcome>> {
        let rest = line.trim().strip_prefix('/')?;
        let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        let Some(cmd) = self.registry.command(name) else {
            return Some(Ok(Outcome::Message(format!(
                "Unknown command `/{name}`. Type /help to see the available commands."
            ))));
        };
        let mut cx = CmdCx { session: self };
        Some(cmd.run(args.trim(), &mut cx).await)
    }

    /// Manual compaction through the active context strategy.
    pub async fn compact(&mut self) -> anyhow::Result<String> {
        let cancel = self.cancel.begin();
        let cx = self.cx(0, cancel);
        let system = self.system_blocks(&cx).await;
        let budget = self.budget(&system, &[]);
        let ccx = self.context_cx(cx);
        let result = self.context.compact(&self.history, budget, &ccx).await;
        self.cancel.finish();
        result
    }

    // ----- the turn pipeline -----

    /// Runs one user turn: the model is called repeatedly until it stops calling tools,
    /// a hook stops it, the step limit is hit, or the turn is cancelled.
    pub async fn run_turn(&mut self, prompt: impl Into<String>) -> TurnResult {
        self.start().await;
        let cancel = self.cancel.begin();
        self.turn += 1;
        let turn = self.turn;
        let mut prompt = prompt.into();
        self.events.emit(AgentEvent::TurnStart {
            turn,
            prompt: prompt.clone(),
        });

        // user_prompt hooks
        let cx0 = self.cx(0, cancel.clone());
        let mut ev = HookEvent::UserPrompt {
            text: prompt.clone(),
        };
        let outcome = run_hooks(&self.hooks, &mut ev, &cx0).await;
        if let HookEvent::UserPrompt { text } = ev {
            prompt = text;
        }
        self.emit_notes(&outcome);
        let denied = match outcome.verdict {
            Verdict::Deny { hook, reason } => {
                self.emit_denied(&hook, None, &reason);
                Some(TerminationReason::PromptDenied { reason })
            }
            Verdict::Stop { hook, reason } => Some(TerminationReason::HookStop {
                source: hook,
                reason,
            }),
            Verdict::Continue | Verdict::Ask { .. } => None,
        };
        if let Some(reason) = denied {
            self.notify_turn_end(&reason, 0, "", &cx0).await;
            return self.finish_turn(reason, 0, String::new()).await;
        }
        let mut user = Message::user(prompt);
        for (_, note) in &outcome.notes {
            user.content.push(ContentBlock::text(system_reminder(note)));
        }
        self.history.push(turn, user);

        let max_steps = self.kcfg.agent.max_steps.max(1);
        let mut final_text = String::new();
        let mut step: u32 = 0;
        let mut reminders = Vec::new();
        let mut cut_off_in_a_row = 0;
        let (reason, turn_end_ran) = loop {
            if cancel.is_cancelled() {
                break (TerminationReason::Cancelled, false);
            }
            if step >= max_steps {
                break (TerminationReason::StepLimit, false);
            }
            let cx = self.cx(step, cancel.clone());
            let response = match self.call_model(&cx, step, &mut reminders).await {
                Ok(r) => r,
                Err(reason) => break (reason, false),
            };
            step += 1;
            let text = response.message.text();
            if !text.trim().is_empty() {
                final_text = text;
            }
            // A refused or cut-off response may hold incomplete tool calls: never run them.
            let cut_off = match response.stop_reason {
                StopReason::Refusal => Some(TerminationReason::Done),
                StopReason::ContextWindowExceeded => Some(TerminationReason::ContextOverflow {
                    message: "the response was cut off because the context window is full".into(),
                }),
                _ => None,
            };
            // Hitting the output limit can leave the last tool call's input incomplete.
            let truncated = response.stop_reason == StopReason::MaxTokens;
            let mut response = response;
            if cut_off.is_some() {
                response
                    .message
                    .content
                    .retain(|b| !matches!(b, ContentBlock::ToolUse { .. }));
            }
            let calls = response.message.tool_calls();
            self.history.push(turn, response.message.clone());
            self.events.emit(AgentEvent::AssistantMessage {
                turn,
                step: step - 1,
                message: response.message.clone(),
            });

            // after_model hooks
            let mut ev = HookEvent::AfterModel { response };
            let outcome = run_hooks(&self.hooks, &mut ev, &cx).await;
            self.emit_notes(&outcome);
            reminders.extend(outcome.notes.iter().map(|(_, n)| n.clone()));
            let after_stop = match outcome.verdict {
                Verdict::Deny { hook, reason } | Verdict::Stop { hook, reason } => {
                    Some(TerminationReason::HookStop {
                        source: hook,
                        reason,
                    })
                }
                _ => None,
            };

            if let Some(r) = after_stop.or(cut_off) {
                if !calls.is_empty() {
                    // after_model stopped the turn: still answer every tool call.
                    self.history
                        .push(turn, not_run(&calls, "Not run: the turn was stopped."));
                }
                break (r, false);
            }
            if truncated && !calls.is_empty() {
                // Run none of them: the model sends them again, smaller.
                let limit = self.kcfg.model.max_output_tokens;
                self.events.emit(AgentEvent::Warning {
                    message: format!(
                        "the model reached its output limit ({limit} tokens) in the middle of \
                         a tool call; its tool calls were not run"
                    ),
                });
                let why = format!(
                    "Not run: your response reached the output limit ({limit} tokens) before \
                     this tool call was complete, so its input may be cut off. Send the call \
                     again with a smaller input, for example by splitting a large change into \
                     several smaller ones."
                );
                self.history.push(turn, not_run(&calls, &why));
                cut_off_in_a_row += 1;
                if cut_off_in_a_row >= MAX_CUT_OFF_TOOL_CALLS {
                    break (
                        TerminationReason::Error {
                            message: format!(
                                "the model reached its output limit ({limit} tokens) in the \
                                 middle of a tool call {cut_off_in_a_row} times in a row; raise \
                                 model.max_output_tokens or ask for smaller changes"
                            ),
                        },
                        false,
                    );
                }
                continue;
            }
            cut_off_in_a_row = 0;
            if calls.is_empty() {
                // The model finished: turn_end hooks may ask it to keep going.
                let mut ev = HookEvent::TurnEnd {
                    reason: TerminationReason::Done,
                    steps: step,
                    final_text: final_text.clone(),
                };
                let outcome = run_hooks(&self.hooks, &mut ev, &cx).await;
                self.emit_notes(&outcome);
                match outcome.verdict {
                    Verdict::Stop { hook, reason } => {
                        let stop = TerminationReason::HookStop {
                            source: hook,
                            reason,
                        };
                        break (stop, true);
                    }
                    _ => match outcome.notes_text() {
                        Some(notes) if !cancel.is_cancelled() => {
                            self.history
                                .push(turn, Message::user(system_reminder(&notes)));
                            continue;
                        }
                        _ => break (TerminationReason::Done, true),
                    },
                }
            }

            let (results, stop) = self.run_tools(calls, &cx).await;
            self.history.push(
                turn,
                Message {
                    role: Role::User,
                    content: results,
                },
            );
            if cancel.is_cancelled() {
                break (TerminationReason::Cancelled, false);
            }
            if let Some((hook, reason)) = stop {
                let stop = TerminationReason::HookStop {
                    source: hook,
                    reason,
                };
                break (stop, false);
            }
        };

        if !turn_end_ran {
            let cx = self.cx(step, cancel.clone());
            self.notify_turn_end(&reason, step, &final_text, &cx).await;
        }
        self.finish_turn(reason, step, final_text).await
    }

    async fn notify_turn_end(
        &self,
        reason: &TerminationReason,
        steps: u32,
        final_text: &str,
        cx: &Cx,
    ) {
        let mut ev = HookEvent::TurnEnd {
            reason: reason.clone(),
            steps,
            final_text: final_text.to_string(),
        };
        let outcome = run_hooks(&self.hooks, &mut ev, cx).await;
        self.emit_notes(&outcome);
    }

    async fn finish_turn(
        &mut self,
        reason: TerminationReason,
        steps: u32,
        final_text: String,
    ) -> TurnResult {
        self.cancel.finish();
        self.events.emit(AgentEvent::TurnEnd {
            turn: self.turn,
            reason: reason.clone(),
            steps,
        });
        if let Err(e) = self.save() {
            self.events.emit(AgentEvent::Warning {
                message: format!("could not save session: {e:#}"),
            });
        }
        TurnResult {
            reason,
            final_text,
            steps,
        }
    }

    fn emit_notes(&self, outcome: &HookOutcome) {
        for (source, text) in &outcome.notes {
            self.events.emit(AgentEvent::Note {
                source: source.clone(),
                text: text.clone(),
            });
        }
    }

    fn emit_denied(&self, hook: &str, tool: Option<&str>, reason: &str) {
        self.events.emit(AgentEvent::HookDenied {
            hook: hook.to_string(),
            tool: tool.map(str::to_string),
            reason: reason.to_string(),
        });
    }
}

fn not_run(calls: &[ToolCall], why: &str) -> Message {
    Message {
        role: Role::User,
        content: calls
            .iter()
            .map(|c| tool_result(&c.id, ToolOutput::error(why)))
            .collect(),
    }
}
