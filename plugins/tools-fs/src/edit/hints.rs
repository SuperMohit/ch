//! What an `old_text` that wasn't found looks like in the file: the lines where it
//! matches if whitespace is ignored, or the closest lines with a diff.

use std::time::Duration;

use ch_kernel::reexports::CancellationToken;
use similar::{ChangeTag, TextDiff};

use super::lines::{as_lines, body, cancelled_at, find_all, ids, lines_label, places_label, short};

/// How to recover from an old_text that wasn't found.
pub(super) const ADVICE: &str = "Re-read the file with read_file and copy the text exactly; \
                                 whitespace and indentation must match, and line-number \
                                 prefixes are not part of the file.";

/// Lines shown in a hint.
const MAX_HINT_LINES: usize = 40;

/// What a line of old_text found once in the file adds to a window's score; a line
/// found `k` times adds `1/k` of it.
const WEIGHT: u64 = 1 << 20;

/// Line comparisons spent on each place when picking its best-aligned window.
const ALIGN_BUDGET: usize = 1_000_000;

/// Explains why `needle` wasn't found in the file whose lines (with their endings) are
/// `lines`: the file's lines where it matches if whitespace is ignored, or the closest
/// lines with a diff, or just how to recover.
pub(super) fn not_found(
    rel: &str,
    lines: &[&str],
    needle: &str,
    replace_all: bool,
    cancel: &CancellationToken,
) -> String {
    let base = format!("old_text was not found in `{rel}`.");
    let needle_lines = as_lines(needle);
    if let Some((first, last)) = loose_match(lines, needle, cancel) {
        // Blank lines at old_text's ends belong to the place too, so a difference there
        // shows.
        let blank = |l: &&&str| l.trim().is_empty();
        let lead = needle_lines.iter().take_while(blank).count();
        let trail = needle_lines.iter().rev().take_while(blank).count();
        let first = first.saturating_sub(lead);
        let last = (last + trail).min(lines.len() - 1);
        let exact = if replace_all {
            "replace_all only replaces exact matches. "
        } else {
            ""
        };
        return format!(
            "{base} Ignoring whitespace, it matches {}, which read:\n{}\n{exact}{ADVICE}",
            lines_label(first, last - first + 1),
            verbatim(lines, first, last)
        );
    }
    match closest_lines(lines, &needle_lines, cancel) {
        Some(close) => format!("{base} {close}\n{ADVICE}"),
        None => format!("{base} {ADVICE}"),
    }
}

/// The file's lines `first..=last` as they are, for the model to copy.
pub(super) fn verbatim(lines: &[&str], first: usize, last: usize) -> String {
    render(lines[first..=last].iter().map(|l| short(body(l))))
}

/// Hint lines joined, at most [`MAX_HINT_LINES`] of them.
fn render(lines: impl Iterator<Item = String>) -> String {
    let mut shown = Vec::new();
    let mut more = 0;
    for line in lines {
        if shown.len() < MAX_HINT_LINES {
            shown.push(line);
        } else {
            more += 1;
        }
    }
    if more > 0 {
        shown.push(format!("… {more} more lines"));
    }
    shown.join("\n")
}

/// The first and last line (0-based) of the first place where `needle`'s words occur
/// in order, with any whitespace (line breaks included) between them. Words are whole,
/// except that the first may be the end of a file's word and the last the start of one,
/// where the cut doesn't split a run of word characters (`x` is not in `max`).
fn loose_match(lines: &[&str], needle: &str, cancel: &CancellationToken) -> Option<(usize, usize)> {
    let words: Vec<&str> = needle.split_whitespace().collect();
    let (&first, &last) = (words.first()?, words.last()?);
    let mut hay = Vec::new();
    let mut line_of = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if cancelled_at(i, cancel) {
            return None;
        }
        for word in line.split_whitespace() {
            hay.push(word);
            line_of.push(i);
        }
    }
    let at = match words.len() {
        1 => hay.iter().position(|w| inside(w, first))?,
        2 => hay
            .windows(2)
            .position(|w| ends_with_cut(w[0], first) && starts_with_cut(w[1], last))?,
        n => {
            // Where the whole middle words occur, then the cut words around them.
            let middle = &words[1..n - 1];
            let (pattern, text) = ids(middle, &hay);
            let after = |s: usize| s + middle.len();
            let s = find_all(&text, &pattern, cancel)?.into_iter().find(|&s| {
                s >= 1
                    && after(s) < hay.len()
                    && ends_with_cut(hay[s - 1], first)
                    && starts_with_cut(hay[after(s)], last)
            })?;
            s - 1
        }
    };
    Some((line_of[at], line_of[at + words.len() - 1]))
}

/// Whether a cut between `before` and `after` leaves words whole: not both are letters,
/// digits or `_`.
fn clean_cut(before: Option<char>, after: Option<char>) -> bool {
    let word = |c: char| c.is_alphanumeric() || c == '_';
    !(before.is_some_and(word) && after.is_some_and(word))
}

/// Whether `word` ends with `part`, cut cleanly.
fn ends_with_cut(word: &str, part: &str) -> bool {
    word.strip_suffix(part)
        .is_some_and(|rest| clean_cut(rest.chars().next_back(), part.chars().next()))
}

/// Whether `word` starts with `part`, cut cleanly.
fn starts_with_cut(word: &str, part: &str) -> bool {
    word.strip_prefix(part)
        .is_some_and(|rest| clean_cut(part.chars().next_back(), rest.chars().next()))
}

/// Whether `part` is in `word`, cut cleanly at both ends.
fn inside(word: &str, part: &str) -> bool {
    word.match_indices(part).any(|(i, _)| {
        clean_cut(word[..i].chars().next_back(), part.chars().next())
            && clean_cut(
                part.chars().next_back(),
                word[i + part.len()..].chars().next(),
            )
    })
}

/// The `needle.len()` file lines most like `needle`, compared without surrounding
/// whitespace, when at least half of its non-blank lines appear there: where they are, a
/// short diff, and other places that are as close. Lines that are rare in the file count
/// for more, so a window isn't picked for its closing brackets.
fn closest_lines(lines: &[&str], needle: &[&str], cancel: &CancellationToken) -> Option<String> {
    let bodies: Vec<&str> = lines.iter().map(|l| l.trim_end()).collect();
    let w = needle.len().min(bodies.len());
    // Numbers for `needle`'s non-blank lines and the file's lines, compared trimmed; how
    // often each of those lines occurs in `needle` and in the file.
    let kept: Vec<&str> = needle
        .iter()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect();
    let trimmed: Vec<&str> = bodies.iter().map(|l| l.trim()).collect();
    let (needle_ids, file_ids) = ids(&kept, &trimmed);
    let total = needle_ids.len();
    if w == 0 || total == 0 {
        return None;
    }
    let line_ids: Vec<Option<usize>> = file_ids
        .iter()
        .map(|&id| (id != u32::MAX).then_some(id as usize))
        .collect();
    let distinct = needle_ids.iter().max().map_or(0, |&m| m as usize + 1);
    let mut need = vec![0; distinct];
    for &id in &needle_ids {
        need[id as usize] += 1;
    }
    let mut in_file = vec![0; distinct];
    for &id in line_ids.iter().flatten() {
        in_file[id] += 1;
    }
    let weight: Vec<u64> = in_file.iter().map(|&k: &u64| WEIGHT / k.max(1)).collect();

    // For each window, how many of those lines it has (each counted at most as often as
    // in `needle`) and their weight, kept up to date as the window slides.
    let mut have = vec![0; need.len()];
    let (mut count, mut score) = (0, 0);
    let mut windows: Vec<(usize, u64)> = Vec::with_capacity(bodies.len() + 1 - w);
    for end in 0..bodies.len() {
        if cancelled_at(end, cancel) {
            return None;
        }
        if end >= w
            && let Some(id) = line_ids[end - w]
        {
            if have[id] <= need[id] {
                count -= 1;
                score -= weight[id];
            }
            have[id] -= 1;
        }
        if let Some(id) = line_ids[end] {
            have[id] += 1;
            if have[id] <= need[id] {
                count += 1;
                score += weight[id];
            }
        }
        if end + 1 >= w {
            windows.push((count, score));
        }
    }
    let close = |&(count, _): &(usize, u64)| count > 0 && count * 2 >= total;
    let best = windows.iter().filter(|x| close(x)).map(|x| x.1).max()?;

    // Overlapping windows with the best score are one place. In each, take the window
    // whose lines line up with `needle` best (the earliest on a tie), comparing a
    // bounded number of them so a long run of ties stays cheap.
    let mut places: Vec<Vec<usize>> = Vec::new();
    for (start, _) in windows
        .iter()
        .enumerate()
        .filter(|(_, x)| close(x) && x.1 == best)
    {
        match places.last_mut() {
            Some(place) if start < place[place.len() - 1] + w => place.push(start),
            _ => places.push(vec![start]),
        }
    }
    let per_place = (ALIGN_BUDGET / w).clamp(1, 64);
    let aligned = |place: &[usize]| {
        let mut pick = (0, place[0]);
        for &start in place.iter().take(per_place) {
            let same = needle
                .iter()
                .zip(&bodies[start..start + w])
                .filter(|(p, f)| !p.trim().is_empty() && p.trim() == f.trim())
                .count();
            if same > pick.0 {
                pick = (same, start);
            }
        }
        pick.1
    };
    let start = aligned(&places[0]);
    let window = &bodies[start..start + w];

    let old: Vec<&str> = needle.iter().map(|l| l.trim()).collect();
    let new: Vec<&str> = window.iter().map(|l| l.trim()).collect();
    let diff = TextDiff::configure()
        .timeout(Duration::from_millis(200))
        .diff_slices(&old, &new);
    let mut shown = Vec::new();
    // Whether an unchanged line differs in indentation (shown as in the file).
    let mut moved = false;
    for (g, group) in diff.grouped_ops(2).iter().enumerate() {
        if g > 0 {
            shown.push(" …".to_string());
        }
        for op in group {
            for change in diff.iter_changes(op) {
                let (sign, text) = match (change.tag(), change.old_index(), change.new_index()) {
                    (ChangeTag::Delete, Some(i), _) => ('-', needle[i].trim_end()),
                    (ChangeTag::Insert, _, Some(j)) => ('+', window[j]),
                    (ChangeTag::Equal, Some(i), Some(j)) => {
                        moved |= needle[i].trim_end() != window[j];
                        (' ', window[j])
                    }
                    _ => continue,
                };
                shown.push(format!("{sign}{}", short(text)));
            }
        }
    }
    let ignored = if moved {
        " (indentation ignored; unchanged lines as in the file)"
    } else {
        ""
    };
    let mut msg = format!(
        "The closest part of the file is {} ({} of old_text's {total} non-blank lines \
         appear there). Diff from old_text (-) to the file (+){ignored}:\n{}",
        lines_label(start, w),
        windows[start].0,
        render(shown.into_iter())
    );
    if places.len() > 1 {
        let others: Vec<usize> = places[1..]
            .iter()
            .enumerate()
            .map(|(i, p)| if i < 10 { aligned(p) } else { p[0] })
            .collect();
        msg.push_str(&format!("\nAs close: {}.", places_label(&others, w)));
    }
    Some(msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hint(file: &str, needle: &str) -> String {
        let lines: Vec<&str> = file.split_inclusive('\n').collect();
        not_found("a.rs", &lines, needle, false, &CancellationToken::new())
    }

    fn closest(file: &[&str], needle: &[&str]) -> Option<String> {
        closest_lines(file, needle, &CancellationToken::new())
    }

    #[test]
    fn hints() {
        let hay = "fn main() {\n    let x = 1;\n    let y = 2;\n}\n";
        let h = hint(hay, "let  x = 1;\nlet y = 2;");
        assert_eq!(
            h,
            format!(
                "old_text was not found in `a.rs`. Ignoring whitespace, it matches lines 2–3, \
                 which read:\n    let x = 1;\n    let y = 2;\n{ADVICE}"
            )
        );
        let h = hint(hay, "    let x = 1;\n    let y = 3;");
        assert!(h.contains("closest part of the file is lines 2–3"), "{h}");
        assert!(h.contains("-    let y = 3;\n+    let y = 2;"), "{h}");
        let h = hint(hay, "nothing like it");
        assert_eq!(h, format!("old_text was not found in `a.rs`. {ADVICE}"));
    }

    #[test]
    fn loose_matches_whole_words_only() {
        // `x  = 1` is not inside `max = 1`.
        let h = hint("fn f() {\n    max = 1\n}\n", "x  = 1");
        assert_eq!(h, format!("old_text was not found in `a.rs`. {ADVICE}"));
        let h = hint("fn f() {\n    x = 1\n}\n", "x  = 1");
        assert!(
            h.contains("it matches line 2, which read:\n    x = 1\n"),
            "{h}"
        );
    }

    #[test]
    fn loose_matches_part_of_a_line() {
        // old_text may start or end inside a line, next to punctuation.
        let h = hint("fn f() {\n    x = foo(a, b);\n}\n", "foo(a,  b)");
        assert!(
            h.contains("it matches line 2, which read:\n    x = foo(a, b);\n"),
            "{h}"
        );
        let h = hint(
            "fn f() {\n    let total = price  * qty;\n}\n",
            "total = price * qty",
        );
        assert!(
            h.contains("it matches line 2, which read:\n    let total = price  * qty;\n"),
            "{h}"
        );
        // One word: inside a file's word, between punctuation.
        let h = hint("a\n  call(x,y);\n", "\n x,y \n");
        assert!(
            h.contains("it matches lines 1–2, which read:\na\n  call(x,y);\n"),
            "{h}"
        );
        // Not between letters.
        let h = hint("a\n  maximum = 1\n", "\n xim \n");
        assert_eq!(h, format!("old_text was not found in `a.rs`. {ADVICE}"));
    }

    #[test]
    fn blank_lines_at_the_ends_are_shown() {
        // old_text expects blank lines around the call; the file has others there.
        let h = hint("a\nb\ncall(1,\n  2)\nc\n", "\n\ncall(1, 2)\n\n");
        assert!(
            h.contains("it matches lines 1–5, which read:\na\nb\ncall(1,\n  2)\nc\n"),
            "{h}"
        );
    }

    #[test]
    fn closest_lines_caps_the_diff_and_long_lines() {
        let file: Vec<String> = (0..200).map(|i| format!("line {i}\n")).collect();
        let file: Vec<&str> = file.iter().map(String::as_str).collect();
        // Every third line differs, so the diff has no long unchanged runs to skip.
        let needle: Vec<String> = (0..120)
            .map(|i| {
                if i % 3 == 0 {
                    format!("other {i} {}", "x".repeat(400))
                } else {
                    format!("line {i}")
                }
            })
            .collect();
        let needle: Vec<&str> = needle.iter().map(String::as_str).collect();
        let h = closest(&file, &needle).unwrap();
        assert!(
            h.starts_with("The closest part of the file is lines 1–120"),
            "{h}"
        );
        assert!(h.contains("(80 of old_text's 120 non-blank lines"), "{h}");
        assert!(h.contains("more lines"), "{h}");
        assert!(h.lines().count() < MAX_HINT_LINES + 5, "{h}");
        assert!(h.lines().all(|l| l.chars().count() <= 200), "{h}");
        assert!(h.contains("chars truncated"), "{h}");
    }

    #[test]
    fn closest_lines_needs_half_the_lines() {
        let file = ["a", "b", "c", "d"];
        assert!(closest(&file, &["a", "x", "y"]).is_none());
        assert!(closest(&file, &["a", "b", "y"]).is_some());
        assert!(closest(&file, &["", " "]).is_none());
    }

    #[test]
    fn closest_lines_picks_the_aligned_window_and_lists_ties() {
        // Windows shifted by one line score the same; the aligned one is shown.
        let file = ["z", "a", "b", "c", "d", "q", "q", "q", "a", "b", "c", "e"];
        let h = closest(&file, &["a", "b", "c", "x"]).unwrap();
        assert!(h.contains("lines 2–5"), "{h}");
        assert!(h.contains(":\n b\n c\n-x\n+d\n"), "{h}");
        assert!(h.contains("As close: lines 9–12."), "{h}");
    }

    #[test]
    fn rare_lines_outweigh_common_ones() {
        // The block of `}` lines below has more of old_text's lines (5 against 4), but
        // the two rare lines at the top weigh more.
        let file = [
            "fn one() {",
            "    body();",
            "    more();",
            "}",
            "}",
            "x",
            "x",
            "x",
            "}",
            "}",
            "}",
            "}",
            "}",
            "x",
            "x",
            "x",
        ];
        let needle = [
            "fn one() {",
            "    body();",
            "    more ();",
            "}",
            "}",
            "}",
            "}",
            "}",
        ];
        let h = closest(&file, &needle).unwrap();
        assert!(
            h.contains("closest part of the file is lines 1–8 (4 of old_text's 8"),
            "{h}"
        );
        assert!(!h.contains("As close"), "{h}");
    }

    #[test]
    fn cancelled_hints_stop() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let lines = ["a\n", "b\n"];
        assert!(loose_match(&lines, "a b", &cancel).is_none());
        assert!(closest_lines(&lines, &["a", "x"], &cancel).is_none());
    }
}
