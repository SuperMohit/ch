//! Small helpers shared by the kernel and plugins.

/// Truncates `s` to at most `max_chars` characters, keeping the head and the tail and
/// putting a marker in the middle that says how much was removed.
pub fn truncate_middle(s: &str, max_chars: usize) -> String {
    let total = s.chars().count();
    if total <= max_chars {
        return s.to_string();
    }
    let marker_budget = 80.min(max_chars);
    let keep = max_chars - marker_budget;
    let head_n = keep * 2 / 3;
    let tail_n = keep - head_n;
    let head: String = s.chars().take(head_n).collect();
    let tail: String = s.chars().skip(total - tail_n).collect::<String>();
    let removed = total - head_n - tail_n;
    format!("{head}\n[... {removed} chars truncated ...]\n{tail}")
}

/// Heuristic binary check: a NUL byte in the first 8 KiB.
pub fn looks_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8192).any(|&b| b == 0)
}

/// The first line of `s` after leading whitespace, without trailing whitespace.
pub fn first_line(s: &str) -> &str {
    s.trim().lines().next().unwrap_or("").trim_end()
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

/// Wraps text in a `<system-reminder>` tag.
pub fn system_reminder(text: &str) -> String {
    format!("<system-reminder>\n{}\n</system-reminder>", text.trim())
}

/// Replaces `path` with `bytes` so that readers see the old file or the new one, never
/// a partial write: the bytes go to a unique temp file next to it, reach the disk, and
/// are renamed over it.
pub fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let tmp = path.with_file_name(format!(".{name}.{}.tmp", uuid::Uuid::new_v4().simple()));
    let written = std::fs::File::create(&tmp).and_then(|mut f| {
        f.write_all(bytes)?;
        f.sync_all()
    });
    if let Err(e) = written.and_then(|()| std::fs::rename(&tmp, path)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// `text` without complete ANSI escape sequences: CSI (`ESC [`, colours and cursor
/// moves) and OSC (`ESC ]`, titles and links, ended by BEL or `ESC \`). An `ESC` that
/// doesn't start a complete sequence is kept, for [`printable`] to show.
pub fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find('\u{1b}') {
        out.push_str(&rest[..i]);
        let seq = &rest[i + 1..];
        match ansi_len(seq) {
            Some(n) => rest = &seq[n..],
            None => {
                out.push('\u{1b}');
                rest = seq;
            }
        }
    }
    out.push_str(rest);
    out
}

/// The length of the escape sequence after an `ESC`, if it is a complete CSI or OSC.
fn ansi_len(seq: &str) -> Option<usize> {
    if let Some(body) = seq.strip_prefix('[') {
        // Parameter and intermediate bytes, then one final byte.
        let end = body.find(|c: char| !matches!(c, '\u{20}'..='\u{3f}'))?;
        let last = body[end..].chars().next()?;
        return matches!(last, '\u{40}'..='\u{7e}').then_some(1 + end + 1);
    }
    let body = seq.strip_prefix(']')?;
    let end = body.find(['\u{7}', '\u{1b}'])?;
    match &body[end..] {
        b if b.starts_with('\u{7}') => Some(1 + end + 1),
        b if b.starts_with("\u{1b}\\") => Some(1 + end + 2),
        _ => None,
    }
}

/// `text` safe to print on a terminal: control characters other than tab, and the
/// characters that reorder bidirectional text, are shown escaped (`\u{1b}`), so the
/// text can't move the cursor, clear the screen or disguise what is around it. A
/// newline is escaped too: split multi-line text first.
pub fn printable(text: &str) -> String {
    let bidi = |c: char| matches!(c, '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}');
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if (c.is_control() && c != '\t') || bidi(c) {
            out.extend(c.escape_unicode());
        } else {
            out.push(c);
        }
    }
    out
}

/// Text from the model, a tool or a hook made safe to print, keeping its newlines:
/// colour and cursor codes and carriage returns are removed, then each line is made
/// [`printable`].
pub fn terminal_text(text: &str) -> String {
    strip_ansi(text)
        .split('\n')
        .map(|line| printable(&line.replace('\r', "")))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_ansi_removes_complete_sequences_only() {
        assert_eq!(strip_ansi("\u{1b}[1;31mred\u{1b}[0m"), "red");
        assert_eq!(strip_ansi("a\u{1b}[2Kb\u{1b}[10;1Hc"), "abc");
        assert_eq!(strip_ansi("\u{1b}]0;title\u{7}x"), "x");
        assert_eq!(strip_ansi("\u{1b}]8;;http://a\u{1b}\\link"), "link");
        // An unfinished sequence is kept for `printable` to show, not swallowed with
        // the text after it.
        assert_eq!(strip_ansi("\u{1b}]0;no end"), "\u{1b}]0;no end");
        assert_eq!(strip_ansi("x\u{1b}"), "x\u{1b}");
        assert_eq!(strip_ansi("\u{1b}c reset"), "\u{1b}c reset");
    }

    #[test]
    fn printable_escapes_controls_and_bidi() {
        assert_eq!(printable("ls\r\u{1b}[2K"), "ls\\u{d}\\u{1b}[2K");
        assert_eq!(printable("a\tb\nc"), "a\tb\\u{a}c");
        assert_eq!(printable("x\u{9b}2Jy"), "x\\u{9b}2Jy");
        assert_eq!(printable("abc\u{202e}fed"), "abc\\u{202e}fed");
        assert_eq!(printable("plain ✓ text"), "plain ✓ text");
    }

    #[test]
    fn terminal_text_keeps_lines_and_drops_codes() {
        assert_eq!(
            terminal_text("\u{1b}[31mfail\u{1b}[0m\r\nok\rhidden\u{7}"),
            "fail\nokhidden\\u{7}"
        );
    }

    #[test]
    fn short_strings_unchanged() {
        assert_eq!(truncate_middle("hello", 10), "hello");
    }

    #[test]
    fn long_strings_keep_head_and_tail() {
        let s: String = "a".repeat(500) + &"z".repeat(500);
        let t = truncate_middle(&s, 300);
        assert!(t.starts_with("aaa"));
        assert!(t.ends_with("zzz"));
        assert!(t.contains("chars truncated"));
        assert!(t.chars().count() <= 300 + 40);
    }

    #[test]
    fn binary_detection() {
        assert!(looks_binary(b"ab\0cd"));
        assert!(!looks_binary(b"plain text"));
    }

    #[test]
    fn write_atomic_replaces_the_file_and_leaves_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.toml");
        std::fs::write(&path, "old").unwrap();
        write_atomic(&path, b"new").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        let names: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(names.len(), 1);
        assert!(write_atomic(&dir.path().join("missing/f"), b"x").is_err());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
