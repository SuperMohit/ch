//! Running one headless turn and reporting it.

use std::io::Write;
use std::time::Instant;

use ch_kernel::{AgentEvent, Session, TerminationReason, Usage};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::Receiver;
use tokio::sync::broadcast::error::{RecvError, TryRecvError};

use crate::render::verbose_line;
use crate::{OutputFormat, PrintOptions};

/// The summary printed at the end in `json` and `stream-json` modes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PrintResult {
    /// Always `"result"`, so stream-json readers can tell it from events.
    #[serde(rename = "type")]
    pub kind: String,
    pub session_id: String,
    /// `done`, `error`, `step_limit`, `context_overflow`, `hook_stop`, `cancelled` or
    /// `prompt_denied`.
    pub reason: String,
    /// The error message or stop reason, when there is one.
    pub detail: Option<String>,
    /// The process exit code for `reason` (see [`exit_code`]).
    pub exit_code: i32,
    /// Text of the last assistant message.
    pub final_text: String,
    /// Model calls made.
    pub steps: u32,
    /// Tokens used by this run (not earlier turns of a resumed session).
    pub usage: Usage,
    /// Cost of this run in USD (0 when pricing isn't configured).
    pub cost: f64,
    pub duration_ms: u64,
}

/// Process exit code for a termination reason.
///
/// done 0, error 1, step_limit 2, context_overflow 3, hook_stop 4, prompt_denied 5,
/// cancelled 130.
pub fn exit_code(reason: &TerminationReason) -> i32 {
    match reason {
        TerminationReason::Done => 0,
        TerminationReason::Error { .. } => 1,
        TerminationReason::StepLimit => 2,
        TerminationReason::ContextOverflow { .. } => 3,
        TerminationReason::HookStop { .. } => 4,
        TerminationReason::PromptDenied { .. } => 5,
        TerminationReason::Cancelled => 130,
    }
}

fn reason_detail(reason: &TerminationReason) -> Option<String> {
    match reason {
        TerminationReason::Done | TerminationReason::StepLimit | TerminationReason::Cancelled => {
            None
        }
        TerminationReason::ContextOverflow { message } | TerminationReason::Error { message } => {
            Some(message.clone())
        }
        TerminationReason::HookStop { source, reason } => Some(format!("{source}: {reason}")),
        TerminationReason::PromptDenied { reason } => Some(reason.clone()),
    }
}

/// Writes events in the chosen format. Write errors (a closed pipe, say) are ignored so
/// the run still finishes and the session is saved.
struct Printer<'a, O, E> {
    format: OutputFormat,
    verbose: bool,
    out: &'a mut O,
    err: &'a mut E,
    /// Some assistant text has been written.
    wrote_text: bool,
    /// The last character written to `out` was a newline.
    at_line_start: bool,
    /// The current assistant message has ended; the next text starts a new paragraph.
    block_ended: bool,
}

impl<O: Write, E: Write> Printer<'_, O, E> {
    fn event(&mut self, event: &AgentEvent) {
        match self.format {
            OutputFormat::StreamJson => {
                if let Ok(line) = serde_json::to_string(event) {
                    let _ = writeln!(self.out, "{line}");
                    let _ = self.out.flush();
                }
            }
            OutputFormat::Json => {}
            OutputFormat::Text => self.text_event(event),
        }
    }

    fn text_event(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::TextDelta { text } if !text.is_empty() => {
                if self.block_ended && self.wrote_text {
                    let sep = if self.at_line_start { "\n" } else { "\n\n" };
                    let _ = self.out.write_all(sep.as_bytes());
                }
                self.block_ended = false;
                let _ = self.out.write_all(text.as_bytes());
                let _ = self.out.flush();
                self.wrote_text = true;
                self.at_line_start = text.ends_with('\n');
            }
            AgentEvent::AssistantMessage { .. } => self.block_ended = true,
            AgentEvent::Warning { message } => {
                let _ = writeln!(self.err, "warning: {message}");
            }
            other if self.verbose => {
                if let Some(line) = verbose_line(other) {
                    // On a shared terminal, start tool lines on a fresh line.
                    if self.wrote_text && !self.at_line_start {
                        let _ = self.out.write_all(b"\n");
                        let _ = self.out.flush();
                        self.at_line_start = true;
                    }
                    let _ = writeln!(self.err, "{line}");
                }
            }
            _ => {}
        }
    }

    fn finish(&mut self, result: &PrintResult) {
        match self.format {
            OutputFormat::Text => {
                if self.wrote_text && !self.at_line_start {
                    let _ = self.out.write_all(b"\n");
                }
                if result.reason != "done" {
                    let detail = result
                        .detail
                        .as_deref()
                        .map(|d| format!(": {d}"))
                        .unwrap_or_default();
                    let _ = writeln!(self.err, "ch: stopped ({}){detail}", result.reason);
                }
            }
            OutputFormat::Json | OutputFormat::StreamJson => {
                if let Ok(line) = serde_json::to_string(result) {
                    let _ = writeln!(self.out, "{line}");
                }
            }
        }
        let _ = self.out.flush();
        let _ = self.err.flush();
    }

    fn drain(&mut self, rx: &mut Receiver<AgentEvent>) {
        loop {
            match rx.try_recv() {
                Ok(ev) => self.event(&ev),
                Err(TryRecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
    }
}

/// Runs `opts.prompt` as one turn, writing output to `out` and `err`, then ends the
/// session. The caller maps the result to an exit code with [`exit_code`].
///
/// This is the whole frontend minus Ctrl-C handling, so tests can capture output.
pub async fn run_with<O, E>(
    mut session: Session,
    opts: &PrintOptions,
    out: &mut O,
    err: &mut E,
) -> anyhow::Result<PrintResult>
where
    O: Write + Send,
    E: Write + Send,
{
    if opts.prompt.trim().is_empty() {
        session.end().await;
        anyhow::bail!("no prompt given: pass it with `ch -p \"<task>\"` or pipe it on stdin");
    }
    let mut printer = Printer {
        format: opts.output_format,
        verbose: opts.verbose,
        out,
        err,
        wrote_text: false,
        at_line_start: true,
        block_ended: false,
    };
    let mut rx = session.events().subscribe();
    let usage_before = session.usage();
    let cost_before = session.cost();
    let started = Instant::now();

    let turn = {
        let fut = session.run_turn(opts.prompt.clone());
        tokio::pin!(fut);
        loop {
            tokio::select! {
                result = &mut fut => break result,
                ev = rx.recv() => match ev {
                    Ok(ev) => printer.event(&ev),
                    Err(RecvError::Lagged(_)) => {}
                    Err(RecvError::Closed) => break fut.await,
                },
            }
        }
    };
    printer.drain(&mut rx);

    let result = PrintResult {
        kind: "result".into(),
        session_id: session.id().to_string(),
        reason: turn.reason.label().to_string(),
        detail: reason_detail(&turn.reason),
        exit_code: exit_code(&turn.reason),
        final_text: turn.final_text.clone(),
        steps: turn.steps,
        usage: session.usage().since(&usage_before),
        cost: (session.cost() - cost_before).max(0.0),
        duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
    };
    session.end().await;
    printer.drain(&mut rx);
    printer.finish(&result);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes() {
        assert_eq!(exit_code(&TerminationReason::Done), 0);
        assert_eq!(
            exit_code(&TerminationReason::Error {
                message: "x".into()
            }),
            1
        );
        assert_eq!(exit_code(&TerminationReason::StepLimit), 2);
        assert_eq!(
            exit_code(&TerminationReason::ContextOverflow {
                message: "x".into()
            }),
            3
        );
        assert_eq!(
            exit_code(&TerminationReason::HookStop {
                source: "s".into(),
                reason: "r".into()
            }),
            4
        );
        assert_eq!(
            exit_code(&TerminationReason::PromptDenied { reason: "r".into() }),
            5
        );
        assert_eq!(exit_code(&TerminationReason::Cancelled), 130);
    }
}
