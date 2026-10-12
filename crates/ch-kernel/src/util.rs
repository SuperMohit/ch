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

/// Wraps text in a `<system-reminder>` tag.
pub fn system_reminder(text: &str) -> String {
    format!("<system-reminder>\n{}\n</system-reminder>", text.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
