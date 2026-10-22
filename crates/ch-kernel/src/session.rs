//! A conversation with the agent and the turn pipeline that drives it.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use chrono::Utc;
use futures::StreamExt;
use futures::future::join_all;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::approver::{Approval, ApprovalRequest, Approver};
use crate::command::{CmdCx, Outcome};
use crate::config::{Config, KernelConfig};
use crate::context::{Budget, ContextCx, ContextStrategy};
use crate::cx::Cx;
use crate::events::{AgentEvent, EventBus};
use crate::executor::Executor;
use crate::history::History;
use crate::hook::{Hook, HookEvent, HookOutcome, Verdict, run_hooks};
use crate::model::{ModelError, ModelEvent, ModelProvider, ModelRequest, ModelResponse};
use crate::registry::Registry;
use crate::store::{SessionMeta, SessionSnapshot, SessionStore};
use crate::tokens::{TokenCalibrator, estimate_tokens};
use crate::tool::{Tool, ToolOutput, ToolSpec};
use crate::types::{ContentBlock, Message, Role, StopReason, TerminationReason, ToolCall, Usage};
use crate::util::{system_reminder, truncate_middle};

/// How long a cancelled tool gets to wind down before it is aborted.
const CANCEL_GRACE: Duration = Duration::from_secs(3);

/// Minimum time between `ToolInputProgress` events for one tool call.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, PartialEq)]
pub struct TurnResult {
    pub reason: TerminationReason,
    /// Text of the last assistant message.
    pub final_text: String,
    /// Model calls made in this turn.
    pub steps: u32,
}

/// Cancels the running turn. Clone it before starting a turn and call
/// [`CancelHandle::cancel`] from another task (e.g. on Ctrl-C).
///
/// A cancel that arrives while no turn is running is kept and applies to the next turn
/// (so an early Ctrl-C in headless mode isn't lost). Call
/// [`CancelHandle::clear_pending`] to drop it.
#[derive(Clone, Default)]
pub struct CancelHandle(Arc<Mutex<CancelState>>);

#[derive(Default)]
struct CancelState {
    token: CancellationToken,
    active: bool,
    pending: bool,
}

impl CancelHandle {
    pub fn cancel(&self) {
        let mut s = self.0.lock().expect("cancel lock");
        if s.active {
            s.token.cancel();
        } else {
            s.pending = true;
        }
    }

    /// Whether the running turn is cancelled, or a cancel is waiting for the next one.
    pub fn is_cancelled(&self) -> bool {
        let s = self.0.lock().expect("cancel lock");
        s.pending || (s.active && s.token.is_cancelled())
    }

    /// Forgets a cancel requested while no turn was running.
    pub fn clear_pending(&self) {
        self.0.lock().expect("cancel lock").pending = false;
    }

    /// Starts a turn: a fresh token, already cancelled if a cancel is pending.
    fn begin(&self) -> CancellationToken {
        let mut s = self.0.lock().expect("cancel lock");
        s.token = CancellationToken::new();
        if std::mem::take(&mut s.pending) {
            s.token.cancel();
        }
        s.active = true;
        s.token.clone()
    }

    fn finish(&self) {
        self.0.lock().expect("cancel lock").active = false;
    }
}

pub struct SessionBuilder {
    registry: Arc<Registry>,
    config: Arc<Config>,
    workspace: PathBuf,
    id: Option<String>,
    approver: Option<String>,
    store: Option<SessionStore>,
    restore: Option<SessionSnapshot>,
    events: Option<EventBus>,
    model: Option<String>,
}

impl SessionBuilder {
    pub fn new(registry: Arc<Registry>, config: Arc<Config>) -> Self {
        Self {
            registry,
            config,
            workspace: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            id: None,
            approver: None,
            store: None,
            restore: None,
            events: None,
            model: None,
        }
    }

    /// Model id; overrides `model.id` and the model of a restored session.
    pub fn model(mut self, id: impl Into<String>) -> Self {
        self.model = Some(id.into());
        self
    }

    pub fn workspace(mut self, workspace: impl Into<PathBuf>) -> Self {
        self.workspace = workspace.into();
        self
    }

    pub fn id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }

    /// Approver id; overrides `select.approver`.
    pub fn approver(mut self, id: impl Into<String>) -> Self {
        self.approver = Some(id.into());
        self
    }

    /// Persist the session to this store after every turn.
    pub fn store(mut self, store: SessionStore) -> Self {
        self.store = Some(store);
        self
    }

    /// Continue a saved session.
    pub fn restore(mut self, snapshot: SessionSnapshot) -> Self {
        self.restore = Some(snapshot);
        self
    }

    /// Use an existing bus (so a frontend can subscribe before the session exists).
    pub fn events(mut self, bus: EventBus) -> Self {
        self.events = Some(bus);
        self
    }

    pub fn build(self) -> anyhow::Result<Session> {
        let kcfg = self.config.kernel()?;
        let registry = self.registry;
        let provider = registry.provider(&kcfg.model.provider)?;
        let summ = kcfg.model.summarizer.clone().unwrap_or_default();
        let summarizer = match &summ.provider {
            Some(id) => Some(registry.provider(id)?),
            None => Some(provider.clone()),
        };
        let summarizer_model = summ.id.unwrap_or_else(|| kcfg.model.id.clone());
        let context = registry.context(kcfg.select.context.as_deref())?;
        let executor = registry.executor(kcfg.select.executor.as_deref())?;
        let approver_id = self.approver.or(kcfg.select.approver.clone());
        let approver = registry.approver(approver_id.as_deref())?;
        let tools: HashMap<String, Arc<dyn Tool>> = registry
            .tools
            .iter()
            .map(|(_, t)| (t.spec().name, t.clone()))
            .collect();
        let tool_specs = Arc::new(registry.tool_specs());
        let hooks = registry.hooks.iter().map(|(_, h)| h.clone()).collect();
        let workspace = Arc::new(self.workspace);

        let now = Utc::now();
        let (id, history, usage, cost, created_at, model, turn) = match self.restore {
            Some(snap) => (
                snap.meta.id.clone(),
                snap.history,
                snap.usage,
                snap.cost,
                snap.meta.created_at,
                restored_model(&snap.meta, provider.id(), &kcfg.model.id),
                snap.meta.turns,
            ),
            None => (
                self.id.unwrap_or_else(new_session_id),
                History::new(),
                Usage::default(),
                0.0,
                now,
                kcfg.model.id.clone(),
                0,
            ),
        };
        let model = self.model.unwrap_or(model);

        Ok(Session {
            id,
            registry,
            config: self.config,
            kcfg,
            workspace,
            history,
            events: self.events.unwrap_or_default(),
            provider,
            model,
            summarizer,
            summarizer_model,
            context,
            executor,
            approver,
            tools,
            tool_specs,
            hooks,
            cancel: CancelHandle::default(),
            turn,
            usage,
            cost,
            calibrator: TokenCalibrator::default(),
            always_allow: HashSet::new(),
            store: self.store,
            created_at,
            sink_tasks: Vec::new(),
            started: false,
            ended: false,
            pending_reminders: Vec::new(),
            peak_prompt_tokens: 0,
        })
    }
}

/// The model to continue a saved session with: the saved one, unless it ran on another
/// provider than the configured one (a `claude-*` id means nothing to OpenAI).
fn restored_model(meta: &SessionMeta, provider: &str, configured: &str) -> String {
    match &meta.provider {
        Some(p) if p != provider => configured.to_string(),
        _ => meta.model.clone(),
    }
}

fn new_session_id() -> String {
    let ts = Utc::now().format("%Y%m%d-%H%M%S");
    let short = uuid::Uuid::new_v4().simple().to_string();
    format!("{ts}-{}", &short[..6])
}

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
    summarizer: Option<Arc<dyn ModelProvider>>,
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
    created_at: chrono::DateTime<Utc>,
    sink_tasks: Vec<JoinHandle<()>>,
    started: bool,
    ended: bool,
    /// One-shot reminders for the next model call (from after_model hooks).
    pending_reminders: Vec<String>,
    peak_prompt_tokens: u64,
}

/// A tool call after pre_tool hooks and approval.
enum Prepared {
    Run {
        call: ToolCall,
        tool: Arc<dyn Tool>,
        notes: Vec<(String, String)>,
    },
    Done {
        call: ToolCall,
        output: ToolOutput,
        denied: bool,
        notes: Vec<(String, String)>,
    },
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
    pub fn history_mut(&mut self) -> &mut History {
        &mut self.history
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
    pub fn peak_prompt_tokens(&self) -> u64 {
        self.peak_prompt_tokens
    }
    pub fn context_strategy_id(&self) -> &str {
        self.context.id()
    }

    pub fn context_window(&self) -> u64 {
        self.for_configured_model(self.kcfg.model.context_window)
            .or_else(|| self.provider.context_window(&self.model))
            .unwrap_or(self.kcfg.agent.default_context_window)
    }

    /// A `[model]` setting such as `context_window` or `pricing` describes `model.id`,
    /// so it doesn't apply after `/model` or a resume switched to another model.
    fn for_configured_model<T>(&self, setting: Option<T>) -> Option<T> {
        setting.filter(|_| self.model == self.kcfg.model.id)
    }

    /// Forgets the conversation (`/clear`). Usage totals are kept.
    pub fn clear(&mut self) {
        self.history.clear();
        self.pending_reminders.clear();
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
    pub fn restore(&mut self, snap: SessionSnapshot) {
        self.model = restored_model(&snap.meta, self.provider.id(), &self.kcfg.model.id);
        self.id = snap.meta.id;
        self.created_at = snap.meta.created_at;
        self.turn = snap.meta.turns;
        self.history = snap.history;
        self.usage = snap.usage;
        self.cost = snap.cost;
        self.pending_reminders.clear();
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
            self.turn_end_hooks(&reason, 0, "", &cx0).await;
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
        // Whether turn_end hooks already ran for the way this turn ends.
        let mut turn_end_ran = false;
        let reason = loop {
            if cancel.is_cancelled() {
                break TerminationReason::Cancelled;
            }
            if step >= max_steps {
                break TerminationReason::StepLimit;
            }
            let cx = self.cx(step, cancel.clone());
            let response = match self.call_model(&cx, step).await {
                Ok(Ok(r)) => r,
                Ok(Err(reason)) => break reason,
                Err(e) => {
                    break TerminationReason::Error {
                        message: format!("{e:#}"),
                    };
                }
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
            self.pending_reminders
                .extend(outcome.notes.iter().map(|(_, n)| n.clone()));
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
                    let results = calls
                        .iter()
                        .map(|c| {
                            tool_result(&c.id, ToolOutput::error("Not run: the turn was stopped."))
                        })
                        .collect();
                    self.history.push(
                        turn,
                        Message {
                            role: Role::User,
                            content: results,
                        },
                    );
                }
                break r;
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
                let results = calls
                    .iter()
                    .map(|c| tool_result(&c.id, ToolOutput::error(why.clone())))
                    .collect();
                self.history.push(
                    turn,
                    Message {
                        role: Role::User,
                        content: results,
                    },
                );
                continue;
            }
            if calls.is_empty() {
                // The model finished: turn_end hooks may ask it to keep going.
                let mut ev = HookEvent::TurnEnd {
                    reason: TerminationReason::Done,
                    steps: step,
                    final_text: final_text.clone(),
                };
                let outcome = run_hooks(&self.hooks, &mut ev, &cx).await;
                self.emit_notes(&outcome);
                turn_end_ran = true;
                match outcome.verdict {
                    Verdict::Stop { hook, reason } => {
                        break TerminationReason::HookStop {
                            source: hook,
                            reason,
                        };
                    }
                    _ => match outcome.notes_text() {
                        Some(notes) if !cancel.is_cancelled() => {
                            self.history
                                .push(turn, Message::user(system_reminder(&notes)));
                            // The turn goes on, so it hasn't ended yet.
                            turn_end_ran = false;
                            continue;
                        }
                        _ => break TerminationReason::Done,
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
                break TerminationReason::Cancelled;
            }
            if let Some((hook, reason)) = stop {
                break TerminationReason::HookStop {
                    source: hook,
                    reason,
                };
            }
        };

        if !turn_end_ran {
            let cx = self.cx(step, cancel.clone());
            self.turn_end_hooks(&reason, step, &final_text, &cx).await;
        }
        self.finish_turn(reason, step, final_text).await
    }

    /// Runs turn_end hooks for a turn that ends anyway: their decisions can't change it.
    async fn turn_end_hooks(
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
        // Reminders from after_model hooks belong to this turn's next call, which
        // won't happen.
        self.pending_reminders.clear();
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

    async fn system_blocks(&self, cx: &Cx) -> Vec<String> {
        let mut blocks = Vec::new();
        for (_, p) in &self.registry.prompts {
            blocks.extend(
                p.system_blocks(cx)
                    .await
                    .into_iter()
                    .filter(|b| !b.trim().is_empty()),
            );
        }
        blocks
    }

    fn budget(&self, system: &[String], reminders: &[String]) -> Budget {
        let fixed_chars: usize = system.iter().map(String::len).sum::<usize>()
            + reminders.iter().map(String::len).sum::<usize>()
            + self
                .tool_specs
                .iter()
                .map(|t| t.name.len() + t.description.len() + t.input_schema.to_string().len())
                .sum::<usize>();
        Budget {
            window: self.context_window(),
            reserved_output: u64::from(self.kcfg.model.max_output_tokens),
            fixed_tokens: self.calibrator.calibrate(estimate_tokens(fixed_chars)),
            calibrator: self.calibrator,
        }
    }

    /// One model call. The outer `Result` is for unexpected kernel errors; the inner
    /// `Err` is a normal way for a turn to end.
    async fn call_model(
        &mut self,
        cx: &Cx,
        step: u32,
    ) -> anyhow::Result<Result<ModelResponse, TerminationReason>> {
        let cancel = cx.cancel.clone();
        let system = self.system_blocks(cx).await;
        let mut reminders = Vec::new();
        for (_, p) in &self.registry.prompts {
            reminders.extend(
                p.turn_reminders(cx)
                    .await
                    .into_iter()
                    .filter(|r| !r.trim().is_empty()),
            );
        }
        reminders.append(&mut self.pending_reminders);

        let budget = self.budget(&system, &reminders);
        let ccx = self.context_cx(cx.clone());
        let view = self
            .context
            .view(&self.history, budget, &ccx)
            .await
            .with_context(|| format!("context strategy `{}` failed", self.context.id()))?;
        let mut messages = view.messages;
        let volatile_tail = reminders.len();
        if !reminders.is_empty() {
            let blocks = reminders
                .iter()
                .map(|r| ContentBlock::text(system_reminder(r)));
            match messages.last_mut() {
                Some(last) if last.role == Role::User => last.content.extend(blocks),
                _ => messages.push(Message {
                    role: Role::User,
                    content: blocks.collect(),
                }),
            }
        }

        let request = ModelRequest {
            model: self.model.clone(),
            system,
            messages,
            tools: self.tool_specs.to_vec(),
            max_tokens: self.kcfg.model.max_output_tokens,
            temperature: self.kcfg.model.temperature,
            thinking_budget: self.kcfg.model.thinking_budget,
            purpose: "agent".into(),
            volatile_tail,
        };

        // before_model hooks
        let mut ev = HookEvent::BeforeModel { request };
        let outcome = run_hooks(&self.hooks, &mut ev, cx).await;
        self.emit_notes(&outcome);
        let HookEvent::BeforeModel { mut request } = ev else {
            unreachable!("hooks cannot change the event kind")
        };
        match outcome.verdict {
            Verdict::Deny { hook, reason } | Verdict::Stop { hook, reason } => {
                return Ok(Err(TerminationReason::HookStop {
                    source: hook,
                    reason,
                }));
            }
            _ => {}
        }
        if let Some(notes) = outcome.notes_text() {
            let block = ContentBlock::text(system_reminder(&notes));
            match request.messages.last_mut() {
                Some(last) if last.role == Role::User => last.content.push(block),
                _ => request.messages.push(Message {
                    role: Role::User,
                    content: vec![block],
                }),
            }
            request.volatile_tail += 1;
        }

        let raw_est = estimate_request_tokens(&request);
        self.events.emit(AgentEvent::ModelRequest {
            turn: self.turn,
            step,
            model: request.model.clone(),
            purpose: request.purpose.clone(),
            messages: request.messages.len(),
            tokens_est: self.calibrator.calibrate(raw_est),
            context_window: budget.window,
        });

        let result = self.stream_model(request, cancel).await;
        let response = match result {
            Ok(r) => r,
            Err(ModelError::Cancelled) => return Ok(Err(TerminationReason::Cancelled)),
            Err(ModelError::ContextOverflow(message)) => {
                return Ok(Err(TerminationReason::ContextOverflow { message }));
            }
            Err(e) => {
                return Ok(Err(TerminationReason::Error {
                    message: e.to_string(),
                }));
            }
        };

        let actual = response.usage.prompt_tokens();
        self.calibrator.observe(raw_est, actual);
        self.peak_prompt_tokens = self.peak_prompt_tokens.max(actual);
        self.record_usage(&response.usage, "agent");
        Ok(Ok(response))
    }

    /// Records usage and cost from any model call (plugins making their own calls,
    /// like summaries, should report through events; the kernel only counts its own).
    fn record_usage(&mut self, usage: &Usage, purpose: &str) {
        self.usage.add(usage);
        let cost = self
            .for_configured_model(self.kcfg.model.pricing)
            .map(|p| p.cost(usage));
        if let Some(c) = cost {
            self.cost += c;
        }
        self.events.emit(AgentEvent::Usage {
            usage: *usage,
            cost,
            purpose: purpose.to_string(),
        });
    }

    async fn stream_model(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        let mut stream = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(ModelError::Cancelled),
            s = self.provider.stream(request, cancel.clone()) => s?,
        };
        // Tool calls whose input is streaming: id → (name, bytes so far, last progress).
        let mut inputs: HashMap<String, (String, u64, Instant)> = HashMap::new();
        loop {
            let next = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(ModelError::Cancelled),
                n = stream.next() => n,
            };
            match next {
                None => {
                    return Err(ModelError::Other(anyhow::anyhow!(
                        "model stream ended without a final response"
                    )));
                }
                Some(Err(e)) => return Err(e),
                Some(Ok(ModelEvent::TextDelta(text))) => {
                    self.events.emit(AgentEvent::TextDelta { text });
                }
                Some(Ok(ModelEvent::ThinkingDelta(text))) => {
                    self.events.emit(AgentEvent::ThinkingDelta { text });
                }
                Some(Ok(ModelEvent::ToolUseStart { id, name })) => {
                    self.events.emit(AgentEvent::ToolInputStarted {
                        id: id.clone(),
                        name: name.clone(),
                    });
                    inputs.insert(id, (name, 0, Instant::now()));
                }
                Some(Ok(ModelEvent::ToolInputDelta { id, len })) => {
                    if let Some((name, bytes, last)) = inputs.get_mut(&id) {
                        *bytes += len as u64;
                        if last.elapsed() >= PROGRESS_INTERVAL {
                            *last = Instant::now();
                            self.events.emit(AgentEvent::ToolInputProgress {
                                id,
                                name: name.clone(),
                                bytes: *bytes,
                            });
                        }
                    }
                }
                Some(Ok(ModelEvent::Completed(r))) => return Ok(r),
            }
        }
    }

    // ----- tools -----

    /// Runs all tool calls from one assistant message. Returns one `ToolResult` per call,
    /// in call order, and the first stop request from a hook.
    async fn run_tools(
        &mut self,
        calls: Vec<ToolCall>,
        cx: &Cx,
    ) -> (Vec<ContentBlock>, Option<(String, String)>) {
        let mut results = Vec::with_capacity(calls.len());
        let mut stop: Option<(String, String)> = None;

        // Group consecutive read-only calls into concurrent batches.
        let mut batches: Vec<Vec<ToolCall>> = Vec::new();
        for call in calls {
            let ro = self
                .tools
                .get(&call.name)
                .is_some_and(|t| t.spec().read_only);
            match batches.last_mut() {
                Some(b)
                    if ro
                        && b.iter().all(|c| {
                            self.tools.get(&c.name).is_some_and(|t| t.spec().read_only)
                        }) =>
                {
                    b.push(call)
                }
                _ => batches.push(vec![call]),
            }
        }

        let sem = Arc::new(Semaphore::new(self.kcfg.agent.max_parallel_readonly.max(1)));
        for batch in batches {
            let mut prepared = Vec::with_capacity(batch.len());
            for call in batch {
                // Checked per call, so a stop or a cancel also skips the rest of a
                // read-only batch.
                let skip = if stop.is_some() {
                    Some("Not run: the turn was stopped by a hook.")
                } else if cx.cancel.is_cancelled() {
                    Some("Not run: the user cancelled the turn.")
                } else {
                    None
                };
                if let Some(why) = skip {
                    prepared.push(Prepared::Done {
                        call,
                        output: ToolOutput::error(why),
                        denied: true,
                        notes: Vec::new(),
                    });
                    continue;
                }
                let (p, s) = self.prepare_call(call, cx).await;
                if stop.is_none() {
                    stop = s;
                }
                prepared.push(p);
            }

            let futs = prepared.into_iter().map(|p| {
                let sem = sem.clone();
                let mut cx = cx.clone();
                async move {
                    match p {
                        Prepared::Done {
                            call,
                            output,
                            denied,
                            notes,
                        } => (call, output, denied, notes, 0u64),
                        Prepared::Run { call, tool, notes } => {
                            let _permit = sem.acquire_owned().await;
                            cx.tool_use_id = Some(call.id.clone());
                            let started = Instant::now();
                            let output = execute_tool(tool, call.input.clone(), cx).await;
                            let ms = started.elapsed().as_millis() as u64;
                            (call, output, false, notes, ms)
                        }
                    }
                }
            });
            let done = join_all(futs).await;

            for (call, mut output, denied, pre_notes, ms) in done {
                output.content =
                    truncate_middle(&output.content, self.kcfg.agent.tool_result_max_chars);
                let mut ev = HookEvent::PostTool {
                    call: call.clone(),
                    output,
                    denied,
                };
                let outcome = run_hooks(&self.hooks, &mut ev, cx).await;
                let HookEvent::PostTool { mut output, .. } = ev else {
                    unreachable!("hooks cannot change the event kind")
                };
                if let Verdict::Stop { hook, reason } = &outcome.verdict
                    && stop.is_none()
                {
                    stop = Some((hook.clone(), reason.clone()));
                }
                self.emit_notes(&outcome);
                for (_, note) in pre_notes.iter().chain(&outcome.notes) {
                    if !output.content.is_empty() {
                        output.content.push_str("\n\n");
                    }
                    output.content.push_str(note);
                }
                self.emit_tool_end(&call, &output, ms);
                results.push(tool_result(&call.id, output));
            }
        }
        (results, stop)
    }

    /// Runs pre_tool hooks and approval for one call.
    async fn prepare_call(
        &mut self,
        call: ToolCall,
        cx: &Cx,
    ) -> (Prepared, Option<(String, String)>) {
        let Some(tool) = self.tools.get(&call.name).cloned() else {
            let mut names: Vec<&str> = self.tools.keys().map(String::as_str).collect();
            names.sort_unstable();
            let output = ToolOutput::error(format!(
                "Unknown tool `{}`. Available tools are: {}.",
                call.name,
                names.join(", ")
            ));
            self.events.emit(AgentEvent::ToolCallStart {
                id: call.id.clone(),
                name: call.name.clone(),
                input: call.input.clone(),
                summary: String::new(),
            });
            return (
                Prepared::Done {
                    call,
                    output,
                    denied: false,
                    notes: Vec::new(),
                },
                None,
            );
        };
        let spec = tool.spec();
        let mut ev = HookEvent::PreTool {
            call,
            read_only: spec.read_only,
        };
        let outcome = run_hooks(&self.hooks, &mut ev, cx).await;
        self.emit_notes(&outcome);
        let HookEvent::PreTool { call, .. } = ev else {
            unreachable!("hooks cannot change the event kind")
        };
        let summary = tool.summarize(&call.input);
        self.events.emit(AgentEvent::ToolCallStart {
            id: call.id.clone(),
            name: call.name.clone(),
            input: call.input.clone(),
            summary: summary.clone(),
        });
        let notes = outcome.notes.clone();
        let deny = |call: ToolCall, msg: String, notes| Prepared::Done {
            call,
            output: ToolOutput::error(msg),
            denied: true,
            notes,
        };
        match outcome.verdict {
            Verdict::Continue => (Prepared::Run { call, tool, notes }, None),
            Verdict::Deny { hook, reason } => {
                self.emit_denied(&hook, Some(&call.name), &reason);
                let msg = format!("Permission denied ({hook}): {reason}");
                (deny(call, msg, notes), None)
            }
            Verdict::Stop { hook, reason } => {
                let msg = format!("Not run: {reason}");
                (deny(call, msg, notes), Some((hook, reason)))
            }
            Verdict::Ask { reason, key, .. } => {
                if key.as_ref().is_some_and(|k| self.always_allow.contains(k)) {
                    return (Prepared::Run { call, tool, notes }, None);
                }
                let Some(approver) = self.approver.clone() else {
                    let msg = format!(
                        "Not run: this call needs approval ({reason}), but no one can approve \
                         it in this mode. Use a different approach that doesn't need approval, \
                         or stop and explain what you need."
                    );
                    return (deny(call, msg, notes), None);
                };
                let req = ApprovalRequest {
                    tool: call.name.clone(),
                    input: call.input.clone(),
                    summary,
                    reason: reason.clone(),
                    key: key.clone(),
                };
                self.events.emit(AgentEvent::ApprovalRequested {
                    tool: req.tool.clone(),
                    summary: req.summary.clone(),
                    reason: reason.clone(),
                });
                let answer = tokio::select! {
                    biased;
                    _ = cx.cancel.cancelled() => Approval::Deny { reason: Some("turn cancelled".into()) },
                    a = approver.ask(&req) => a,
                };
                let approved = !matches!(answer, Approval::Deny { .. });
                self.events.emit(AgentEvent::ApprovalResolved {
                    tool: call.name.clone(),
                    approved,
                });
                match answer {
                    Approval::Allow => (Prepared::Run { call, tool, notes }, None),
                    Approval::AllowAlways => {
                        if let Some(k) = key {
                            self.always_allow.insert(k);
                        }
                        (Prepared::Run { call, tool, notes }, None)
                    }
                    Approval::Deny { reason } => {
                        let msg = match reason {
                            Some(r) => format!(
                                "The user denied this tool call: {r}. Don't retry the same call; \
                                 adjust your approach or ask the user."
                            ),
                            None => "The user denied this tool call. Don't retry the same call; \
                                     adjust your approach or ask the user."
                                .to_string(),
                        };
                        (deny(call, msg, notes), None)
                    }
                }
            }
        }
    }

    fn emit_tool_end(&self, call: &ToolCall, output: &ToolOutput, duration_ms: u64) {
        self.events.emit(AgentEvent::ToolCallEnd {
            id: call.id.clone(),
            name: call.name.clone(),
            content: output.content.clone(),
            is_error: output.is_error,
            duration_ms,
            metadata: output.metadata.clone(),
        });
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

    /// The approver in use, if any.
    pub fn approver(&self) -> Option<&Arc<dyn Approver>> {
        self.approver.as_ref()
    }

    /// Hooks in execution order.
    pub fn hooks(&self) -> &[Arc<dyn Hook>] {
        &self.hooks
    }
}

fn tool_result(id: &str, output: ToolOutput) -> ContentBlock {
    ContentBlock::ToolResult {
        tool_use_id: id.to_string(),
        content: output.content,
        is_error: output.is_error,
    }
}

fn estimate_request_tokens(req: &ModelRequest) -> u64 {
    let chars = req.system.iter().map(String::len).sum::<usize>()
        + req.messages.iter().map(Message::char_len).sum::<usize>()
        + req
            .tools
            .iter()
            .map(|t| t.name.len() + t.description.len() + t.input_schema.to_string().len())
            .sum::<usize>();
    estimate_tokens(chars)
}

/// Runs a tool in its own task so a panic becomes an error observation. On cancellation
/// the tool gets a short grace period (it sees `cx.cancel`) before it is aborted.
async fn execute_tool(tool: Arc<dyn Tool>, input: serde_json::Value, cx: Cx) -> ToolOutput {
    let cancel = cx.cancel.clone();
    let name = tool.spec().name;
    let mut handle = tokio::spawn(async move { tool.call(input, &cx).await });
    let joined = tokio::select! {
        r = &mut handle => r,
        _ = cancel.cancelled() => {
            match tokio::time::timeout(CANCEL_GRACE, &mut handle).await {
                Ok(r) => r,
                Err(_) => {
                    handle.abort();
                    return ToolOutput::error("Cancelled by the user.");
                }
            }
        }
    };
    match joined {
        Ok(out) => out,
        Err(e) if e.is_panic() => {
            let payload = e.into_panic();
            let msg = payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".into());
            tracing::error!(tool = %name, "tool panicked: {msg}");
            ToolOutput::error(format!(
                "Internal error: tool `{name}` crashed ({msg}). Try a different approach."
            ))
        }
        Err(_) => ToolOutput::error("Cancelled by the user."),
    }
}
