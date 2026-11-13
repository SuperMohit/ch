use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use futures::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::context::{Budget, CompactReason, CompactRequest, ContextCx, View};
use crate::cx::Cx;
use crate::events::AgentEvent;
use crate::hook::{HookEvent, Verdict, run_hooks};
use crate::ledger::{MAX_LEDGER, Staged, ToolLedger};
use crate::model::{
    ModelError, ModelEvent, ModelRequest, ModelResponse, ToolChanges, estimate_request_tokens,
    specs_chars,
};
use crate::tokens::estimate_tokens;
use crate::tool::ToolSpec;
use crate::types::{ContentBlock, Message, Role, TerminationReason, Usage};
use crate::util::system_reminder;

use super::Session;

/// Minimum time between `ToolInputProgress` events for one tool call.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);

impl Session {
    /// The conversation's system prompt: the one its first request was sent with, or,
    /// before that, what the prompt contributors give now. It is rendered once per
    /// conversation because a model's signed reasoning is checked against it.
    pub(super) async fn system_prompt(&self, cx: &Cx) -> Vec<String> {
        match &self.system {
            Some(system) => system.clone(),
            None => self.render_system(cx).await,
        }
    }

    async fn render_system(&self, cx: &Cx) -> Vec<String> {
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

    pub(super) fn budget(
        &self,
        system: &[String],
        reminders: &[String],
        tools: &[ToolSpec],
    ) -> Budget {
        let fixed_chars: usize = system.iter().map(String::len).sum::<usize>()
            + reminders.iter().map(String::len).sum::<usize>()
            + specs_chars(tools);
        Budget {
            window: self.context_window(),
            reserved_output: u64::from(self.kcfg.model.max_output_tokens),
            fixed_tokens: self.calibrator.calibrate(estimate_tokens(fixed_chars)),
            calibrator: self.calibrator,
        }
    }

    /// The agent's request without its messages, sending `ledger`'s tools.
    pub(super) fn agent_request(
        &self,
        system: Vec<String>,
        ledger: &ToolLedger,
        mode: ToolChanges,
    ) -> ModelRequest {
        // Without native tool changes a model that checks its reasoning must get the
        // definitions it was first sent, withdrawn ones included.
        let tools = match mode {
            ToolChanges::Frozen => ledger.sent_specs(),
            _ => ledger.active_specs(),
        };
        ModelRequest {
            model: self.model.clone(),
            system,
            messages: Vec::new(),
            tools,
            max_tokens: self.kcfg.model.max_output_tokens,
            temperature: self.kcfg.model.temperature,
            thinking_budget: self.kcfg.model.thinking_budget,
            purpose: "agent".into(),
            tool_ledger: (mode == ToolChanges::Native).then(|| ledger.entries().to_vec()),
        }
    }

    /// The tool ledger the next model call sends, with what changed since the last one.
    /// Warns once per tool about what the model isn't told.
    fn stage_tools(&mut self, mode: ToolChanges) -> Staged {
        let (offered, shadowed) = self.catalog.offered();
        for s in shadowed {
            self.warn_once(
                format!("shadowed {} {}", s.name, s.source),
                format!(
                    "`{}` also offers a tool named `{}`; the one registered first keeps the name",
                    s.source, s.name
                ),
            );
        }
        let staged = self.ledger.stage(&offered, mode);
        if mode == ToolChanges::Frozen && staged.ledger.has_deferred() {
            self.warn_once(
                "frozen with deferred".into(),
                format!(
                    "this conversation added tools in a way `{}` can't repeat here; if \
                     requests are rejected, start over with /clear",
                    self.model
                ),
            );
        }
        for name in &staged.changed {
            self.warn_once(
                format!("changed {name}"),
                format!(
                    "`{name}` changed its definition; the model keeps the first one until \
                     /clear or a new session"
                ),
            );
        }
        let waiting: Vec<String> = (staged.waiting.iter())
            .filter(|n| !self.tool_warnings.contains(&format!("waiting {n}")))
            .cloned()
            .collect();
        if !waiting.is_empty() {
            self.tool_warnings
                .extend(waiting.iter().map(|n| format!("waiting {n}")));
            let why = match mode {
                ToolChanges::Frozen => {
                    "this model can't take new tools in a running conversation".to_string()
                }
                _ => format!("the conversation already has {MAX_LEDGER} tool definitions"),
            };
            self.events.emit(AgentEvent::Warning {
                message: format!(
                    "{why}; these tools are available after /clear: {}",
                    waiting.join(", ")
                ),
            });
        }
        staged
    }

    fn warn_once(&mut self, key: String, message: String) {
        if self.tool_warnings.insert(key) {
            self.events.emit(AgentEvent::Warning { message });
        }
    }

    /// Makes `staged` the conversation's tools and `system` its system prompt, once a
    /// call's additions are stored in history.
    fn commit_prompt(&mut self, staged: Staged, system: &[String]) {
        if let Some(ContentBlock::ToolsChanged { added, removed }) = staged.block() {
            self.events
                .emit(AgentEvent::ToolsChanged { added, removed });
        }
        self.ledger = staged.ledger;
        self.tool_specs = Arc::new(self.ledger.active_specs());
        if self.system.is_none() {
            self.system = Some(system.to_vec());
        }
    }

    pub(super) async fn call_model(
        &mut self,
        cx: &Cx,
        step: u32,
        after_model_notes: &mut Vec<String>,
    ) -> Result<ModelResponse, TerminationReason> {
        // The tools this call sends. They become the conversation's only when the call's
        // additions are stored below, so a call that stops early leaves them pending.
        let mode = self.provider.tool_changes(&self.model);
        let staged = self.stage_tools(mode);
        // A conversation's system prompt is rendered with its first request's tools; ones
        // added later are announced, not written into it.
        let system = match &self.system {
            Some(system) => system.clone(),
            None => {
                let first = Cx {
                    tools: Arc::new(staged.ledger.sent_specs()),
                    ..cx.clone()
                };
                self.render_system(&first).await
            }
        };
        let cx = &Cx {
            tools: Arc::new(staged.ledger.active_specs()),
            ..cx.clone()
        };
        let mut reminders = Vec::new();
        for (_, p) in &self.registry.prompts {
            reminders.extend(
                p.turn_reminders(cx)
                    .await
                    .into_iter()
                    .filter(|r| !r.trim().is_empty()),
            );
        }
        reminders.append(after_model_notes);

        let request = self.agent_request(system, &staged.ledger, mode);
        let budget = self.budget(&request.system, &reminders, &sent_tools(&request));
        let ccx = self.context_cx(cx.clone(), request);
        let mut messages = self.view(budget, &ccx).await?.messages;
        let mut added: Vec<ContentBlock> = staged.block().into_iter().collect();
        added.extend(
            reminders
                .iter()
                .map(|r| ContentBlock::text(system_reminder(r))),
        );
        append_to_last_user(&mut messages, added.clone());

        let request = ModelRequest {
            messages,
            ..ccx.agent_request.clone()
        };

        // before_model hooks
        let mut ev = HookEvent::BeforeModel { request };
        let outcome = run_hooks(&self.hooks, &mut ev, cx).await;
        self.emit_notes(&outcome);
        let HookEvent::BeforeModel { mut request } = ev else {
            unreachable!("hooks cannot change the event kind")
        };
        if let Verdict::Deny { hook, reason } | Verdict::Stop { hook, reason } = outcome.verdict {
            return Err(TerminationReason::HookStop {
                source: hook,
                reason,
            });
        }
        if let Some(notes) = outcome.notes_text() {
            let block = ContentBlock::text(system_reminder(&notes));
            append_to_last_user(&mut request.messages, vec![block.clone()]);
            added.push(block);
        }
        // What this call added is stored, not sent once and dropped: with signed
        // thinking, a block is valid only while everything before it stays the same, so
        // text that is in one request and missing from the next invalidates the model's
        // later reasoning. The view ends with the newest history message, so after this
        // the request's last message is the history's. The tools and system prompt
        // the request carries become the conversation's at the same point.
        self.commit_prompt(staged, &ccx.agent_request.system);
        if !added.is_empty() {
            self.history.append_to_last_user(self.turn, added);
        }

        match self.send(request, step, cx).await {
            // Only before any output: what streamed has been shown already.
            Err(Failed {
                error: ModelError::ContextOverflow(message),
                streamed: false,
            }) => self.retry_after_overflow(&ccx, message, step).await,
            sent => sent.map_err(|f| termination(f.error)),
        }
    }

    /// The provider rejected the request as too long: compacts once and, if that changed
    /// anything, sends the request again with a new view. The compaction and the new
    /// view get the same `ccx` as the first view. Only the view is recomputed: the call's
    /// reminders and notes are in the history by now, so the new view holds them, and
    /// asking the prompts and `before_model` hooks again would store them twice. Those
    /// hooks can't change the request, so the retry sends the same system blocks, tools
    /// and settings. A second overflow ends the turn.
    async fn retry_after_overflow(
        &mut self,
        ccx: &ContextCx,
        message: String,
        step: u32,
    ) -> Result<ModelResponse, TerminationReason> {
        let cancel = &ccx.cx.cancel;
        if cancel.is_cancelled() {
            return Err(TerminationReason::Cancelled);
        }
        // The call's reminders are in the history now, not on top of it.
        let budget = self.budget(
            &ccx.agent_request.system,
            &[],
            &sent_tools(&ccx.agent_request),
        );
        let request = CompactRequest {
            reason: CompactReason::Overflow,
            instructions: None,
        };
        let compacted = self
            .context
            .compact(&self.history, budget, &request, ccx)
            .await;
        self.record_context_usage();
        if cancel.is_cancelled() {
            return Err(TerminationReason::Cancelled);
        }
        match compacted {
            Ok(outcome) if outcome.changed => {}
            Ok(_) => return Err(TerminationReason::ContextOverflow { message }),
            Err(e) => {
                self.events.emit(AgentEvent::Warning {
                    message: format!(
                        "context strategy `{}` failed to compact: {e:#}",
                        self.context.id()
                    ),
                });
                return Err(TerminationReason::ContextOverflow { message });
            }
        }
        let messages = self.view(budget, ccx).await?.messages;
        let request = ModelRequest {
            messages,
            ..ccx.agent_request.clone()
        };
        self.send(request, step, &ccx.cx)
            .await
            .map_err(|f| termination(f.error))
    }

    /// The context strategy's view of the history.
    async fn view(&mut self, budget: Budget, ccx: &ContextCx) -> Result<View, TerminationReason> {
        let view = self.context.view(&self.history, budget, ccx).await;
        self.record_context_usage();
        if ccx.cx.cancel.is_cancelled() {
            return Err(TerminationReason::Cancelled);
        }
        view.with_context(|| format!("context strategy `{}` failed", self.context.id()))
            .map_err(|e| TerminationReason::Error {
                message: format!("{e:#}"),
            })
    }

    async fn send(
        &mut self,
        request: ModelRequest,
        step: u32,
        cx: &Cx,
    ) -> Result<ModelResponse, Failed> {
        debug_assert_eq!(
            request.messages.last(),
            self.history.last().map(|e| &e.message),
            "the request must end with the newest history message"
        );
        let raw_est = estimate_request_tokens(&request);
        let (model, purpose) = (request.model.clone(), request.purpose.clone());
        self.events.emit(AgentEvent::ModelRequest {
            turn: self.turn,
            step,
            model: model.clone(),
            purpose: purpose.clone(),
            messages: request.messages.len(),
            tokens_est: self.calibrator.calibrate(raw_est),
            context_window: self.context_window(),
        });

        let response = self.stream_model(request, cx.cancel.clone()).await?;
        let actual = response.usage.prompt_tokens();
        self.calibrator.observe(raw_est, actual);
        self.record_usage(&model, &purpose, &response.usage);
        Ok(response)
    }

    /// Counts the model calls the context strategy made itself, such as summaries.
    pub(super) fn record_context_usage(&mut self) {
        for call in self.context_usage.take() {
            self.record_usage(&call.model, &call.purpose, &call.usage);
        }
    }

    /// Adds a model call's usage and cost to the session's totals.
    fn record_usage(&mut self, model: &str, purpose: &str, usage: &Usage) {
        self.usage.add(usage);
        let cost = self
            .for_configured_model(model, self.kcfg.model.pricing)
            .map(|p| p.cost(usage));
        if let Some(c) = cost {
            self.cost += c;
        }
        self.events.emit(AgentEvent::Usage {
            usage: *usage,
            cost,
            purpose: purpose.into(),
        });
    }

    async fn stream_model(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
    ) -> Result<ModelResponse, Failed> {
        let mut stream = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(ModelError::Cancelled.into()),
            s = self.provider.stream(request, cancel.clone()) => s?,
        };
        // Tool calls whose input is streaming: id → (name, bytes so far, last progress).
        let mut inputs: HashMap<String, (String, u64, Instant)> = HashMap::new();
        // Whether some of the response has reached the event bus.
        let mut streamed = false;
        loop {
            let next = tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    return Err(Failed { error: ModelError::Cancelled, streamed });
                }
                n = stream.next() => n,
            };
            match next {
                None => {
                    let error = ModelError::Other(anyhow::anyhow!(
                        "model stream ended without a final response"
                    ));
                    return Err(Failed { error, streamed });
                }
                Some(Err(error)) => return Err(Failed { error, streamed }),
                Some(Ok(ModelEvent::TextDelta(text))) => {
                    streamed = true;
                    self.events.emit(AgentEvent::TextDelta { text });
                }
                Some(Ok(ModelEvent::ThinkingDelta(text))) => {
                    streamed = true;
                    self.events.emit(AgentEvent::ThinkingDelta { text });
                }
                Some(Ok(ModelEvent::ToolUseStart { id, name })) => {
                    streamed = true;
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
}

/// A failed model call, and whether any of its response had streamed out already.
struct Failed {
    error: ModelError,
    streamed: bool,
}

impl From<ModelError> for Failed {
    fn from(error: ModelError) -> Self {
        Self {
            error,
            streamed: false,
        }
    }
}

/// How a failed agent call ends the turn.
fn termination(e: ModelError) -> TerminationReason {
    match e {
        ModelError::Cancelled => TerminationReason::Cancelled,
        ModelError::ContextOverflow(message) => TerminationReason::ContextOverflow { message },
        e => TerminationReason::Error {
            message: e.to_string(),
        },
    }
}

/// The tool definitions `request` makes the model read: with native tool changes, those
/// of the first request and the deferred ones still active (a deferred tool is loaded
/// only once announced); otherwise what it sends.
pub(super) fn sent_tools(request: &ModelRequest) -> Vec<ToolSpec> {
    match &request.tool_ledger {
        Some(ledger) => (ledger.iter())
            .filter(|e| !e.deferred || e.active)
            .map(|e| e.spec.clone())
            .collect(),
        None => request.tools.clone(),
    }
}

fn append_to_last_user(messages: &mut Vec<Message>, blocks: Vec<ContentBlock>) {
    match messages.last_mut() {
        Some(last) if last.role == Role::User => last.content.extend(blocks),
        _ => messages.push(Message {
            role: Role::User,
            content: blocks,
        }),
    }
}
