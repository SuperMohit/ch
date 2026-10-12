//! Turning an [`ExecResult`] into the text the model sees.

use ch_kernel::ExecResult;

/// stdout, then stderr under `[stderr]`, then a status line. `(no output)` when both
/// streams are empty.
pub(crate) fn render(result: &ExecResult, timeout_secs: u64) -> String {
    let mut out = String::new();
    let stdout = result.stdout.trim_end_matches('\n');
    let stderr = result.stderr.trim_end_matches('\n');
    if !stdout.is_empty() {
        out.push_str(stdout);
        out.push('\n');
    }
    if !stderr.is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str("[stderr]\n");
        out.push_str(stderr);
        out.push('\n');
    }
    if out.is_empty() {
        out.push_str("(no output)\n");
    }
    out.push_str(&status_line(result, timeout_secs));
    out
}

fn status_line(result: &ExecResult, timeout_secs: u64) -> String {
    if result.timed_out {
        format!("[timed out after {timeout_secs}s — the process was killed]")
    } else if result.cancelled {
        "[cancelled]".to_string()
    } else {
        match result.exit_code {
            Some(code) => format!("[exit code: {code}]"),
            None => match result.signal {
                Some(sig) => format!("[terminated by signal {sig}]"),
                None => "[terminated by a signal]".to_string(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(stdout: &str, stderr: &str, code: Option<i32>) -> ExecResult {
        ExecResult {
            exit_code: code,
            stdout: stdout.into(),
            stderr: stderr.into(),
            ..Default::default()
        }
    }

    #[test]
    fn stdout_only() {
        assert_eq!(
            render(&result("hi\n", "", Some(0)), 5),
            "hi\n[exit code: 0]"
        );
    }

    #[test]
    fn both_streams() {
        assert_eq!(
            render(&result("a\n", "b\n", Some(1)), 5),
            "a\n\n[stderr]\nb\n[exit code: 1]"
        );
    }

    #[test]
    fn stderr_only_and_empty() {
        assert_eq!(
            render(&result("", "b", Some(2)), 5),
            "[stderr]\nb\n[exit code: 2]"
        );
        assert_eq!(
            render(&result("", "", Some(0)), 5),
            "(no output)\n[exit code: 0]"
        );
    }

    #[test]
    fn timeout_and_cancel_lines() {
        let mut r = result("", "", None);
        r.timed_out = true;
        assert!(render(&r, 7).ends_with("[timed out after 7s — the process was killed]"));
        r.timed_out = false;
        r.cancelled = true;
        assert!(render(&r, 7).ends_with("[cancelled]"));
        r.cancelled = false;
        assert!(render(&r, 7).ends_with("[terminated by a signal]"));
    }
}
