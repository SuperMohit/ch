//! The fallback for an `old_text` with no exact match. When, ignoring whitespace at both
//! ends of lines, it occurs at exactly one place, and one swap of indentation turns it
//! into the file's lines there, the edit is applied re-indented. Otherwise nothing is
//! changed and the refusal says why.

use ch_kernel::reexports::CancellationToken;

use super::hints::{self, ADVICE};
use super::lines::{
    as_lines, body, describe_chars, describe_indent, find_all, ids, indent, lines_label,
    places_label,
};

/// Why the fallback changed nothing: the failure reason for metadata and the error.
pub(super) struct Refusal {
    pub reason: &'static str,
    pub message: String,
}

impl Refusal {
    fn new(reason: &'static str, message: String) -> Self {
        Self { reason, message }
    }
}

/// For an `old_text` with no exact match in `file`: the edited file and a note saying
/// what was adjusted, or why nothing was changed. Never applies with `replace_all`.
pub(super) fn edit(
    rel: &str,
    file: &str,
    old_text: &str,
    new_text: &str,
    replace_all: bool,
    cancel: &CancellationToken,
) -> Result<(String, String), Refusal> {
    let lines: Vec<&str> = file.split_inclusive('\n').collect();
    let result = fallback(rel, file, &lines, old_text, new_text, replace_all, cancel);
    if cancel.is_cancelled() {
        return Err(Refusal::new("cancelled", "Cancelled.".into()));
    }
    result
}

/// [`edit`], before the last check for cancellation.
fn fallback(
    rel: &str,
    file: &str,
    lines: &[&str],
    old_text: &str,
    new_text: &str,
    replace_all: bool,
    cancel: &CancellationToken,
) -> Result<(String, String), Refusal> {
    let not_found = || {
        let message = hints::not_found(rel, lines, old_text, replace_all, cancel);
        Refusal::new("not_found", message)
    };
    let raw = as_lines(old_text);
    if replace_all || raw.iter().all(|l| l.trim().is_empty()) {
        return Err(not_found());
    }
    // Every place where old_text's lines occur with whitespace at their ends ignored,
    // found in linear time.
    let trimmed: Vec<&str> = raw.iter().map(|l| l.trim()).collect();
    let file_trimmed: Vec<&str> = lines.iter().map(|l| l.trim()).collect();
    let (pattern_ids, file_ids) = ids(&trimmed, &file_trimmed);
    let Some(starts) = find_all(&file_ids, &pattern_ids, cancel) else {
        return Err(Refusal::new("cancelled", "Cancelled.".into()));
    };
    let n = raw.len();
    let start = match starts[..] {
        [] => return Err(not_found()),
        [start] => start,
        _ => {
            return Err(Refusal::new(
                "whitespace_ambiguous",
                format!(
                    "old_text was not found in `{rel}` exactly. Ignoring whitespace it \
                     matches {} places ({}), so nothing was changed. Include more \
                     surrounding lines to make it unique, copied exactly as read_file shows \
                     them.",
                    starts.len(),
                    places_label(&starts, n)
                ),
            ));
        }
    };
    let window = &lines[start..start + n];
    let matched = Pattern::new(old_text).and_then(|p| p.target(window).map(|t| (p, t)));
    let Some((pattern, target)) = matched else {
        return Err(Refusal::new(
            "not_reindentable",
            format!(
                "old_text was not found in `{rel}` exactly. Ignoring whitespace it matches \
                 {}, but no single change of indentation turns it into those lines, which \
                 read:\n{}\n{ADVICE}",
                lines_label(start, n),
                hints::verbatim(lines, start, start + n - 1)
            ),
        ));
    };
    pattern.apply(rel, file, lines, start, target, new_text)
}

/// old_text as whole lines, matched against a window of as many file lines.
///
/// The window matches when, with trailing whitespace ignored, blank lines pair with blank
/// lines and every other line differs only by one swap of indentation: `base`, the
/// indentation of old_text's least-indented line, stands for the indentation of the file
/// line in its place, and what follows `base` on each line must be in the file after that
/// indentation.
struct Pattern<'a> {
    /// The lines as given, without line endings.
    raw: Vec<&'a str>,
    /// The same without trailing whitespace.
    lines: Vec<&'a str>,
    /// Index of the least-indented non-blank line.
    anchor: usize,
    base: &'a str,
}

impl<'a> Pattern<'a> {
    /// `None` when old_text is blank or a line's indentation doesn't start with `base`.
    fn new(old_text: &'a str) -> Option<Self> {
        let raw = as_lines(old_text);
        let lines: Vec<&str> = raw.iter().copied().map(str::trim_end).collect();
        let (anchor, base) = lines
            .iter()
            .enumerate()
            .filter(|(_, l)| !l.is_empty())
            .map(|(i, l)| (i, indent(l)))
            .min_by_key(|(_, ind)| ind.len())?;
        lines
            .iter()
            .all(|l| l.is_empty() || l.starts_with(base))
            .then_some(Self {
                raw,
                lines,
                anchor,
                base,
            })
    }

    /// The file's indentation in place of `base`, if `window` (as many file lines, with
    /// their endings) matches.
    fn target<'f>(&self, window: &[&'f str]) -> Option<&'f str> {
        let target = indent(window[self.anchor].trim_end());
        let base = self.base.len();
        self.lines
            .iter()
            .zip(window)
            .all(|(p, f)| {
                let f = f.trim_end();
                if p.is_empty() {
                    f.is_empty()
                } else {
                    f.strip_prefix(target) == Some(&p[base..])
                }
            })
            .then_some(target)
    }

    /// Replaces the lines `new_text` changes in the window at `start` of `lines` (the
    /// file's lines with their endings), re-indented the same way and with the file's
    /// line endings. Lines it leaves as they were keep their bytes.
    fn apply(
        &self,
        rel: &str,
        file: &str,
        lines: &[&str],
        start: usize,
        target: &str,
        new_text: &str,
    ) -> Result<(String, String), Refusal> {
        let n = self.lines.len();
        let window = &lines[start..start + n];
        let swap = self.base != target;
        let place = lines_label(start, n);
        let shift = format!(
            "`{}` → `{}`",
            describe_indent(self.base),
            describe_indent(target)
        );
        let refuse = |reason, why: String| {
            Err(Refusal::new(
                reason,
                format!(
                    "old_text was not found in `{rel}` exactly. It matches {place} with the \
                     indentation changed ({shift}), but {why}"
                ),
            ))
        };
        let new_lines = if new_text.is_empty() {
            Vec::new()
        } else {
            as_lines(new_text)
        };
        // A new_text that only changes indentation sets it; shifting it as well would be
        // wrong.
        if swap
            && new_lines.len() == n
            && self
                .raw
                .iter()
                .zip(&new_lines)
                .all(|(o, l)| o.trim() == l.trim())
        {
            return refuse(
                "indentation_only",
                "new_text changes only indentation, so it isn't clear which indentation you \
                 want. Copy old_text with the file's indentation exactly as read_file shows \
                 it, and put the indentation you want in new_text."
                    .into(),
            );
        }

        // Lines at either end that new_text leaves as they were stay as they are in the
        // file; only the lines between are replaced.
        let kept_head = self
            .raw
            .iter()
            .zip(&new_lines)
            .take_while(|(o, l)| o == l)
            .count();
        let kept_tail = self.raw[kept_head..]
            .iter()
            .rev()
            .zip(new_lines[kept_head..].iter().rev())
            .take_while(|(o, l)| o == l)
            .count();
        // Re-indented lines may only use whitespace the file indents with here (or
        // anywhere, if these lines aren't indented), so tabs and spaces don't mix.
        let allowed = indent_chars(window).or_else(|| indent_chars(lines));
        let mut middle = Vec::new();
        for (i, line) in new_lines
            .iter()
            .enumerate()
            .take(new_lines.len() - kept_tail)
            .skip(kept_head)
        {
            if line.trim().is_empty() {
                middle.push(String::new());
                continue;
            }
            if !swap {
                middle.push(line.to_string());
                continue;
            }
            let Some(rest) = line.strip_prefix(self.base) else {
                return refuse(
                    "not_reindentable",
                    format!(
                        "new_text line {} doesn't start with old_text's indentation `{}`, so \
                         it can't be re-indented the same way. {ADVICE}",
                        i + 1,
                        describe_indent(self.base)
                    ),
                );
            };
            if let Some(allowed) = &allowed {
                let mut foreign: Vec<char> = Vec::new();
                for c in indent(rest).chars() {
                    if !allowed.contains(&c) && !foreign.contains(&c) {
                        foreign.push(c);
                    }
                }
                if !foreign.is_empty() {
                    return refuse(
                        "not_reindentable",
                        format!(
                            "the file indents these lines with {} and new_text line {} is \
                             indented further with {}, so re-indenting it would mix them. \
                             Copy the lines exactly as read_file shows them and indent \
                             new_text the same way.",
                            describe_chars(allowed),
                            i + 1,
                            describe_chars(&foreign)
                        ),
                    );
                }
            }
            middle.push(format!("{target}{rest}"));
        }

        let (a, b) = (start + kept_head, start + n - kept_tail);
        let eol = if window[0].ends_with("\r\n")
            || (!window[0].ends_with('\n') && file.contains("\r\n"))
        {
            "\r\n"
        } else {
            "\n"
        };
        let offset = |i: usize| -> usize { lines[..i].iter().map(|l| l.len()).sum() };
        let (mut from, to) = (offset(a), offset(b));
        let text = if middle.is_empty() {
            // Only removed lines. If they end the file without a final line break, the
            // line before loses its break instead.
            if a < b && a > 0 && !lines[b - 1].ends_with('\n') {
                from -= lines[a - 1].len() - body(lines[a - 1]).len();
            }
            String::new()
        } else if a == b {
            // Only inserted lines.
            if a > 0 && !lines[a - 1].ends_with('\n') {
                format!("{eol}{}", middle.join(eol))
            } else {
                middle.iter().map(|l| format!("{l}{eol}")).collect()
            }
        } else {
            let mut text = middle.join(eol);
            if lines[b - 1].ends_with('\n') {
                text.push_str(eol);
            }
            text
        };
        let edited = format!("{}{text}{}", &file[..from], &file[to..]);

        let trailing = self.raw.iter().zip(window).any(|(p, f)| {
            let f = body(f);
            p[p.trim_end().len()..] != f[f.trim_end().len()..]
        });
        let mut what = Vec::new();
        if swap {
            what.push(format!("indentation {shift}"));
        }
        if trailing {
            what.push("trailing whitespace ignored".to_string());
        }
        if what.is_empty() {
            what.push("line endings ignored".to_string());
        }
        let reindented = if swap {
            "; new_text was re-indented the same way"
        } else {
            ""
        };
        let note = format!(
            "old_text matched {place} only after adjusting whitespace ({}){reindented}. Check \
             the diff.",
            what.join(", ")
        );
        Ok((edited, note))
    }
}

/// The whitespace characters `lines` are indented with, in order of first use; `None`
/// if no non-blank line is indented.
fn indent_chars(lines: &[&str]) -> Option<Vec<char>> {
    let mut chars = Vec::new();
    for line in lines.iter().map(|l| l.trim_end()).filter(|l| !l.is_empty()) {
        for c in indent(line).chars() {
            if !chars.contains(&c) {
                chars.push(c);
            }
        }
    }
    (!chars.is_empty()).then_some(chars)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(file: &str, old: &str, new: &str) -> Result<(String, String), Refusal> {
        edit("a.txt", file, old, new, false, &CancellationToken::new())
    }

    fn applied(file: &str, old: &str, new: &str) -> String {
        match run(file, old, new) {
            Ok((edited, _)) => edited,
            Err(r) => panic!("{}: {}", r.reason, r.message),
        }
    }

    fn refused(file: &str, old: &str, new: &str) -> Refusal {
        match run(file, old, new) {
            Ok((edited, _)) => panic!("applied: {edited:?}"),
            Err(r) => r,
        }
    }

    #[test]
    fn pattern_rule() {
        let p = Pattern::new("  a\n    b\n\n  c\n").unwrap();
        assert_eq!((p.anchor, p.base, p.lines.len()), (0, "  ", 4));
        // Base 2 spaces -> 1 tab, nested remainder kept; trailing whitespace ignored.
        assert_eq!(p.target(&["\ta\n", "\t  b \n", "\n", "\tc"]), Some("\t"));
        // Blank lines pair with blank lines only.
        assert_eq!(p.target(&["\ta\n", "\t  b\n", "x\n", "\tc\n"]), None);
        // The nested remainder must be the same.
        assert_eq!(p.target(&["\ta\n", "\t\tb\n", "\n", "\tc\n"]), None);
        // Text after the indentation must be equal.
        assert_eq!(p.target(&["\ta\n", "\t  B\n", "\n", "\tc\n"]), None);
        // No single prefix: a tab line and a space line.
        assert!(Pattern::new("\ta\n    b").is_none());
        assert!(Pattern::new("\n  \n").is_none());
    }

    #[test]
    fn keeps_a_missing_final_line_break() {
        let file = "a:\n        x = 1\n        y = 2";
        assert_eq!(
            applied(file, "    x = 1\n    y = 2\n", "    x = 1\n    y = 3\n"),
            "a:\n        x = 1\n        y = 3"
        );
        // Removing the last lines leaves none behind either.
        assert_eq!(applied(file, "x = 1\ny = 2", ""), "a:");
    }

    #[test]
    fn crlf_last_line_without_a_line_break() {
        let file = "x:\r\n    a = 1\r\n    b = 2";
        assert_eq!(
            applied(file, "a = 1\nb = 2", "a = 1\nb = 3\nc = 4"),
            "x:\r\n    a = 1\r\n    b = 3\r\n    c = 4"
        );
    }

    #[test]
    fn deletes_and_inserts_whole_lines() {
        let file = "keep\n        a = 1\n        b = 2\nend\n";
        assert_eq!(applied(file, "    a = 1\n    b = 2", ""), "keep\nend\n");
        // Only inserted lines, between kept ones.
        assert_eq!(
            applied(
                file,
                "    a = 1\n    b = 2",
                "    a = 1\n    new = 0\n    b = 2"
            ),
            "keep\n        a = 1\n        new = 0\n        b = 2\nend\n"
        );
        // Inserted after the last line of a file without a final line break.
        assert_eq!(
            applied("k\n    a = 1", "a = 1\n", "a = 1\nb = 2\n"),
            "k\n    a = 1\n    b = 2"
        );
    }

    #[test]
    fn blank_lines_at_either_end_of_old_text() {
        let file = "x:\n\n        a\n        b\n\ny\n";
        assert_eq!(
            applied(file, "\n    a\n    b\n\n", "\n    a\n    c\n\n"),
            "x:\n\n        a\n        c\n\ny\n"
        );
        // The blank lines are part of what must match.
        let r = refused(
            "x:\n        a\n        b\ny\n",
            "\n    a\n    b\n\n",
            "\n    a\n",
        );
        assert_eq!(r.reason, "not_found");
    }

    #[test]
    fn new_blank_lines_are_empty() {
        assert_eq!(
            applied("f:\n\ta\n\tb\n", "    a\n    b", "    a\n    \n    b"),
            "f:\n\ta\n\n\tb\n"
        );
    }

    #[test]
    fn unchanged_lines_keep_their_bytes() {
        // Markdown hard breaks (two trailing spaces) on lines old_text has without them.
        let file = "Intro:\n    line one  \n    line two  \n    line three\n";
        assert_eq!(
            applied(
                file,
                "  line one\n  line two\n  line three",
                "  line one\n  line two\n  line 3"
            ),
            "Intro:\n    line one  \n    line two  \n    line 3\n"
        );
        // A form feed on a blank line survives an edit next to it.
        let file = "start:\n        a = 1\n\x0c\n        b = 2\n";
        assert_eq!(
            applied(file, "    a = 1\n\n    b = 2", "    a = 1\n\n    b = 3"),
            "start:\n        a = 1\n\x0c\n        b = 3\n"
        );
    }

    #[test]
    fn mixing_tabs_and_spaces_is_refused() {
        let file = "start {\n\tif x {\n\t\ty();\n\t}\n}\n";
        let old = "if x {\n\ty();\n}";
        // Nested further with spaces where the file uses tabs.
        let r = refused(file, old, "if x {\n\ty();\n    w = 0;\n}");
        assert_eq!(r.reason, "not_reindentable");
        assert!(
            r.message.contains(
                "the file indents these lines with tabs and new_text line 3 is indented \
                 further with spaces"
            ),
            "{}",
            r.message
        );
        // With tabs it is fine.
        assert_eq!(
            applied(file, old, "if x {\n\ty();\n\tw = 0;\n}"),
            "start {\n\tif x {\n\t\ty();\n\t\tw = 0;\n\t}\n}\n"
        );
    }

    #[test]
    fn the_whitespace_free_form_must_be_unique() {
        // The method's continuation line is aligned like old_text's; the function's
        // isn't, but ignoring whitespace both match, so the edit could be meant for either.
        let file = "def a():\n    return f(1,\n             2)\n\nclass C:\n    def a(self):\n        \
                    return f(1,\n            2)\n";
        let r = refused(file, "  return f(1,\n      2)", "  return f(1,\n      3)");
        assert_eq!(r.reason, "whitespace_ambiguous");
        assert!(
            r.message.contains("matches 2 places (lines 2–3, 7–8)"),
            "{}",
            r.message
        );
    }

    #[test]
    fn indentation_only_changes_are_refused() {
        let file = "list:\n        a = 1\n        b = 2\n";
        let r = refused(file, "    a = 1\n    b = 2", "        a = 1\n        b = 2");
        assert_eq!(r.reason, "indentation_only");
        // Without a shift, new_text's indentation is used as given.
        assert_eq!(
            applied(
                "l:\n    a = 1  \n    b = 2\n",
                "    a = 1\n    b = 2",
                "  a = 1\n  b = 2"
            ),
            "l:\n  a = 1\n  b = 2\n"
        );
    }

    #[test]
    fn inconsistent_indentation_shows_the_lines() {
        let file = "start {\n\tif x {\n\t\ty();\n\t}\n}\n";
        let r = refused(file, "    if x {\n        y();\n    }", "x");
        assert_eq!(r.reason, "not_reindentable");
        assert!(
            r.message
                .contains("matches lines 2–4, but no single change of indentation"),
            "{}",
            r.message
        );
        assert!(
            r.message.contains("read:\n\tif x {\n\t\ty();\n\t}\n"),
            "{}",
            r.message
        );
    }

    #[test]
    fn cancelled_search_changes_nothing() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let r = edit("a.txt", "x:\n    a\n", "a\n", "b\n", false, &cancel).unwrap_err();
        assert_eq!(r.reason, "cancelled");
    }
}
