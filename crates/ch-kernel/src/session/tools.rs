use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::future::join_all;
use tokio::sync::Semaphore;

use crate::approver::{Approval, ApprovalRequest};
use crate::cx::Cx;
use crate::events::AgentEvent;
use crate::hook::{HookEvent, Verdict, run_hooks};
use crate::tool::{Tool, ToolOutput};
use crate::types::{ContentBlock, ToolCall};
use crate::util::truncate_middle;

use super::Session;

/// How long a cancelled tool gets to wind down before it is aborted.
const CANCEL_GRACE: Duration = Duration::from_secs(3);

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
        not_run: bool,
        notes: Vec<(String, String)>,
    },
}

impl Session {
    /// Runs all tool calls from one assistant message. Returns one `ToolResult` per call,
    /// in call order, and the first stop request from a hook.
    pub(super) async fn run_tools(
        &mut self,
        calls: Vec<ToolCall>,
        cx: &Cx,
    ) -> (Vec<ContentBlock>, Option<(String, String)>) {
        let mut results = Vec::with_capacity(calls.len());
        let mut stop: Option<(String, String)> = None;

        let mut batches: Vec<(bool, Vec<ToolCall>)> = Vec::new();
        for call in calls {
            let ro = self
                .tools
                .get(&call.name)
                .is_some_and(|t| t.spec().read_only);
            match batches.last_mut() {
                Some((true, b)) if ro => b.push(call),
                _ => batches.push((ro, vec![call])),
            }
        }

        let sem = Arc::new(Semaphore::new(self.kcfg.agent.max_parallel_readonly.max(1)));
        for (_, batch) in batches {
            let mut prepared = Vec::with_capacity(batch.len());
            for call in batch {
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
                        not_run: true,
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
                            not_run,
                            notes,
                        } => (call, output, not_run, notes, 0u64),
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

            for (call, mut output, not_run, pre_notes, ms) in done {
                output.content =
                    truncate_middle(&output.content, self.kcfg.agent.tool_result_max_chars);
                let mut ev = HookEvent::PostTool {
                    call: call.clone(),
                    output,
                    not_run,
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
                    not_run: false,
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
            not_run: true,
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
}

pub(super) fn tool_result(id: &str, output: ToolOutput) -> ContentBlock {
    ContentBlock::ToolResult {
        tool_use_id: id.to_string(),
        content: output.content,
        is_error: output.is_error,
    }
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
