//! Turns agent events into terminal actions.
//!
//! [`TurnView`] is a pure state machine: it never touches the terminal, it returns
//! [`Action`]s that the terminal layer carries out. That keeps the rendering rules
//! testable.

use ch_kernel::{AgentEvent, CompactionKind};

use crate::format::{dim, first_line, red, tool_end_lines, tool_start_line, yellow};
use crate::markdown::MarkdownLines;

/// Something the terminal layer should do.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Write this text (it may contain newlines). Stops the spinner first.
    Write(String),
    /// The current line was written raw as `raw` and is now complete: redraw it as
    /// `styled`, then end the line.
    Restyle { raw: String, styled: String },
    /// Show the spinner with this message, or hide it.
    Spinner(Option<String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Last {
    Nothing,
    Text,
    Tool,
}

/// Rendering state for one turn.
pub struct TurnView {
    md: MarkdownLines,
    /// Raw text of the current, incomplete line of assistant text.
    line: String,
    /// Assistant text is streaming.
    in_text: bool,
    last: Last,
    /// Tool calls started and not yet ended: `(id, name)`.
    running: Vec<(String, String)>,
    /// The id of the tool whose start line is the last thing printed.
    last_started: Option<String>,
}

impl Default for TurnView {
    fn default() -> Self {
        Self::new()
    }
}

impl TurnView {
    pub fn new() -> Self {
        Self {
            md: MarkdownLines::new(),
            line: String::new(),
            in_text: false,
            last: Last::Nothing,
            running: Vec::new(),
            last_started: None,
        }
    }

    /// Actions for one event.
    pub fn handle(&mut self, event: &AgentEvent) -> Vec<Action> {
        let mut out = Vec::new();
        match event {
            AgentEvent::TurnStart { .. } | AgentEvent::ModelRequest { .. } => {
                if !self.in_text {
                    out.push(Action::Spinner(Some("thinking".into())));
                }
            }
            AgentEvent::TextDelta { text } => self.text(text, &mut out),
            // The model is writing a tool call. Big inputs (a whole file) take a while,
            // so show what it's preparing and how much has arrived.
            AgentEvent::ToolInputStarted { name, .. } => {
                self.end_text(&mut out);
                out.push(Action::Spinner(Some(format!("preparing {name}"))));
            }
            AgentEvent::ToolInputProgress { name, bytes, .. } => {
                self.end_text(&mut out);
                out.push(Action::Spinner(Some(format!(
                    "preparing {name} · {}",
                    crate::format::fmt_bytes(*bytes)
                ))));
            }
            AgentEvent::AssistantMessage { .. } => self.end_text(&mut out),
            AgentEvent::ToolCallStart {
                id, name, summary, ..
            } => {
                self.end_text(&mut out);
                if self.last == Last::Text {
                    out.push(Action::Write("\n".into()));
                }
                out.push(Action::Write(format!(
                    "{}\n",
                    tool_start_line(name, summary)
                )));
                self.running.push((id.clone(), name.clone()));
                self.last_started = Some(id.clone());
                self.last = Last::Tool;
                out.push(Action::Spinner(Some(format!("running {name}"))));
            }
            AgentEvent::ToolCallEnd {
                id,
                name,
                content,
                is_error,
                metadata,
                ..
            } => {
                self.end_text(&mut out);
                let header =
                    (self.last_started.as_deref() != Some(id.as_str())).then_some(name.as_str());
                let lines = tool_end_lines(name, content, *is_error, metadata, header);
                out.push(Action::Write(format!("{}\n", lines.join("\n"))));
                self.running.retain(|(i, _)| i != id);
                self.last_started = None;
                self.last = Last::Tool;
                out.push(Action::Spinner(Some(self.waiting_message())));
            }
            AgentEvent::ApprovalResolved { .. } => {
                // The approval prompt was printed after the start line.
                self.last_started = None;
                out.push(Action::Spinner(Some(self.waiting_message())));
            }
            AgentEvent::Note { source, text } => {
                self.end_text(&mut out);
                out.push(Action::Write(format!("{}\n", note_line(source, text))));
                self.last_started = None;
            }
            AgentEvent::HookDenied {
                hook,
                tool: None,
                reason,
            } => {
                self.end_text(&mut out);
                out.push(Action::Write(format!(
                    "{}\n",
                    red(&format!("  ⎿ blocked by {hook}: {}", first_line(reason)))
                )));
            }
            AgentEvent::Warning { message } => {
                self.end_text(&mut out);
                out.push(Action::Write(format!(
                    "{}\n",
                    yellow(&format!("⚠ {}", first_line(message)))
                )));
                self.last_started = None;
            }
            AgentEvent::ContextCompacted {
                kind,
                tokens_before,
                tokens_after,
            } => {
                self.end_text(&mut out);
                let what = match kind {
                    CompactionKind::Elided => "old tool output elided",
                    CompactionKind::Summarized => "older turns summarized",
                };
                out.push(Action::Write(format!(
                    "{}\n",
                    dim(&format!(
                        "  · context: {what} ({} → {} tokens)",
                        crate::format::fmt_tokens(*tokens_before),
                        crate::format::fmt_tokens(*tokens_after)
                    ))
                )));
                self.last_started = None;
            }
            _ => {}
        }
        out
    }

    /// Ends the turn: flushes any partial line and hides the spinner.
    pub fn finish(&mut self) -> Vec<Action> {
        let mut out = Vec::new();
        self.end_text(&mut out);
        out.push(Action::Spinner(None));
        out
    }

    /// Before printing something else in the middle of a turn (an approval prompt):
    /// flushes partial text and hides the spinner.
    pub fn interrupt_output(&mut self) -> Vec<Action> {
        self.finish()
    }

    fn waiting_message(&self) -> String {
        match self.running.last() {
            Some((_, name)) => format!("running {name}"),
            None => "thinking".into(),
        }
    }

    fn text(&mut self, text: &str, out: &mut Vec<Action>) {
        if text.is_empty() {
            return;
        }
        if !self.in_text {
            self.in_text = true;
            out.push(Action::Spinner(None));
            if self.last != Last::Nothing {
                out.push(Action::Write("\n".into()));
            }
            self.last = Last::Text;
            self.last_started = None;
        }
        for (i, piece) in text.split('\n').enumerate() {
            if i > 0 {
                self.complete_line(out);
            }
            if !piece.is_empty() {
                self.line.push_str(piece);
                out.push(Action::Write(piece.to_string()));
            }
        }
    }

    fn complete_line(&mut self, out: &mut Vec<Action>) {
        let raw = std::mem::take(&mut self.line);
        match self.md.render(&raw) {
            // Tabs make the on-screen width unknowable; leave such lines raw.
            Some(styled) if !raw.contains('\t') => out.push(Action::Restyle { raw, styled }),
            _ => out.push(Action::Write("\n".into())),
        }
    }

    fn end_text(&mut self, out: &mut Vec<Action>) {
        if !self.in_text {
            return;
        }
        if !self.line.is_empty() {
            self.complete_line(out);
        }
        self.in_text = false;
        self.md.reset();
    }
}

fn note_line(source: &str, text: &str) -> String {
    let lines: Vec<&str> = text.trim().lines().collect();
    let first = lines.first().copied().unwrap_or("");
    let more = if lines.len() > 1 {
        format!(" (+{} lines)", lines.len() - 1)
    } else {
        String::new()
    };
    dim(&format!("  · {source}: {first}{more}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::strip_ansi;
    use ch_kernel::Message;
    use serde_json::{Value, json};

    /// What the user would see, ignoring the spinner and styles.
    fn screen(actions: &[Action]) -> String {
        let mut s = String::new();
        for a in actions {
            match a {
                Action::Write(t) => s.push_str(&strip_ansi(t)),
                Action::Restyle { raw, styled } => {
                    let cut = s.len() - raw.len();
                    s.truncate(cut);
                    s.push_str(&strip_ansi(styled));
                    s.push('\n');
                }
                Action::Spinner(_) => {}
            }
        }
        s
    }

    fn run(events: &[AgentEvent]) -> Vec<Action> {
        let mut v = TurnView::new();
        let mut out: Vec<Action> = events.iter().flat_map(|e| v.handle(e)).collect();
        out.extend(v.finish());
        out
    }

    fn delta(t: &str) -> AgentEvent {
        AgentEvent::TextDelta { text: t.into() }
    }

    fn assistant() -> AgentEvent {
        AgentEvent::AssistantMessage {
            turn: 1,
            step: 0,
            message: Message::assistant(""),
        }
    }

    fn start(id: &str, name: &str, summary: &str) -> AgentEvent {
        AgentEvent::ToolCallStart {
            id: id.into(),
            name: name.into(),
            input: json!({}),
            summary: summary.into(),
        }
    }

    fn end(id: &str, name: &str, content: &str) -> AgentEvent {
        AgentEvent::ToolCallEnd {
            id: id.into(),
            name: name.into(),
            content: content.into(),
            is_error: false,
            duration_ms: 1,
            metadata: Value::Null,
        }
    }

    #[test]
    fn streams_text_and_restyles_markdown_lines() {
        let actions = run(&[delta("Hello **wor"), delta("ld**\nplain"), assistant()]);
        assert_eq!(screen(&actions), "Hello world\nplain\n");
        assert!(actions.iter().any(|a| matches!(a, Action::Restyle { .. })));
    }

    #[test]
    fn text_then_tools_then_text() {
        let actions = run(&[
            AgentEvent::TurnStart {
                turn: 1,
                prompt: "x".into(),
            },
            delta("Checking."),
            assistant(),
            start("1", "read_file", "src/main.rs"),
            end("1", "read_file", "a\nb"),
            delta("Done."),
            assistant(),
        ]);
        assert_eq!(
            screen(&actions),
            "Checking.\n\n● read_file src/main.rs\n  ⎿ 2 lines\n\nDone.\n"
        );
        assert_eq!(
            actions.first(),
            Some(&Action::Spinner(Some("thinking".into())))
        );
        assert_eq!(actions.last(), Some(&Action::Spinner(None)));
    }

    #[test]
    fn parallel_calls_repeat_the_tool_name() {
        let actions = run(&[
            start("1", "read_file", "a.rs"),
            start("2", "read_file", "b.rs"),
            end("2", "read_file", "x"),
            end("1", "read_file", "x\ny"),
        ]);
        assert_eq!(
            screen(&actions),
            "● read_file a.rs\n● read_file b.rs\n  ⎿ 1 line\n  ⎿ read_file: 2 lines\n"
        );
    }

    #[test]
    fn spinner_tracks_running_tools() {
        let mut v = TurnView::new();
        let a = v.handle(&start("1", "bash", "sleep 1"));
        assert_eq!(
            a.last(),
            Some(&Action::Spinner(Some("running bash".into())))
        );
        let a = v.handle(&end("1", "bash", ""));
        assert_eq!(a.last(), Some(&Action::Spinner(Some("thinking".into()))));
    }

    #[test]
    fn spinner_shows_tool_input_progress() {
        let mut v = TurnView::new();
        let text = v.handle(&delta("Writing the guide now."));
        assert!(!text.iter().any(|x| matches!(x, Action::Spinner(Some(_)))));
        // The text line is ended and the spinner says what's being prepared.
        let a = v.handle(&AgentEvent::ToolInputStarted {
            id: "t1".into(),
            name: "write_file".into(),
        });
        assert_eq!(
            screen(&[text, a.clone()].concat()),
            "Writing the guide now.\n"
        );
        assert_eq!(
            a.last(),
            Some(&Action::Spinner(Some("preparing write_file".into())))
        );
        let a = v.handle(&AgentEvent::ToolInputProgress {
            id: "t1".into(),
            name: "write_file".into(),
            bytes: 49_357,
        });
        assert_eq!(
            a,
            vec![Action::Spinner(Some(
                "preparing write_file · 48.2 KB".into()
            ))]
        );
    }

    #[test]
    fn notes_and_warnings_are_single_lines() {
        let actions = run(&[
            AgentEvent::Note {
                source: "diagnostics".into(),
                text: "2 warnings\nline 2\nline 3".into(),
            },
            AgentEvent::Warning {
                message: "could not save".into(),
            },
        ]);
        assert_eq!(
            screen(&actions),
            "  · diagnostics: 2 warnings (+2 lines)\n⚠ could not save\n"
        );
    }

    #[test]
    fn partial_lines_are_flushed_on_finish() {
        let actions = run(&[delta("no newline")]);
        assert_eq!(screen(&actions), "no newline\n");
    }

    #[test]
    fn fenced_code_spans_deltas() {
        let actions = run(&[delta("```\n- x\n"), delta("```\n- y\n"), assistant()]);
        assert_eq!(screen(&actions), "```\n- x\n```\n• y\n");
    }
}
