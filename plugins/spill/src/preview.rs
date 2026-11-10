//! The head, tail and notice that replace a spilled result.

/// How much of one line `read_file` shows (tools-fs `MAX_LINE_CHARS`). Longer lines need
/// another way to read their middle.
const READ_FILE_LINE_CHARS: usize = 2000;

/// The tools the notice may point at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hint {
    /// `read_file` and `grep_text`, and whether `bash` is there for long lines.
    FileTools { bash: bool },
    /// Only `bash`.
    Bash,
    /// Nothing that can read a file.
    None,
}

/// `content`'s head and tail around a notice naming `path`, at most `max` characters
/// in all. Cuts fall on line breaks when one is near. `None` when the notice alone
/// leaves too little room.
pub fn preview(content: &str, path: &str, hint: Hint, max: usize) -> Option<String> {
    let total = content.chars().count();
    let lines = line_count(content);
    let quirks = Quirks {
        long_lines: content
            .split('\n')
            .any(|l| l.chars().count() > READ_FILE_LINE_CHARS),
        nul: content.contains('\0'),
    };
    // Sized with the longest range text and the largest numbers it can hold, so the
    // real notice is never longer.
    let widest_range = format!("lines {lines}–{lines}");
    let widest = notice(&widest_range, lines, lines, total, path, hint, quirks);
    let keep = max.checked_sub(widest.chars().count() + "\n\n".len() * 2)?;
    if keep < 100 || keep >= total {
        return None;
    }

    let head_budget = keep / 2;
    let tail_budget = keep - head_budget;
    let mut head_end = byte_at(content, head_budget);
    if let Some(nl) = content[..head_end].rfind('\n')
        && content[..=nl].chars().count() >= head_budget / 2
    {
        head_end = nl + 1;
    }
    let mut tail_start = byte_at(content, total - tail_budget);
    if let Some(nl) = content[tail_start..].find('\n') {
        let snapped = tail_start + nl + 1;
        if snapped < content.len()
            && content[tail_start..snapped].chars().count() <= tail_budget / 2
        {
            tail_start = snapped;
        }
    }

    let head = &content[..head_end];
    let tail = &content[tail_start..];
    let omitted = total - head.chars().count() - tail.chars().count();
    let first = head.matches('\n').count() + 1;
    // The line holding the last omitted character; a newline belongs to its line.
    let before_tail = &content[..tail_start];
    let breaks = before_tail.matches('\n').count();
    let last = if before_tail.ends_with('\n') {
        breaks
    } else {
        breaks + 1
    };
    let range = if first == last {
        format!("line {first}")
    } else {
        format!("lines {first}–{last}")
    };
    let notice = notice(&range, first, lines, omitted, path, hint, quirks);
    // A blank line on each side of the notice.
    let gap = if head.ends_with('\n') { "\n" } else { "\n\n" };
    Some(format!("{head}{gap}{notice}\n\n{tail}"))
}

/// What the file tools can't read in the saved output.
#[derive(Clone, Copy)]
struct Quirks {
    /// Some line is longer than `read_file` shows.
    long_lines: bool,
    /// A NUL byte, which makes `read_file` and `grep_text` treat the file as binary.
    nul: bool,
}

fn notice(
    range: &str,
    first: usize,
    lines: usize,
    omitted: usize,
    path: &str,
    hint: Hint,
    quirks: Quirks,
) -> String {
    let how = match hint {
        Hint::FileTools { .. } => format!(
            "Read the rest with read_file with offset={first} (and a limit), or search it \
             with grep_text on that path."
        ),
        Hint::Bash => format!(
            "Read the rest with `tail -n +{first} <path> | head -n 200`, or search it with \
             `grep -n` in bash."
        ),
        Hint::None => "It is kept for the user.".to_string(),
    };
    let bash = matches!(hint, Hint::Bash | Hint::FileTools { bash: true });
    let mut extra = String::new();
    if quirks.long_lines && hint != Hint::None {
        extra.push_str(&format!(
            " Some lines are longer than read_file shows ({READ_FILE_LINE_CHARS} \
             characters)"
        ));
        extra.push_str(if bash {
            "; read part of line N with `head -n N <path> | tail -n 1 | cut -c A-B` in \
             bash."
        } else {
            "."
        });
    }
    if quirks.nul && hint != Hint::None {
        extra.push_str(" It contains NUL bytes, which read_file and grep_text treat as binary");
        extra.push_str(if bash {
            "; in bash, `cat -v <path> | tail -n +N | head -n 200` shows them as ^@."
        } else {
            "."
        });
    }
    format!(
        "[… {range} of {lines} omitted ({omitted} characters). The full output is saved \
         at {path}.\n{how}{extra}]"
    )
}

/// Lines in `s`, counting a last line without a newline.
fn line_count(s: &str) -> usize {
    let breaks = s.matches('\n').count();
    if s.is_empty() || s.ends_with('\n') {
        breaks
    } else {
        breaks + 1
    }
}

/// The byte offset of the `n`th character (or the end).
fn byte_at(s: &str, n: usize) -> usize {
    s.char_indices().nth(n).map_or(s.len(), |(i, _)| i)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILES: Hint = Hint::FileTools { bash: true };

    fn log(lines: usize) -> String {
        (1..=lines).map(|i| format!("log line {i:05}\n")).collect()
    }

    /// The lines of `text` around its notice, as numbers.
    fn kept_lines(text: &str) -> (Vec<usize>, Vec<usize>) {
        let (head, rest) = text.split_once("\n\n[… ").unwrap();
        let tail = rest.split_once("]\n\n").unwrap().1;
        let nums = |s: &str| {
            s.lines()
                .map(|l| l.trim_start_matches("log line ").parse().unwrap())
                .collect()
        };
        (nums(head), nums(tail))
    }

    #[test]
    fn cuts_fall_on_lines_and_the_notice_names_the_omitted_ones() {
        let content = log(5000);
        let text = preview(&content, "/s/spill/x.txt", FILES, 2000).unwrap();
        assert!(text.chars().count() <= 2000, "{}", text.chars().count());
        let (head, tail) = kept_lines(&text);
        assert_eq!(head[0], 1);
        assert_eq!(*tail.last().unwrap(), 5000);
        let (first, last) = (head.last().unwrap() + 1, tail[0] - 1);
        assert!(
            text.contains(&format!("[… lines {first}–{last} of 5000 omitted (")),
            "{text}"
        );
        assert!(
            text.contains(&format!("read_file with offset={first}")),
            "{text}"
        );
        assert!(!text.contains("longer than read_file"));
        let omitted = (first..=last)
            .map(|i| format!("log line {i:05}\n").len())
            .sum::<usize>();
        assert!(text.contains(&format!("({omitted} characters)")), "{text}");
    }

    #[test]
    fn it_never_exceeds_the_budget() {
        let path = "/a/very/long/path/".repeat(10);
        for content in [
            log(3000),
            "é".repeat(30_000),
            "x".repeat(30_000),
            log(1) + &"y".repeat(9000),
            "y".repeat(30_000) + "\n" + &"z".repeat(30_000),
            "a\0b\n".repeat(20_000),
        ] {
            for max in [1000, 1500, 4000, 24_000] {
                if let Some(text) = preview(&content, &path, FILES, max) {
                    assert!(
                        text.chars().count() <= max,
                        "{max}: {}",
                        text.chars().count()
                    );
                    assert!(text.contains(&path));
                }
            }
        }
    }

    #[test]
    fn one_long_line_is_cut_by_characters_and_says_how_to_read_it() {
        let content = "x".repeat(30_000);
        let text = preview(&content, "/p.txt", FILES, 2000).unwrap();
        assert!(text.contains("[… line 1 of 1 omitted ("), "{text}");
        assert!(
            text.contains("`head -n N <path> | tail -n 1 | cut -c A-B`"),
            "{text}"
        );
        let no_bash = preview(&content, "/p.txt", Hint::FileTools { bash: false }, 2000).unwrap();
        assert!(no_bash.contains("longer than read_file shows (2000 characters).]"));
        assert!(!no_bash.contains("cut -c"));
    }

    #[test]
    fn nul_bytes_are_called_out() {
        let content = "a\0b\n".repeat(20_000);
        let text = preview(&content, "/p.txt", FILES, 2000).unwrap();
        assert!(text.contains("It contains NUL bytes"), "{text}");
        assert!(text.contains("`cat -v <path>"), "{text}");
        assert!(
            !preview(&log(5000), "/p.txt", FILES, 2000)
                .unwrap()
                .contains("NUL")
        );
    }

    #[test]
    fn the_hint_follows_the_tools() {
        let content = log(5000);
        let bash = preview(&content, "/p.txt", Hint::Bash, 2000).unwrap();
        assert!(bash.contains("`tail -n +"), "{bash}");
        assert!(!bash.contains("read_file with"));
        let none = preview(&content, "/p.txt", Hint::None, 2000).unwrap();
        assert!(none.contains("It is kept for the user.]"), "{none}");
    }

    #[test]
    fn a_notice_that_does_not_fit_gives_none() {
        let path = "/p".repeat(600);
        assert_eq!(preview(&log(5000), &path, FILES, 1000), None);
        // Content shorter than what would be kept is not previewed.
        assert_eq!(preview(&log(10), "/p.txt", FILES, 100_000), None);
    }

    #[test]
    fn line_counting() {
        assert_eq!(line_count(""), 0);
        assert_eq!(line_count("a"), 1);
        assert_eq!(line_count("a\n"), 1);
        assert_eq!(line_count("a\nb"), 2);
    }
}
