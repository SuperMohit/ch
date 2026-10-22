//! Plain-text lines for `--verbose` output on stderr. No colours: stderr is often a log.

use ch_kernel::util::{diff_counts, first_line};
use ch_kernel::{AgentEvent, CompactionKind};
use serde_json::Value;

/// The stderr line for an event in verbose text mode, if it has one.
pub fn verbose_line(event: &AgentEvent) -> Option<String> {
    match event {
        AgentEvent::ToolCallStart { name, summary, .. } => Some(tool_start(name, summary)),
        AgentEvent::ToolCallEnd {
            name,
            content,
            is_error,
            metadata,
            ..
        } => Some(format!(
            "  ⎿ {}",
            result_summary(name, content, *is_error, metadata)
        )),
        AgentEvent::ApprovalRequested {
            tool,
            summary,
            reason,
        } => Some(format!("  ? {tool} {summary} needs approval: {reason}")),
        AgentEvent::ApprovalResolved { approved, .. } => Some(format!(
            "  ⎿ {}",
            if *approved { "approved" } else { "denied" }
        )),
        AgentEvent::HookDenied { hook, reason, .. } => {
            Some(format!("  ⎿ denied by {hook}: {}", first_line(reason)))
        }
        AgentEvent::Note { source, text } => Some(format!("  · {source}: {}", first_line(text))),
        AgentEvent::ContextCompacted {
            kind,
            tokens_before,
            tokens_after,
        } => Some(format!(
            "  · context {}: {tokens_before} → {tokens_after} tokens",
            match kind {
                CompactionKind::Elided => "elided",
                CompactionKind::Summarized => "summarized",
            }
        )),
        _ => None,
    }
}

/// `● read_file src/main.rs`
pub fn tool_start(name: &str, summary: &str) -> String {
    let summary = first_line(summary);
    if summary.is_empty() {
        format!("● {name}")
    } else {
        format!("● {name} {summary}")
    }
}

/// A one-line description of a tool result: `120 lines`, `exit 1 · 3 lines`, `+3 -1`,
/// or `error: <first line>`.
pub fn result_summary(name: &str, content: &str, is_error: bool, metadata: &Value) -> String {
    if is_error {
        return format!("error: {}", first_line(content));
    }
    if let Some(diff) = metadata.get("diff").and_then(Value::as_str) {
        let (added, removed) = diff_counts(diff);
        return format!("+{added} -{removed}");
    }
    let lines = content.lines().count();
    let lines = format!("{lines} line{}", if lines == 1 { "" } else { "s" });
    match metadata.get("exit_code") {
        Some(Value::Number(code)) => format!("exit {code} · {lines}"),
        _ if name == "bash" && metadata.get("timed_out") == Some(&Value::Bool(true)) => {
            format!("timed out · {lines}")
        }
        _ => lines,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tool_lines() {
        let start = AgentEvent::ToolCallStart {
            id: "1".into(),
            name: "read_file".into(),
            input: json!({}),
            summary: "src/main.rs".into(),
        };
        assert_eq!(verbose_line(&start).unwrap(), "● read_file src/main.rs");
        let end = AgentEvent::ToolCallEnd {
            id: "1".into(),
            name: "read_file".into(),
            content: "a\nb\nc".into(),
            is_error: false,
            duration_ms: 3,
            metadata: Value::Null,
        };
        assert_eq!(verbose_line(&end).unwrap(), "  ⎿ 3 lines");
    }

    #[test]
    fn summaries() {
        assert_eq!(
            result_summary("read_file", "No such file\nmore", true, &Value::Null),
            "error: No such file"
        );
        assert_eq!(
            result_summary(
                "edit_file",
                "ok",
                false,
                &json!({"diff": "--- a\n+++ b\n@@ -1 +1 @@\n-x\n+y\n+z"})
            ),
            "+2 -1"
        );
        assert_eq!(
            result_summary("bash", "hi", false, &json!({"exit_code": 0})),
            "exit 0 · 1 line"
        );
    }

    #[test]
    fn deltas_have_no_verbose_line() {
        assert!(verbose_line(&AgentEvent::TextDelta { text: "x".into() }).is_none());
    }
}
