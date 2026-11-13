//! The fallback for an `old_text` with no exact match. When, ignoring whitespace at both
//! ends of lines, it occurs at exactly one place, and one swap of indentation turns it
//! into the file's lines there, the edit is applied re-indented. Otherwise nothing is
//! changed and the refusal says why.

use std::time::{Duration, Instant};

use ch_kernel::reexports::CancellationToken;
use similar::{Algorithm, DiffOp, capture_diff_slices_deadline};

use super::hints::{self, ADVICE};
use super::lines::{
    as_lines, body, describe_chars, describe_indent, ending, find_all, ids, indent, lines_label,
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
    let lines = &lines[..];
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

    /// Replaces the window at `start` of `lines` (the file's lines with their endings)
    /// with `new_text`, re-indented the same way. Lines `new_text` leaves as they were in
    /// old_text keep the file's bytes, and a changed line keeps the line ending of the
    /// line it replaces.
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
            Refusal::new(
                reason,
                format!(
                    "old_text was not found in `{rel}` exactly. It matches {place} with the \
                     indentation changed ({shift}), but {why}"
                ),
            )
        };
        let new_lines = if new_text.is_empty() {
            Vec::new()
        } else {
            as_lines(new_text)
        };
        if new_lines.len() == n
            && self
                .raw
                .iter()
                .zip(&new_lines)
                .all(|(o, l)| o.trim_end() == l.trim_end())
        {
            return Err(Refusal::new(
                "identical",
                "Apart from trailing whitespace, new_text is the same as old_text, so nothing \
                 would change. Put the changed text in new_text."
                    .into(),
            ));
        }
        // A new_text that only changes indentation (blank lines aside) sets it; shifting
        // it as well would be wrong.
        let (old_kept, new_kept) = (non_blank(&self.raw), non_blank(&new_lines));
        if swap
            && old_kept.len() == new_kept.len()
            && old_kept
                .iter()
                .zip(&new_kept)
                .all(|(o, l)| o.trim() == l.trim())
            && old_kept
                .iter()
                .zip(&new_kept)
                .any(|(o, l)| indent(o) != indent(l))
        {
            return Err(refuse(
                "indentation_only",
                "new_text changes only indentation, so it isn't clear which indentation you \
                 want. Copy old_text with the file's indentation exactly as read_file shows \
                 it, and put the indentation you want in new_text."
                    .into(),
            ));
        }

        // Re-indented lines may only be indented further with the file's indentation
        // here (anywhere in the file, if these lines aren't indented), so tabs and spaces
        // don't mix and alignment spaces aren't taken for indentation.
        let (allowed, whose) = if target.is_empty() {
            (indent_chars(lines), "the file indents")
        } else {
            (Some(distinct_chars(target)), "the file indents these lines")
        };
        let reindent = |i: usize| -> Result<String, Refusal> {
            let line = new_lines[i];
            if line.trim().is_empty() {
                return Ok(String::new());
            }
            if !swap {
                return Ok(line.to_string());
            }
            let Some(rest) = line.strip_prefix(self.base) else {
                return Err(refuse(
                    "not_reindentable",
                    format!(
                        "new_text line {} doesn't start with old_text's indentation `{}`, so \
                         it can't be re-indented the same way. {ADVICE}",
                        i + 1,
                        describe_indent(self.base)
                    ),
                ));
            };
            if let Some(allowed) = &allowed {
                let foreign: Vec<char> = distinct_chars(indent(rest))
                    .into_iter()
                    .filter(|c| !allowed.contains(c))
                    .collect();
                if !foreign.is_empty() {
                    return Err(refuse(
                        "not_reindentable",
                        format!(
                            "{whose} with {} and new_text line {} is indented further with \
                             {}, so re-indenting it would mix them. Copy the lines exactly as \
                             read_file shows them and indent new_text the same way.",
                            describe_chars(allowed),
                            i + 1,
                            describe_chars(&foreign)
                        ),
                    ));
                }
            }
            Ok(format!("{target}{rest}"))
        };

        // The window's new lines, each with its line ending ("" for the default).
        let deadline = Instant::now() + Duration::from_millis(200);
        let ops =
            capture_diff_slices_deadline(Algorithm::Myers, &self.raw, &new_lines, Some(deadline));
        let mut out: Vec<(String, &str)> = Vec::new();
        for op in ops {
            match op {
                DiffOp::Equal { old_index, len, .. } => {
                    for line in &window[old_index..old_index + len] {
                        out.push((body(line).to_string(), ending(line)));
                    }
                }
                DiffOp::Delete { .. } => {}
                DiffOp::Insert {
                    new_index, new_len, ..
                } => {
                    for i in new_index..new_index + new_len {
                        out.push((reindent(i)?, ""));
                    }
                }
                DiffOp::Replace {
                    old_index,
                    old_len,
                    new_index,
                    new_len,
                } => {
                    for k in 0..new_len {
                        let end = if k < old_len {
                            ending(window[old_index + k])
                        } else {
                            ""
                        };
                        out.push((reindent(new_index + k)?, end));
                    }
                }
            }
        }

        let eol = window
            .iter()
            .map(|l| ending(l))
            .find(|e| !e.is_empty())
            .unwrap_or(if file.contains("\r\n") { "\r\n" } else { "\n" });
        // The window ends the file without a final line break: so does its last new line.
        let breaks = window[n - 1].ends_with('\n');
        let mut text = String::new();
        for (i, (line, end)) in out.iter().enumerate() {
            text.push_str(line);
            if i + 1 < out.len() || breaks {
                text.push_str(if end.is_empty() { eol } else { end });
            }
        }
        let offset = |i: usize| -> usize { lines[..i].iter().map(|l| l.len()).sum() };
        let (mut from, to) = (offset(start), offset(start + n));
        if out.is_empty() && !breaks && start > 0 {
            // All of it removed: the line before now ends the file, without a break.
            from -= ending(lines[start - 1]).len();
        }
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
    let indents: String = lines.iter().map(|l| indent(l.trim_end())).collect();
    let chars = distinct_chars(&indents);
    (!chars.is_empty()).then_some(chars)
}

/// The characters of `s`, each once, in order of first use.
fn distinct_chars(s: &str) -> Vec<char> {
    let mut chars = Vec::new();
    for c in s.chars() {
        if !chars.contains(&c) {
            chars.push(c);
        }
    }
    chars
}

/// The lines of `lines` that aren't blank.
fn non_blank<'a>(lines: &[&'a str]) -> Vec<&'a str> {
    lines
        .iter()
        .copied()
        .filter(|l| !l.trim().is_empty())
        .collect()
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
        // Between changed lines, and at the end.
        let file = "Intro:\n    l1  \n    l2  \n    l3  \n";
        assert_eq!(
            applied(file, "  l1\n  l2\n  l3", "  L1\n  l2\n  L3"),
            "Intro:\n    L1\n    l2  \n    L3\n"
        );
        assert_eq!(
            applied(file, "  l1\n  l2\n  l3", "  L1\n  l2\n  l3"),
            "Intro:\n    L1\n    l2  \n    l3  \n"
        );
        // A form feed on a blank line survives edits next to it.
        let file = "start:\n        a = 1\n\x0c\n        b = 2\n";
        assert_eq!(
            applied(file, "    a = 1\n\n    b = 2", "    a = 1\n\n    b = 3"),
            "start:\n        a = 1\n\x0c\n        b = 3\n"
        );
        assert_eq!(
            applied(file, "    a = 1\n\n    b = 2", "    a = 10\n\n    b = 20"),
            "start:\n        a = 10\n\x0c\n        b = 20\n"
        );
    }

    #[test]
    fn replaced_lines_keep_their_line_endings() {
        // A file with mixed endings: the replaced line keeps its CRLF, the kept one its LF.
        assert_eq!(
            applied("k\r\n    a\n    b\r\n", "a\nb", "a\nc"),
            "k\r\n    a\n    c\r\n"
        );
    }

    #[test]
    fn alignment_spaces_are_not_indentation() {
        // The file indents with tabs and aligns with spaces; a new line nested with spaces
        // would mix them.
        let file = "def f(a, b):\n\tresult = foo(a,\n\t             b)\n\tif result:\n\t\t\
                    print(result)\n\treturn result\n";
        let old = "    result = foo(a,\n                 b)\n    if result:";
        let r = refused(file, old, &format!("{old}\n        log(result)"));
        assert_eq!(r.reason, "not_reindentable");
        assert!(
            r.message.contains(
                "the file indents these lines with tabs and new_text line 4 is indented \
                 further with spaces"
            ),
            "{}",
            r.message
        );
        let file = "f() {\n\t/*\n\t * x\n\t */\n\tcode();\n}\n";
        let old = "    /*\n     * x\n     */\n    code();";
        let r = refused(file, old, &format!("{old}\n        more();"));
        assert_eq!(r.reason, "not_reindentable");
    }

    #[test]
    fn unindented_lines_use_the_files_indentation() {
        // old_text is indented, the lines it matches aren't: the rest of the file says
        // what nested lines are indented with.
        let file = "x = 1\ny = 2\nblock:\n\tz = 3\n";
        let old = "  x = 1\n  y = 2";
        let r = refused(file, old, "  x = 1\n  if y:\n      y = 2");
        assert_eq!(r.reason, "not_reindentable");
        assert!(
            r.message
                .contains("the file indents with tabs and new_text line 3"),
            "{}",
            r.message
        );
        assert_eq!(
            applied(file, old, "  x = 1\n  if y:\n  \ty = 2"),
            "x = 1\nif y:\n\ty = 2\nblock:\n\tz = 3\n"
        );
        // A file indented nowhere doesn't limit it.
        assert_eq!(
            applied("x = 1\ny = 2\n", old, "  x = 1\n  if y:\n      y = 2"),
            "x = 1\nif y:\n    y = 2\n"
        );
    }

    #[test]
    fn trailing_whitespace_only_changes_are_identical() {
        let file = "list:\n        a = 1\n        b = 2\n";
        let r = refused(file, "    a = 1\n    b = 2", "    a = 1  \n    b = 2\n");
        assert_eq!(r.reason, "identical");
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
        // Adding a blank line doesn't make it more than an indentation change.
        let file = "f:\n      a()\n      b()\n";
        let r = refused(file, "a()\nb()", "    a()\n\n    b()");
        assert_eq!(r.reason, "indentation_only");
        // Adding only a blank line is a change, applied.
        assert_eq!(
            applied(file, "a()\nb()", "a()\n\nb()"),
            "f:\n      a()\n\n      b()\n"
        );
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
