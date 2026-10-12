//! Pure formatting: tool lines, result summaries, coloured diffs, footers.
//!
//! Everything here returns strings (with ANSI styling) so it can be unit-tested without
//! a terminal.

use std::time::Duration;

use ch_kernel::{ApprovalRequest, TerminationReason, Usage};
use nu_ansi_term::{Color, Style};
use serde_json::Value;

/// Diff lines shown under an edit before the rest is cut off.
pub const MAX_DIFF_LINES: usize = 60;

/// Lines of a file preview shown in an approval prompt.
const MAX_PREVIEW_LINES: usize = 20;

pub fn dim(s: &str) -> String {
    Style::new().dimmed().paint(s).to_string()
}

pub fn bold(s: &str) -> String {
    Style::new().bold().paint(s).to_string()
}

pub fn red(s: &str) -> String {
    Color::Red.paint(s).to_string()
}

pub fn green(s: &str) -> String {
    Color::Green.paint(s).to_string()
}

pub fn yellow(s: &str) -> String {
    Color::Yellow.paint(s).to_string()
}

pub fn cyan(s: &str) -> String {
    Color::Cyan.paint(s).to_string()
}

/// Removes ANSI escape sequences (CSI `ESC [ ... letter` and OSC `ESC ] ... BEL`).
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('[') => {
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() || c == '~' {
                        break;
                    }
                }
            }
            Some(']') => {
                for c in chars.by_ref() {
                    if c == '\u{7}' {
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// The first non-empty line of `s`, trimmed.
pub fn first_line(s: &str) -> &str {
    s.trim().lines().next().unwrap_or("").trim_end()
}

/// `● read_file src/main.rs`
pub fn tool_start_line(name: &str, summary: &str) -> String {
    let summary = first_line(summary);
    let head = format!("{} {}", cyan("●"), bold(name));
    if summary.is_empty() {
        head
    } else {
        format!("{head} {summary}")
    }
}

/// Lines shown when a tool call ends: `  ⎿ 120 lines`, an error in red, and for
/// successful edits and writes the coloured diff.
///
/// `header` repeats the tool name when other output came between the start line and
/// this result (parallel calls).
pub fn tool_end_lines(
    name: &str,
    content: &str,
    is_error: bool,
    metadata: &Value,
    header: Option<&str>,
) -> Vec<String> {
    let prefix = match header {
        Some(h) => format!("  ⎿ {h}: "),
        None => "  ⎿ ".to_string(),
    };
    let summary = result_summary(name, content, is_error, metadata);
    let mut lines = vec![if is_error {
        format!("{}{}", dim(&prefix), red(&summary))
    } else {
        dim(&format!("{prefix}{summary}"))
    }];
    if !is_error
        && matches!(name, "edit_file" | "write_file")
        && let Some(diff) = metadata.get("diff").and_then(Value::as_str)
    {
        lines.extend(color_diff(diff, MAX_DIFF_LINES).into_iter().map(|l| {
            let mut s = "    ".to_string();
            s.push_str(&l);
            s
        }));
    }
    lines
}

/// A one-line description of a tool result: `120 lines`, `exit 1 · 3 lines`, `+3 -1`,
/// or `error: <first line>`.
pub fn result_summary(name: &str, content: &str, is_error: bool, metadata: &Value) -> String {
    let exit = metadata.get("exit_code").and_then(Value::as_i64);
    let timed_out = metadata.get("timed_out") == Some(&Value::Bool(true));
    if is_error {
        let first = first_line(content);
        return match (name, exit, timed_out) {
            (_, _, true) => format!("timed out: {first}"),
            ("bash", Some(code), _) => format!("exit {code}: {first}"),
            _ => first.to_string(),
        };
    }
    if let Some(diff) = metadata.get("diff").and_then(Value::as_str) {
        let (added, removed) = diff_counts(diff);
        return format!("+{added} -{removed}");
    }
    let n = content.lines().count();
    let lines = if n == 0 {
        "no output".to_string()
    } else {
        format!("{n} line{}", if n == 1 { "" } else { "s" })
    };
    match exit {
        Some(code) => format!("exit {code} · {lines}"),
        None => lines,
    }
}

/// Added and removed lines in a unified diff (file headers excluded).
pub fn diff_counts(diff: &str) -> (usize, usize) {
    let mut added = 0;
    let mut removed = 0;
    for line in diff.lines() {
        if line.starts_with("+++") || line.starts_with("---") {
            continue;
        }
        if line.starts_with('+') {
            added += 1;
        } else if line.starts_with('-') {
            removed += 1;
        }
    }
    (added, removed)
}

/// Colours a unified diff: additions green, removals red, hunk headers cyan, context
/// dim. File headers (`---`/`+++`) are dropped (the tool line already names the file).
/// At most `max_lines` lines are returned, plus a `… N more lines` marker.
pub fn color_diff(diff: &str, max_lines: usize) -> Vec<String> {
    let body: Vec<&str> = diff
        .lines()
        .filter(|l| !(l.starts_with("+++ ") || l.starts_with("--- ") || *l == "---"))
        .collect();
    let mut out: Vec<String> = body
        .iter()
        .take(max_lines)
        .map(|line| {
            if line.starts_with("@@") {
                cyan(line)
            } else if line.starts_with('+') {
                green(line)
            } else if line.starts_with('-') {
                red(line)
            } else {
                dim(line)
            }
        })
        .collect();
    if body.len() > max_lines {
        out.push(dim(&format!("… {} more lines", body.len() - max_lines)));
    }
    out
}

/// `12.3k`-style token counts.
pub fn fmt_tokens(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{:.1}k", n as f64 / 1_000.0),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}

/// `512 B`, `48.2 KB` or `1.3 MB`.
pub fn fmt_bytes(n: u64) -> String {
    match n {
        0..=1_023 => format!("{n} B"),
        1_024..=1_048_575 => format!("{:.1} KB", n as f64 / 1_024.0),
        _ => format!("{:.1} MB", n as f64 / 1_048_576.0),
    }
}

/// `8.1s` or `2m 05s`.
pub fn fmt_duration(d: Duration) -> String {
    let secs = d.as_secs_f64();
    if secs < 60.0 {
        format!("{secs:.1}s")
    } else {
        let s = d.as_secs();
        format!("{}m {:02}s", s / 60, s % 60)
    }
}

/// What to say about a turn that didn't finish normally, if anything.
pub fn reason_text(reason: &TerminationReason) -> Option<String> {
    match reason {
        TerminationReason::Done => None,
        TerminationReason::Cancelled => Some("interrupted".into()),
        TerminationReason::StepLimit => {
            Some("stopped: step limit reached (raise agent.max_steps to allow more)".into())
        }
        TerminationReason::ContextOverflow { message } => Some(format!(
            "stopped: context window full ({message}); try /compact or /clear"
        )),
        TerminationReason::HookStop { source, reason } => {
            Some(format!("stopped by {source}: {reason}"))
        }
        TerminationReason::PromptDenied { reason } => Some(format!("prompt blocked: {reason}")),
        TerminationReason::Error { message } => Some(format!("error: {message}")),
    }
}

/// The dim line printed after each turn, preceded by the reason when it wasn't `done`.
pub fn footer(reason: &TerminationReason, usage: &Usage, cost: f64, elapsed: Duration) -> String {
    let mut parts = vec![
        format!("{} in", fmt_tokens(usage.prompt_tokens())),
        format!("{} out", fmt_tokens(usage.output_tokens)),
    ];
    if cost > 0.0 {
        parts.push(format!("${cost:.4}"));
    }
    parts.push(fmt_duration(elapsed));
    let stats = dim(&format!("  {}", parts.join(" · ")));
    match reason_text(reason) {
        None => stats,
        Some(text) => {
            let styled = match reason {
                TerminationReason::Error { .. } | TerminationReason::ContextOverflow { .. } => {
                    red(&text)
                }
                _ => yellow(&text),
            };
            format!("  {styled}\n{stats}")
        }
    }
}

/// The approval prompt: what is being run, why it needs approval, a preview of the
/// change, and the keys.
pub fn approval_prompt(req: &ApprovalRequest) -> String {
    let mut lines = vec![format!(
        "{} {} {}",
        yellow("?"),
        bold(&req.tool),
        first_line(&req.summary)
    )];
    if !req.reason.trim().is_empty() {
        lines.push(dim(&format!("  {}", first_line(&req.reason))));
    }
    lines.extend(approval_preview(req).into_iter().map(|l| format!("  {l}")));
    let always = match &req.key {
        Some(k) => format!("{} always allow `{k}`", bold("a")),
        None => format!("{} always (this session)", bold("a")),
    };
    lines.push(format!(
        "  {} yes · {} no · {always} · {} ",
        bold("y"),
        bold("n"),
        dim("Esc no ›")
    ));
    lines.join("\n")
}

/// A short preview of what the call would do: the command for `bash`, removed and added
/// text for `edit_file`, the start of the content for `write_file`.
pub fn approval_preview(req: &ApprovalRequest) -> Vec<String> {
    let s = |k: &str| req.input.get(k).and_then(Value::as_str);
    match req.tool.as_str() {
        "bash" => s("command")
            .map(|c| {
                c.lines()
                    .take(MAX_PREVIEW_LINES)
                    .map(|l| format!("$ {l}"))
                    .collect()
            })
            .unwrap_or_default(),
        "edit_file" => {
            let mut out = Vec::new();
            if let Some(old) = s("old_text") {
                out.extend(
                    old.lines()
                        .take(MAX_PREVIEW_LINES)
                        .map(|l| red(&format!("-{l}"))),
                );
            }
            if let Some(new) = s("new_text") {
                out.extend(
                    new.lines()
                        .take(MAX_PREVIEW_LINES)
                        .map(|l| green(&format!("+{l}"))),
                );
            }
            out
        }
        "write_file" => s("content")
            .map(|c| {
                let total = c.lines().count();
                let mut out: Vec<String> = c
                    .lines()
                    .take(MAX_PREVIEW_LINES)
                    .map(|l| green(&format!("+{l}")))
                    .collect();
                if total > MAX_PREVIEW_LINES {
                    out.push(dim(&format!("… {} more lines", total - MAX_PREVIEW_LINES)));
                }
                out
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn strip_ansi_removes_styles() {
        assert_eq!(strip_ansi(&red("x")), "x");
        assert_eq!(strip_ansi(&format!("a{}b", bold("c"))), "acb");
    }

    #[test]
    fn tool_start() {
        assert_eq!(
            strip_ansi(&tool_start_line("read_file", "src/main.rs")),
            "● read_file src/main.rs"
        );
        assert_eq!(strip_ansi(&tool_start_line("x", "")), "● x");
    }

    #[test]
    fn tool_end_summaries() {
        let plain = |lines: Vec<String>| {
            lines
                .iter()
                .map(|l| strip_ansi(l))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let content = (1..=120)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            plain(tool_end_lines(
                "read_file",
                &content,
                false,
                &Value::Null,
                None
            )),
            "  ⎿ 120 lines"
        );
        assert_eq!(
            plain(tool_end_lines(
                "read_file",
                "File not found: x\nhint",
                true,
                &Value::Null,
                None
            )),
            "  ⎿ File not found: x"
        );
        assert_eq!(
            plain(tool_end_lines(
                "bash",
                "boom",
                true,
                &json!({"exit_code": 2, "timed_out": false}),
                Some("bash")
            )),
            "  ⎿ bash: exit 2: boom"
        );
        assert_eq!(
            plain(tool_end_lines(
                "bash",
                "a\nb",
                false,
                &json!({"exit_code": 0}),
                None
            )),
            "  ⎿ exit 0 · 2 lines"
        );
        assert_eq!(
            plain(tool_end_lines("grep_text", "", false, &Value::Null, None)),
            "  ⎿ no output"
        );
    }

    #[test]
    fn edits_show_counts_and_diff() {
        let diff = "--- a/x.rs\n+++ b/x.rs\n@@ -1,2 +1,2 @@\n ctx\n-old\n+new\n";
        let lines = tool_end_lines(
            "edit_file",
            "Edited x.rs",
            false,
            &json!({"path": "x.rs", "diff": diff}),
            None,
        );
        let plain: Vec<String> = lines.iter().map(|l| strip_ansi(l)).collect();
        assert_eq!(
            plain,
            vec![
                "  ⎿ +1 -1",
                "    @@ -1,2 +1,2 @@",
                "     ctx",
                "    -old",
                "    +new"
            ]
        );
        // Failed edits don't show a diff.
        assert_eq!(
            tool_end_lines("edit_file", "no match", true, &json!({"diff": diff}), None).len(),
            1
        );
    }

    #[test]
    fn diff_colours() {
        let lines = color_diff("@@ -1 +1 @@\n-a\n+b\n c", 10);
        assert_eq!(lines[0], cyan("@@ -1 +1 @@"));
        assert_eq!(lines[1], red("-a"));
        assert_eq!(lines[2], green("+b"));
        assert_eq!(lines[3], dim(" c"));
    }

    #[test]
    fn long_diffs_are_cut() {
        let diff: String = (0..100).map(|i| format!("+{i}\n")).collect();
        let lines = color_diff(&diff, 10);
        assert_eq!(lines.len(), 11);
        assert_eq!(strip_ansi(&lines[10]), "… 90 more lines");
    }

    #[test]
    fn token_and_duration_formatting() {
        assert_eq!(fmt_tokens(950), "950");
        assert_eq!(fmt_tokens(12_345), "12.3k");
        assert_eq!(fmt_tokens(1_500_000), "1.5M");
        assert_eq!(fmt_duration(Duration::from_millis(8100)), "8.1s");
        assert_eq!(fmt_duration(Duration::from_secs(125)), "2m 05s");
    }

    #[test]
    fn footers() {
        let usage = Usage {
            input_tokens: 12_000,
            output_tokens: 450,
            cache_read_tokens: 300,
            ..Default::default()
        };
        let f = strip_ansi(&footer(
            &TerminationReason::Done,
            &usage,
            0.0213,
            Duration::from_millis(8100),
        ));
        assert_eq!(f, "  12.3k in · 450 out · $0.0213 · 8.1s");
        let f = strip_ansi(&footer(
            &TerminationReason::StepLimit,
            &usage,
            0.0,
            Duration::from_secs(1),
        ));
        assert!(f.starts_with("  stopped: step limit reached"), "{f}");
        assert!(f.ends_with("12.3k in · 450 out · 1.0s"), "{f}");
    }

    #[test]
    fn approval_prompt_shows_command_and_key() {
        let req = ApprovalRequest {
            tool: "bash".into(),
            input: json!({"command": "git commit -m wip"}),
            summary: "git commit -m wip".into(),
            reason: "not a read-only command".into(),
            key: Some("bash:git commit *".into()),
        };
        let p = strip_ansi(&approval_prompt(&req));
        assert!(p.starts_with("? bash git commit -m wip"), "{p}");
        assert!(p.contains("  not a read-only command"), "{p}");
        assert!(p.contains("$ git commit -m wip"), "{p}");
        assert!(p.contains("a always allow `bash:git commit *`"), "{p}");
    }

    #[test]
    fn edit_preview() {
        let req = ApprovalRequest {
            tool: "edit_file".into(),
            input: json!({"path": "a", "old_text": "x\ny", "new_text": "z"}),
            summary: "a".into(),
            reason: String::new(),
            key: None,
        };
        let p: Vec<String> = approval_preview(&req)
            .iter()
            .map(|l| strip_ansi(l))
            .collect();
        assert_eq!(p, vec!["-x", "-y", "+z"]);
    }
}
