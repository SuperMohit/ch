//! Reading the event stream of `ch -p --output-format stream-json`.
//!
//! Lines are read as plain JSON rather than as the kernel's `AgentEvent`, so a run of
//! another `ch` build (`--ch`) with events this one doesn't know is still counted. A
//! line that isn't JSON, such as the last one of a killed run, is skipped and counted.
//!
//! The stream is ch's own account, and the agent can write to it (through
//! `/proc/<pid>/fd/1`), so nothing here is trusted: numbers are clamped to finite,
//! non-negative values and added without overflow, and the termination reason is kept
//! to a short word. Usage and cost that count come from the proxy's ledger instead.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Token counts, as in the kernel's `Usage`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
}

impl Usage {
    /// Adds `other`, saturating.
    pub fn add(&mut self, other: &Usage) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.cache_read_tokens = self
            .cache_read_tokens
            .saturating_add(other.cache_read_tokens);
        self.cache_write_tokens = self
            .cache_write_tokens
            .saturating_add(other.cache_write_tokens);
    }

    /// The kernel's type, for its pricing.
    pub fn to_kernel(self) -> ch_kernel::Usage {
        ch_kernel::Usage {
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cache_read_tokens: self.cache_read_tokens,
            cache_write_tokens: self.cache_write_tokens,
        }
    }
}

/// The final `result` line (the print frontend's `PrintResult`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ResultLine {
    /// `done`, `error`, `step_limit`, `context_overflow`, `hook_stop`, `cancelled` or
    /// `prompt_denied`.
    pub reason: String,
    pub detail: Option<String>,
    pub exit_code: i32,
    pub steps: u32,
    pub usage: Usage,
    pub cost: f64,
    pub duration_ms: u64,
}

/// Counts of what the harness did during a run.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Signals {
    /// Finished tool calls, by tool, denied ones included.
    pub tool_calls: BTreeMap<String, u64>,
    /// Tool calls that ran and ended in an error, by `<tool>:<reason>`. The reason is
    /// the result's `failure` metadata (as `edit_file` sets it), or `error` without one.
    pub tool_errors: BTreeMap<String, u64>,
    /// Tool calls that didn't run: denied by a hook or the permission policy, or
    /// stopped, by tool.
    pub tool_denials: BTreeMap<String, u64>,
    /// `context_compacted` events (trims and summaries).
    pub compactions: u64,
    /// The planning plugin's `plan_refreshed` events.
    pub plan_refreshes: u64,
    /// `hook_denied` events.
    pub hook_denials: u64,
    /// `warning` events.
    pub warnings: u64,
}

impl Signals {
    pub fn tool_call_total(&self) -> u64 {
        sum(&self.tool_calls)
    }

    pub fn tool_error_total(&self) -> u64 {
        sum(&self.tool_errors)
    }

    pub fn tool_denial_total(&self) -> u64 {
        sum(&self.tool_denials)
    }
}

fn sum(map: &BTreeMap<String, u64>) -> u64 {
    map.values().fold(0, |a, b| a.saturating_add(*b))
}

/// What a run's event stream says.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Stream {
    /// The `result` line, missing when the run was killed or `ch` failed to start.
    pub result: Option<ResultLine>,
    pub signals: Signals,
    /// The enabled plugins, from the first `session_start` event.
    pub plugins: Option<Vec<String>>,
    /// Agent model calls (`assistant_message` events).
    pub steps: u32,
    /// The sum of `usage` events, including calls the context strategy made.
    pub usage: Usage,
    /// The sum of the costs in `usage` events.
    pub cost: f64,
    /// Lines that weren't a JSON object.
    pub unreadable_lines: usize,
}

impl Stream {
    /// Steps, tokens and cost as ch reports them: the result line's, or the events'
    /// sums without one.
    pub fn totals(&self) -> (u32, Usage, f64) {
        match &self.result {
            Some(r) => (r.steps, r.usage, clean(r.cost)),
            None => (self.steps, self.usage, self.cost),
        }
    }

    /// The termination reason, if the result line has a plausible one.
    pub fn reason(&self) -> Option<&str> {
        let reason = self.result.as_ref()?.reason.as_str();
        let plausible = (1..=32).contains(&reason.len())
            && reason.chars().all(|c| c.is_ascii_lowercase() || c == '_');
        Some(if plausible { reason } else { "invalid" })
    }
}

/// A cost that can be added up: finite and not negative, or 0.
fn clean(cost: f64) -> f64 {
    crate::ledger::clean_cost(cost)
}

/// The start of a tool result that didn't run (see the kernel's tool dispatch).
const NOT_RUN: &[&str] = &["Permission denied (", "Not run:", "The user denied"];

/// The longest line read from `events.jsonl`; a longer one is skipped as unreadable.
pub const MAX_LINE: usize = 8 * 1024 * 1024;
/// How much of `events.jsonl` is read at most.
pub const MAX_TOTAL: u64 = 256 * 1024 * 1024;

/// Reads an `events.jsonl` held in memory.
pub fn parse(text: &str) -> Stream {
    let mut out = Stream::default();
    for line in text.lines() {
        read_line(&mut out, line);
    }
    out
}

/// Reads an `events.jsonl` file in bounded memory: lines over [`MAX_LINE`] are skipped
/// and counted as unreadable, and nothing past [`MAX_TOTAL`] is read.
pub fn parse_file(path: &std::path::Path) -> Stream {
    use std::io::Read as _;
    let mut out = Stream::default();
    let Ok(file) = std::fs::File::open(path) else {
        return out;
    };
    let mut reader = std::io::BufReader::new(file.take(MAX_TOTAL));
    let mut line = Vec::new();
    while let Ok(Some(too_long)) = next_line(&mut reader, &mut line, MAX_LINE) {
        if too_long {
            out.unreadable_lines += 1;
        } else {
            read_line(&mut out, &String::from_utf8_lossy(&line));
        }
    }
    out
}

/// Reads the next line into `buf`, keeping at most `max` bytes of it. `None` at the
/// end; otherwise whether the line was longer than `max`.
fn next_line(
    reader: &mut impl std::io::BufRead,
    buf: &mut Vec<u8>,
    max: usize,
) -> std::io::Result<Option<bool>> {
    buf.clear();
    let (mut any, mut too_long) = (false, false);
    loop {
        let chunk = reader.fill_buf()?;
        if chunk.is_empty() {
            return Ok(any.then_some(too_long));
        }
        any = true;
        let newline = chunk.iter().position(|&b| b == b'\n');
        let used = newline.map_or(chunk.len(), |i| i + 1);
        let content = &chunk[..newline.unwrap_or(chunk.len())];
        if buf.len() + content.len() > max {
            too_long = true;
        } else if !too_long {
            buf.extend_from_slice(content);
        }
        reader.consume(used);
        if newline.is_some() {
            return Ok(Some(too_long));
        }
    }
}

fn read_line(out: &mut Stream, line: &str) {
    if line.trim().is_empty() {
        return;
    }
    let Ok(Value::Object(event)) = serde_json::from_str::<Value>(line) else {
        out.unreadable_lines += 1;
        return;
    };
    let str_field = |k: &str| event.get(k).and_then(Value::as_str).unwrap_or("");
    let bump = |map: &mut BTreeMap<String, u64>, key: String| {
        let n = map.entry(key).or_default();
        *n = n.saturating_add(1);
    };
    match str_field("type") {
        "result" => {
            out.result = serde_json::from_value(Value::Object(event.clone())).ok();
        }
        "session_start" if out.plugins.is_none() => {
            out.plugins = event
                .get("plugins")
                .and_then(|p| serde_json::from_value(p.clone()).ok());
        }
        "tool_call_end" => {
            let tool = str_field("name").to_string();
            bump(&mut out.signals.tool_calls, tool.clone());
            if event.get("is_error").and_then(Value::as_bool) == Some(true) {
                let failure = event
                    .get("metadata")
                    .and_then(|m| m.get("failure"))
                    .and_then(Value::as_str);
                let content = str_field("content");
                if failure.is_none() && NOT_RUN.iter().any(|p| content.starts_with(p)) {
                    bump(&mut out.signals.tool_denials, tool);
                } else {
                    let reason = failure.unwrap_or("error");
                    bump(&mut out.signals.tool_errors, format!("{tool}:{reason}"));
                }
            }
        }
        "context_compacted" => out.signals.compactions = out.signals.compactions.saturating_add(1),
        "hook_denied" => out.signals.hook_denials = out.signals.hook_denials.saturating_add(1),
        "warning" => out.signals.warnings = out.signals.warnings.saturating_add(1),
        "custom" if str_field("source") == "planning" && str_field("name") == "plan_refreshed" => {
            out.signals.plan_refreshes = out.signals.plan_refreshes.saturating_add(1);
        }
        "assistant_message" => out.steps = out.steps.saturating_add(1),
        "usage" => {
            if let Some(u) = event
                .get("usage")
                .and_then(|u| serde_json::from_value::<Usage>(u.clone()).ok())
            {
                out.usage.add(&u);
            }
            let cost = clean(event.get("cost").and_then(Value::as_f64).unwrap_or(0.0));
            out.cost = (out.cost + cost).min(f64::MAX);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use ch_kernel::{AgentEvent, CompactionKind, Message, TerminationReason};
    use serde_json::json;

    use super::*;

    /// Lines as the kernel serializes them, so a renamed event or field fails here.
    fn lines(events: &[AgentEvent]) -> String {
        events
            .iter()
            .map(|e| serde_json::to_string(e).unwrap() + "\n")
            .collect()
    }

    fn tool_end(name: &str, is_error: bool, metadata: Value) -> AgentEvent {
        AgentEvent::ToolCallEnd {
            id: "c".into(),
            name: name.into(),
            content: "out".into(),
            is_error,
            duration_ms: 3,
            metadata,
        }
    }

    fn usage_event(input: u64, output: u64, cost: Option<f64>) -> AgentEvent {
        AgentEvent::Usage {
            usage: ch_kernel::Usage {
                input_tokens: input,
                output_tokens: output,
                cache_read_tokens: 5,
                cache_write_tokens: 0,
            },
            cost,
            purpose: "agent".into(),
        }
    }

    fn assistant() -> AgentEvent {
        AgentEvent::AssistantMessage {
            turn: 1,
            step: 1,
            message: Message::assistant("ok"),
        }
    }

    #[test]
    fn counts_the_signals() {
        let text = lines(&[
            assistant(),
            tool_end("read_file", false, Value::Null),
            tool_end(
                "edit_file",
                true,
                json!({"failure": "not_found", "path": "a"}),
            ),
            tool_end("edit_file", true, json!({"failure": "not_found"})),
            tool_end("edit_file", false, json!({"path": "a", "diff": "..."})),
            tool_end("bash", true, Value::Null),
            AgentEvent::ContextCompacted {
                kind: CompactionKind::Elided,
                tokens_before: 10,
                tokens_after: 5,
            },
            AgentEvent::Custom {
                source: "planning".into(),
                name: "plan_refreshed".into(),
                data: json!({"steps_since": 10}),
            },
            AgentEvent::Custom {
                source: "planning".into(),
                name: "plan_updated".into(),
                data: json!({}),
            },
            AgentEvent::HookDenied {
                hook: "h".into(),
                tool: Some("bash".into()),
                reason: "no".into(),
            },
            AgentEvent::Warning {
                message: "w".into(),
            },
            AgentEvent::TurnEnd {
                turn: 1,
                reason: TerminationReason::Done,
                steps: 1,
            },
        ]);
        let s = parse(&text).signals;
        assert_eq!(
            s.tool_calls,
            BTreeMap::from([
                ("bash".to_string(), 1),
                ("edit_file".to_string(), 3),
                ("read_file".to_string(), 1)
            ])
        );
        assert_eq!(
            s.tool_errors,
            BTreeMap::from([
                ("bash:error".to_string(), 1),
                ("edit_file:not_found".to_string(), 2)
            ])
        );
        assert_eq!(s.tool_call_total(), 5);
        assert_eq!(s.tool_error_total(), 3);
        assert_eq!(
            (s.compactions, s.plan_refreshes, s.hook_denials, s.warnings),
            (1, 1, 1, 1)
        );
    }

    #[test]
    fn takes_totals_from_the_result_line() {
        let mut text = lines(&[assistant(), usage_event(100, 10, Some(0.5))]);
        text.push_str(
            &json!({
                "type": "result", "session_id": "s", "reason": "step_limit", "detail": null,
                "exit_code": 2, "final_text": "", "steps": 7,
                "usage": {"input_tokens": 700, "output_tokens": 70},
                "cost": 1.25, "duration_ms": 900
            })
            .to_string(),
        );
        let stream = parse(&text);
        let result = stream.result.clone().unwrap();
        assert_eq!(result.reason, "step_limit");
        assert_eq!(result.exit_code, 2);
        let (steps, usage, cost) = stream.totals();
        assert_eq!(
            (steps, usage.input_tokens, usage.cache_read_tokens),
            (7, 700, 0)
        );
        assert_eq!(cost, 1.25);
    }

    #[test]
    fn sums_events_without_a_result_line() {
        // A killed run: no result, and a cut-off last line.
        let mut text = lines(&[
            assistant(),
            usage_event(100, 10, Some(0.5)),
            assistant(),
            usage_event(200, 20, None),
        ]);
        text.push_str("{\"type\": \"text_del");
        let stream = parse(&text);
        assert!(stream.result.is_none());
        assert_eq!(stream.unreadable_lines, 1);
        let (steps, usage, cost) = stream.totals();
        assert_eq!(steps, 2);
        assert_eq!(
            usage,
            Usage {
                input_tokens: 300,
                output_tokens: 30,
                cache_read_tokens: 10,
                cache_write_tokens: 0
            }
        );
        assert_eq!(cost, 0.5);
    }

    #[test]
    fn untrusted_numbers_are_clamped() {
        let huge = json!({"input_tokens": u64::MAX, "output_tokens": 1});
        let mut text = String::new();
        for _ in 0..3 {
            text.push_str(&json!({"type": "usage", "usage": huge, "cost": 1e308}).to_string());
            text.push('\n');
        }
        text.push_str(&json!({"type": "usage", "usage": {}, "cost": -5.0}).to_string());
        text.push('\n');
        let stream = parse(&text);
        assert_eq!(stream.usage.input_tokens, u64::MAX);
        assert_eq!(stream.usage.output_tokens, 3);
        assert!(stream.cost.is_finite() && stream.cost > 0.0);

        // A forged result line: negative cost and a reason that isn't a word.
        let forged = json!({"type": "result", "reason": "done\n| x |", "cost": -1e300, "steps": 3});
        let stream = parse(&forged.to_string());
        assert_eq!(stream.totals().2, 0.0);
        assert_eq!(stream.reason(), Some("invalid"));
        let nan = json!({"type": "result", "reason": "done", "cost": f64::MAX});
        assert_eq!(parse(&nan.to_string()).reason(), Some("done"));
    }

    #[test]
    fn plan_refreshes_only_count_from_the_planning_plugin() {
        let text = lines(&[
            AgentEvent::Custom {
                source: "other".into(),
                name: "plan_refreshed".into(),
                data: json!({}),
            },
            AgentEvent::Custom {
                source: "planning".into(),
                name: "plan_refreshed".into(),
                data: json!({}),
            },
        ]);
        assert_eq!(parse(&text).signals.plan_refreshes, 1);
    }

    #[test]
    fn denied_calls_are_not_tool_errors() {
        let denied = |content: &str| AgentEvent::ToolCallEnd {
            id: "c".into(),
            name: "bash".into(),
            content: content.into(),
            is_error: true,
            duration_ms: 0,
            metadata: Value::Null,
        };
        let text = lines(&[
            denied("Permission denied (safety): sudo is not allowed"),
            denied("Not run: this call needs approval (edit), but no one can approve it"),
            denied("exit code 1"),
            AgentEvent::SessionStart {
                session_id: "s".into(),
                model: "m".into(),
                workspace: "/work".into(),
                plugins: vec!["planning".into(), "tools-fs".into()],
            },
        ]);
        let stream = parse(&text);
        let s = &stream.signals;
        assert_eq!(s.tool_call_total(), 3);
        assert_eq!(s.tool_denial_total(), 2);
        assert_eq!(
            s.tool_errors,
            BTreeMap::from([("bash:error".to_string(), 1)])
        );
        assert_eq!(
            stream.plugins.as_deref(),
            Some(&["planning".to_string(), "tools-fs".to_string()][..])
        );
    }

    #[test]
    fn files_are_read_with_bounded_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let mut text = lines(&[assistant()]);
        text.push_str(&"x".repeat(MAX_LINE + 10));
        text.push('\n');
        text.push_str(&lines(&[assistant()]));
        text.push_str("{\"type\": \"assistant_message\"}");
        std::fs::write(&path, text).unwrap();
        let stream = parse_file(&path);
        assert_eq!(stream.steps, 3, "lines after a long one are still read");
        assert_eq!(stream.unreadable_lines, 1);
        assert_eq!(parse_file(&dir.path().join("missing")), Stream::default());
    }
}
