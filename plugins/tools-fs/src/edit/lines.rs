//! Line helpers for the whitespace fallback and the hints.

use std::collections::HashMap;

use ch_kernel::reexports::CancellationToken;
use ch_kernel::util::truncate_middle;

/// Search loops check for cancellation every this many steps.
const CANCEL_EVERY: usize = 4096;

/// `text` as whole lines without line endings: one trailing line break is dropped.
pub(super) fn as_lines(text: &str) -> Vec<&str> {
    let text = text.strip_suffix('\n').unwrap_or(text);
    text.split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect()
}

/// `line` without its line ending.
pub(super) fn body(line: &str) -> &str {
    let line = line.strip_suffix('\n').unwrap_or(line);
    line.strip_suffix('\r').unwrap_or(line)
}

/// The leading whitespace of `line`.
pub(super) fn indent(line: &str) -> &str {
    &line[..line.len() - line.trim_start().len()]
}

/// Indentation in words: `4 spaces`, `1 tab`, `1 tab + 2 spaces`, `none`.
pub(super) fn describe_indent(ws: &str) -> String {
    if ws.is_empty() {
        return "none".into();
    }
    let mut runs: Vec<(char, usize)> = Vec::new();
    for c in ws.chars() {
        match runs.last_mut() {
            Some((last, n)) if *last == c => *n += 1,
            _ => runs.push((c, 1)),
        }
    }
    runs.iter()
        .map(|&(c, n)| {
            let s = if n == 1 { "" } else { "s" };
            match c {
                ' ' => format!("{n} space{s}"),
                '\t' => format!("{n} tab{s}"),
                _ => format!("{n} × U+{:04X}", c as u32),
            }
        })
        .collect::<Vec<_>>()
        .join(" + ")
}

/// Whitespace characters in words: `spaces`, `tabs`, `spaces and tabs`, `U+00A0`.
pub(super) fn describe_chars(chars: &[char]) -> String {
    let names: Vec<String> = chars
        .iter()
        .map(|&c| match c {
            ' ' => "spaces".to_string(),
            '\t' => "tabs".to_string(),
            _ => format!("U+{:04X}", c as u32),
        })
        .collect();
    match names.split_last() {
        Some((last, rest)) if !rest.is_empty() => format!("{} and {last}", rest.join(", ")),
        _ => names.concat(),
    }
}

/// `3` or `3–5`: `n` lines from the 0-based `start`.
pub(super) fn span(start: usize, n: usize) -> String {
    if n <= 1 {
        (start + 1).to_string()
    } else {
        format!("{}–{}", start + 1, start + n)
    }
}

/// `line 3` or `lines 3–5`.
pub(super) fn lines_label(start: usize, n: usize) -> String {
    let s = if n <= 1 { "" } else { "s" };
    format!("line{s} {}", span(start, n))
}

/// Up to ten places of `n` lines each, as `lines 3–5, 10–12 and 4 more`.
pub(super) fn places_label(starts: &[usize], n: usize) -> String {
    let shown: Vec<String> = starts.iter().take(10).map(|&s| span(s, n)).collect();
    let more = starts.len() - shown.len();
    let more = if more > 0 {
        format!(" and {more} more")
    } else {
        String::new()
    };
    format!("lines {}{more}", shown.join(", "))
}

/// One line of a hint, shortened in the middle when long.
pub(super) fn short(line: &str) -> String {
    truncate_middle(line, 160).replace('\n', " ")
}

/// Numbers for the items of `needle` (equal items get equal numbers) and for each item
/// of `hay` (`u32::MAX`, which matches nothing, for items not in `needle`), so the two
/// can be compared as integers.
pub(super) fn ids(needle: &[&str], hay: &[&str]) -> (Vec<u32>, Vec<u32>) {
    let mut map: HashMap<&str, u32> = HashMap::new();
    let needle_ids = needle
        .iter()
        .map(|s| {
            let next = map.len() as u32;
            *map.entry(s).or_insert(next)
        })
        .collect();
    let hay_ids = hay
        .iter()
        .map(|s| map.get(s).copied().unwrap_or(u32::MAX))
        .collect();
    (needle_ids, hay_ids)
}

/// Where `pattern` starts in `text`, every occurrence in order (overlapping ones too),
/// in time linear in both lengths (the Knuth–Morris–Pratt search). `None` when `cancel`
/// fires first.
pub(super) fn find_all(
    text: &[u32],
    pattern: &[u32],
    cancel: &CancellationToken,
) -> Option<Vec<usize>> {
    if pattern.is_empty() || pattern.len() > text.len() {
        return Some(Vec::new());
    }
    // `border[i]`: the length of the longest proper prefix of `pattern[..=i]` that is
    // also a suffix of it, where a mismatch after `pattern[..=i]` resumes.
    let mut border = vec![0; pattern.len()];
    let mut k = 0;
    for i in 1..pattern.len() {
        while k > 0 && pattern[i] != pattern[k] {
            k = border[k - 1];
        }
        if pattern[i] == pattern[k] {
            k += 1;
        }
        border[i] = k;
    }
    let mut found = Vec::new();
    let mut k = 0;
    for (i, &t) in text.iter().enumerate() {
        if cancelled_at(i, cancel) {
            return None;
        }
        while k > 0 && t != pattern[k] {
            k = border[k - 1];
        }
        if t == pattern[k] {
            k += 1;
        }
        if k == pattern.len() {
            found.push(i + 1 - k);
            k = border[k - 1];
        }
    }
    Some(found)
}

/// Whether to stop a search loop at step `i`.
pub(super) fn cancelled_at(i: usize, cancel: &CancellationToken) -> bool {
    i.is_multiple_of(CANCEL_EVERY) && cancel.is_cancelled()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find(text: &[u32], pattern: &[u32]) -> Vec<usize> {
        find_all(text, pattern, &CancellationToken::new()).unwrap()
    }

    #[test]
    fn finds_every_occurrence() {
        assert_eq!(find(&[1, 1, 1, 1], &[1, 1]), [0, 1, 2]);
        assert_eq!(find(&[1, 2, 1, 2, 1, 3], &[1, 2, 1, 3]), [2]);
        assert_eq!(find(&[1, 2, 3], &[2, 3, 4]), Vec::<usize>::new());
        assert_eq!(find(&[1], &[1, 1]), Vec::<usize>::new());
        assert_eq!(find(&[1, 2], &[]), Vec::<usize>::new());
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(find_all(&[1, 2], &[1], &cancel), None);
    }

    #[test]
    fn interned_ids() {
        let (n, h) = ids(&["a", "b", "a"], &["b", "c", "a"]);
        assert_eq!(n, [0, 1, 0]);
        assert_eq!(h, [1, u32::MAX, 0]);
    }

    #[test]
    fn words() {
        assert_eq!(describe_indent(""), "none");
        assert_eq!(describe_indent("    "), "4 spaces");
        assert_eq!(describe_indent("\t"), "1 tab");
        assert_eq!(describe_indent("\t  "), "1 tab + 2 spaces");
        assert_eq!(describe_indent("\u{a0}"), "1 × U+00A0");
        assert_eq!(describe_chars(&[' ']), "spaces");
        assert_eq!(describe_chars(&['\t', ' ']), "tabs and spaces");
        assert_eq!(places_label(&[0, 4], 2), "lines 1–2, 5–6");
        let many: Vec<usize> = (0..12).collect();
        assert_eq!(
            places_label(&many, 1),
            "lines 1, 2, 3, 4, 5, 6, 7, 8, 9, 10 and 2 more"
        );
    }

    #[test]
    fn line_helpers() {
        assert_eq!(as_lines("a\r\nb\r\n"), ["a", "b"]);
        assert_eq!(as_lines("a\n\n"), ["a", ""]);
        assert_eq!(body("x \r\n"), "x ");
        assert_eq!(indent("  \tx "), "  \t");
        assert_eq!(lines_label(0, 1), "line 1");
        assert_eq!(lines_label(39, 13), "lines 40–52");
    }
}
