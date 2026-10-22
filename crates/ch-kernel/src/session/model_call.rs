//! Building a model request and streaming the response.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use futures::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::context::Budget;
use crate::cx::Cx;
use crate::events::AgentEvent;
use crate::hook::{HookEvent, Verdict, run_hooks};
use crate::model::{ModelError, ModelEvent, ModelRequest, ModelResponse};
use crate::tokens::estimate_tokens;
use crate::tool::ToolSpec;
use crate::types::{ContentBlock, Message, Role, TerminationReason, Usage};
use crate::util::system_reminder;

use super::Session;

/// Minimum time between `ToolInputProgress` events for one tool call.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);

impl Session {
    pub(super) async fn system_blocks(&self, cx: &Cx) -> Vec<String> {
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

    pub(super) fn budget(&self, system: &[String], reminders: &[String]) -> Budget {
        let fixed_chars: usize = system.iter().map(String::len).sum::<usize>()
            + reminders.iter().map(String::len).sum::<usize>()
            + specs_chars(&self.tool_specs);
        Budget {
            window: self.context_window(),
            reserved_output: u64::from(self.kcfg.model.max_output_tokens),
            fixed_tokens: self.calibrator.calibrate(estimate_tokens(fixed_chars)),
            calibrator: self.calibrator,
        }
    }

    /// One model call. `Err` is how the turn ends instead.
    pub(super) async fn call_model(
        &mut self,
        cx: &Cx,
        step: u32,
    ) -> Result<ModelResponse, TerminationReason> {
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
            .with_context(|| format!("context strategy `{}` failed", self.context.id()))
            .map_err(|e| TerminationReason::Error {
                message: format!("{e:#}"),
            })?;
        let mut messages = view.messages;
        let volatile_tail = reminders.len();
        if !reminders.is_empty() {
            let blocks = reminders
                .iter()
                .map(|r| ContentBlock::text(system_reminder(r)));
            append_to_last_user(&mut messages, blocks.collect());
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
        if let Verdict::Deny { hook, reason } | Verdict::Stop { hook, reason } = outcome.verdict {
            return Err(TerminationReason::HookStop {
                source: hook,
                reason,
            });
        }
        if let Some(notes) = outcome.notes_text() {
            let block = ContentBlock::text(system_reminder(&notes));
            append_to_last_user(&mut request.messages, vec![block]);
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

        let response = self
            .stream_model(request, cancel)
            .await
            .map_err(|e| match e {
                ModelError::Cancelled => TerminationReason::Cancelled,
                ModelError::ContextOverflow(message) => {
                    TerminationReason::ContextOverflow { message }
                }
                e => TerminationReason::Error {
                    message: e.to_string(),
                },
            })?;

        let actual = response.usage.prompt_tokens();
        self.calibrator.observe(raw_est, actual);
        self.record_usage(&response.usage);
        Ok(response)
    }

    /// Records usage and cost from any model call (plugins making their own calls,
    /// like summaries, should report through events; the kernel only counts its own).
    fn record_usage(&mut self, usage: &Usage) {
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
            purpose: "agent".into(),
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
}

/// Adds per-call blocks to the last message if it is the user's, or as a new user
/// message after the assistant's.
fn append_to_last_user(messages: &mut Vec<Message>, blocks: Vec<ContentBlock>) {
    match messages.last_mut() {
        Some(last) if last.role == Role::User => last.content.extend(blocks),
        _ => messages.push(Message {
            role: Role::User,
            content: blocks,
        }),
    }
}

/// What the tool definitions add to every request.
fn specs_chars(specs: &[ToolSpec]) -> usize {
    specs
        .iter()
        .map(|t| t.name.len() + t.description.len() + t.input_schema.to_string().len())
        .sum()
}

fn estimate_request_tokens(req: &ModelRequest) -> u64 {
    let chars = req.system.iter().map(String::len).sum::<usize>()
        + req.messages.iter().map(Message::char_len).sum::<usize>()
        + specs_chars(&req.tools);
    estimate_tokens(chars)
}
