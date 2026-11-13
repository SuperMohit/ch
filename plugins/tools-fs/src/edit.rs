//! `edit_file`.

use std::collections::HashMap;
use std::time::Duration;

use ch_kernel::reexports::async_trait;
use ch_kernel::util::truncate_middle;
use ch_kernel::{Cx, Tool, ToolOutput, ToolSpec};
use serde::Deserialize;
use serde_json::{Value, json};
use similar::{ChangeTag, TextDiff};

use crate::common::{FsContext, diff_for_model, input_path, not_found, parse_input, unified_diff};

const USAGE: &str =
    "{\"path\": string, \"old_text\": string, \"new_text\": string, \"replace_all\"?: boolean}";

/// How to recover from an old_text that wasn't found.
const ADVICE: &str = "Re-read the file with read_file and copy the text exactly; whitespace and \
                      indentation must match, and line-number prefixes are not part of the file.";

/// Diff lines shown for the closest lines to an old_text that wasn't found.
const MAX_HINT_LINES: usize = 40;

/// Exact-match string replacement in a file.
pub struct EditFile {
    ctx: FsContext,
}

impl EditFile {
    pub fn new(ctx: FsContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct Input {
    path: String,
    old_text: String,
    new_text: String,
    #[serde(default)]
    replace_all: bool,
}

#[async_trait]
impl Tool for EditFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "edit_file".into(),
            description: "Replace exact text in an existing file and get back a unified diff.\n\
                 When: changing part of a file. Prefer it over write_file for existing files; \
                 make several small edits rather than one huge one.\n\
                 Protocol: read the whole file with read_file first. `old_text` must match \
                 the file exactly, including whitespace and indentation, without read_file's \
                 line-number prefixes. It must be unique: include a few surrounding lines, or \
                 set `replace_all` to change every occurrence (e.g. renames). `new_text` \
                 replaces it.\n\
                 Error modes: file not read (or only partly read) yet; file changed on disk \
                 since your last read (read it again); old_text not found (re-read and copy \
                 exactly; a unique match that differs only in indentation is applied, \
                 re-indented); old_text found several times (add context or use replace_all); \
                 old_text equal to new_text; missing file (use write_file to create).\n\
                 Side effects: writes the file."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path, relative to the workspace root."},
                    "old_text": {"type": "string", "description": "Exact text to replace (must be unique unless replace_all)."},
                    "new_text": {"type": "string", "description": "Replacement text."},
                    "replace_all": {"type": "boolean", "default": false, "description": "Replace every occurrence of old_text."}
                },
                "required": ["path", "old_text", "new_text"]
            }),
            read_only: false,
        }
    }

    async fn call(&self, input: Value, _cx: &Cx) -> ToolOutput {
        let input: Input = match parse_input("edit_file", input, USAGE) {
            Ok(i) => i,
            Err(e) => return failed(e, "bad_input", None),
        };
        self.edit(input)
    }

    fn summarize(&self, input: &Value) -> String {
        let count = |k: &str| {
            input
                .get(k)
                .and_then(Value::as_str)
                .map_or(0, |s| s.lines().count())
        };
        format!(
            "{} (+{} -{})",
            input_path(input),
            count("new_text"),
            count("old_text")
        )
    }
}

impl EditFile {
    fn edit(&self, input: Input) -> ToolOutput {
        let ctx = &self.ctx;
        if input.old_text.is_empty() {
            return failed(
                ToolOutput::error(
                    "old_text is empty. To create a file use write_file; to insert text, put a \
                     nearby line in old_text and repeat it in new_text with your addition.",
                ),
                "empty_old_text",
                None,
            );
        }
        if input.old_text == input.new_text {
            return failed(
                ToolOutput::error(
                    "old_text and new_text are identical, so nothing would change. Put the \
                     changed text in new_text.",
                ),
                "identical",
                None,
            );
        }
        let path = match ctx.guard.resolve(&input.path) {
            Ok(p) => p,
            Err(e) => return failed(ToolOutput::error(e), "bad_path", None),
        };
        let rel = ctx.rel(&path);
        let fail = |reason: &str, out: ToolOutput| failed(out, reason, Some(&rel));
        match std::fs::metadata(&path) {
            Ok(m) if m.is_dir() => {
                return fail(
                    "is_dir",
                    ToolOutput::error(format!(
                        "`{rel}` is a directory; edit_file edits files. Use list_files to see \
                         what it contains."
                    )),
                );
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return fail(
                    "missing",
                    not_found(ctx, &path, "Use write_file to create a new file."),
                );
            }
            Err(e) => {
                return fail(
                    "read_failed",
                    ToolOutput::error(format!("Could not open `{rel}`: {e}.")),
                );
            }
        }
        let bytes = match ctx.state.check_writable(&path) {
            Ok(Some(b)) => b,
            Ok(None) => {
                return fail(
                    "missing",
                    not_found(ctx, &path, "Use write_file to create a new file."),
                );
            }
            Err(msg) => return fail("unread_or_stale", ToolOutput::error(msg)),
        };
        if ch_kernel::util::looks_binary(&bytes) {
            return fail(
                "binary",
                ToolOutput::error(format!(
                    "`{rel}` looks like a binary file; edit_file only edits text."
                )),
            );
        }
        let Ok(old) = String::from_utf8(bytes) else {
            return fail(
                "not_utf8",
                ToolOutput::error(format!(
                    "`{rel}` is not valid UTF-8; edit_file only edits UTF-8 text. Use bash if \
                     you must change it."
                )),
            );
        };

        // Files with CRLF line endings: match and write with CRLF too.
        let (old_text, new_text) = if !old.contains(&input.old_text)
            && old.contains("\r\n")
            && !input.old_text.contains('\r')
        {
            (
                input.old_text.replace('\n', "\r\n"),
                input.new_text.replace('\n', "\r\n"),
            )
        } else {
            (input.old_text.clone(), input.new_text.clone())
        };

        let positions: Vec<usize> = old.match_indices(&old_text).map(|(i, _)| i).collect();
        let (new, note) = if positions.is_empty() {
            match whitespace_edit(
                &rel,
                &old,
                &input.old_text,
                &input.new_text,
                input.replace_all,
            ) {
                Ok((new, note)) => (new, Some(note)),
                Err(msg) => return fail("not_found", ToolOutput::error(msg)),
            }
        } else if positions.len() > 1 && !input.replace_all {
            let lines: Vec<String> = positions
                .iter()
                .take(10)
                .map(|&i| (old[..i].matches('\n').count() + 1).to_string())
                .collect();
            return fail(
                "not_unique",
                ToolOutput::error(format!(
                    "old_text appears {} times in `{rel}` (starting at lines {}). Include more \
                     surrounding lines to make it unique, or set replace_all=true to replace \
                     every occurrence.",
                    positions.len(),
                    lines.join(", ")
                )),
            );
        } else if input.replace_all {
            (old.replace(&old_text, &new_text), None)
        } else {
            (old.replacen(&old_text, &new_text, 1), None)
        };
        if new == old {
            return fail(
                "identical",
                ToolOutput::error(
                    "Once whitespace is adjusted, new_text is what the file already has, so \
                     nothing would change. Put the changed text in new_text.",
                ),
            );
        }
        if let Err(e) = std::fs::write(&path, &new) {
            return fail(
                "write_failed",
                ToolOutput::error(format!("Could not write `{rel}`: {e}.")),
            );
        }
        ctx.state.record_write(&path, new.as_bytes());

        let (diff, added, removed) = unified_diff(&rel, &old, &new);
        let what = if positions.len() > 1 {
            format!(", {} occurrences", positions.len())
        } else {
            String::new()
        };
        let note_text = note.as_deref().map(|n| format!(" {n}")).unwrap_or_default();
        let content = format!(
            "Edited `{rel}` (+{added} -{removed}{what}).{note_text}\n{}",
            diff_for_model(&diff)
        );
        let mut metadata = json!({ "path": rel, "diff": diff });
        if note.is_some() {
            metadata["whitespace_adjusted"] = json!(true);
        }
        ToolOutput::ok(content).with_metadata(metadata)
    }
}

/// `out`, an error, with its failure reason (and the path, once known) in its metadata.
fn failed(out: ToolOutput, reason: &str, rel: Option<&str>) -> ToolOutput {
    let mut metadata = json!({ "failure": reason });
    if let Some(rel) = rel {
        metadata["path"] = json!(rel);
    }
    out.with_metadata(metadata)
}

/// For an old_text with no exact match: when it matches exactly one place under the
/// whitespace rule of [`Pattern`] (never with `replace_all`), the edited file and a note
/// saying what was adjusted; otherwise the error to return.
fn whitespace_edit(
    rel: &str,
    file: &str,
    old_text: &str,
    new_text: &str,
    replace_all: bool,
) -> Result<(String, String), String> {
    let lines: Vec<&str> = file.split_inclusive('\n').collect();
    let bodies: Vec<&str> = lines.iter().map(|l| l.trim_end()).collect();
    if !replace_all && let Some(pattern) = Pattern::new(old_text) {
        let starts = pattern.matches(&bodies);
        if let [start] = starts[..] {
            return pattern.apply(rel, file, &lines, start, new_text);
        }
        if starts.len() > 1 {
            let n = pattern.lines.len();
            let spans: Vec<String> = starts.iter().take(10).map(|&s| span(s, n)).collect();
            return Err(format!(
                "old_text was not found in `{rel}` exactly. With whitespace adjusted it \
                 matches {} places (lines {}), so nothing was changed. Include more \
                 surrounding lines to make it unique, copied exactly as read_file shows them.",
                starts.len(),
                spans.join(", ")
            ));
        }
    }
    Err(not_found_hint(rel, &bodies, old_text, replace_all))
}

/// old_text as whole lines, for the whitespace fallback.
///
/// A window of as many file lines matches when, with trailing whitespace ignored, blank
/// lines pair with blank lines and every other line differs only by one swap of
/// indentation: `base`, the indentation of old_text's least-indented line, stands for
/// the indentation of the file line in its place, and what follows `base` on each line
/// must be in the file after that indentation.
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

    /// Starts of the matching windows in `bodies` (file lines without trailing
    /// whitespace).
    fn matches(&self, bodies: &[&str]) -> Vec<usize> {
        let n = self.lines.len();
        if bodies.len() < n {
            return Vec::new();
        }
        (0..=bodies.len() - n)
            .filter(|&s| self.indent_at(bodies, s).is_some())
            .collect()
    }

    /// The file's indentation in place of `base`, if the window at `start` matches.
    fn indent_at<'f>(&self, bodies: &[&'f str], start: usize) -> Option<&'f str> {
        let target = indent(bodies[start + self.anchor]);
        let base = self.base.len();
        self.lines
            .iter()
            .zip(&bodies[start..])
            .all(|(p, f)| {
                if p.is_empty() {
                    f.is_empty()
                } else {
                    f.strip_prefix(target) == Some(&p[base..])
                }
            })
            .then_some(target)
    }

    /// Replaces the window at `start` of `lines` (the file's lines with their endings)
    /// with `new_text`, re-indented the same way, keeping the file's line endings.
    fn apply(
        &self,
        rel: &str,
        file: &str,
        lines: &[&str],
        start: usize,
        new_text: &str,
    ) -> Result<(String, String), String> {
        let n = self.lines.len();
        let window = &lines[start..start + n];
        let target = indent(window[self.anchor].trim_end());
        let swap = self.base != target;
        let new_lines = if new_text.is_empty() {
            Vec::new()
        } else {
            as_lines(new_text)
        };
        let mut out = Vec::with_capacity(new_lines.len());
        for (i, line) in new_lines.iter().enumerate() {
            if !swap || line.trim().is_empty() {
                out.push(line.to_string());
            } else if let Some(rest) = line.strip_prefix(self.base) {
                out.push(format!("{target}{rest}"));
            } else {
                return Err(format!(
                    "old_text was not found in `{rel}` exactly. It matches {} with the \
                     indentation changed (`{}` → `{}`), but new_text line {} doesn't start \
                     with old_text's indentation `{}`, so it can't be re-indented the same \
                     way. {ADVICE}",
                    lines_label(start, n),
                    describe_indent(self.base),
                    describe_indent(target),
                    i + 1,
                    describe_indent(self.base)
                ));
            }
        }

        let eol = if window[0].ends_with("\r\n")
            || (!window[0].ends_with('\n') && file.contains("\r\n"))
        {
            "\r\n"
        } else {
            "\n"
        };
        let mut text = out.join(eol);
        if !out.is_empty() && window[n - 1].ends_with('\n') {
            text.push_str(eol);
        }
        let from: usize = lines[..start].iter().map(|l| l.len()).sum();
        let to = from + window.iter().map(|l| l.len()).sum::<usize>();
        let edited = format!("{}{text}{}", &file[..from], &file[to..]);

        let trailing = self.raw.iter().zip(window).any(|(p, f)| {
            let f = body(f);
            p[p.trim_end().len()..] != f[f.trim_end().len()..]
        });
        let mut what = Vec::new();
        if swap {
            what.push(format!(
                "indentation `{}` → `{}`",
                describe_indent(self.base),
                describe_indent(target)
            ));
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
            "old_text matched {} only after adjusting whitespace ({}){reindented}. Check the \
             diff.",
            lines_label(start, n),
            what.join(", ")
        );
        Ok((edited, note))
    }
}

/// Explains why `needle` wasn't found in the file whose lines (without trailing
/// whitespace) are `bodies`: where it matches if whitespace is ignored and what differs
/// there, or the most similar lines with a diff.
fn not_found_hint(rel: &str, bodies: &[&str], needle: &str, replace_all: bool) -> String {
    let base = format!("old_text was not found in `{rel}`.");
    let needle_lines = as_lines(needle);
    if let Some((first, last)) = loose_match(bodies, needle) {
        let differs = whitespace_differences(&needle_lines, &bodies[first..=last]);
        let exact = if replace_all {
            " replace_all only replaces exact matches."
        } else {
            ""
        };
        return format!(
            "{base} It matches {} if whitespace is ignored; what differs: {}.{exact} {ADVICE}",
            lines_label(first, last - first + 1),
            differs.join(", ")
        );
    }
    match closest_lines(bodies, &needle_lines) {
        Some(close) => format!("{base} {close}\n{ADVICE}"),
        None => format!("{base} {ADVICE}"),
    }
}

/// The first and last line (0-based) where `needle` occurs in `bodies` when every run of
/// whitespace, line breaks included, counts as one space.
fn loose_match(bodies: &[&str], needle: &str) -> Option<(usize, usize)> {
    let needle = needle.split_whitespace().collect::<Vec<_>>().join(" ");
    if needle.is_empty() {
        return None;
    }
    let mut hay = String::new();
    // Where each non-blank line's first word starts in `hay`.
    let mut starts: Vec<(usize, usize)> = Vec::new();
    for (i, line) in bodies.iter().enumerate() {
        for (k, word) in line.split_whitespace().enumerate() {
            if !hay.is_empty() {
                hay.push(' ');
            }
            if k == 0 {
                starts.push((hay.len(), i));
            }
            hay.push_str(word);
        }
    }
    let at = hay.find(&needle)?;
    let line_of = |offset: usize| starts[starts.partition_point(|&(o, _)| o <= offset) - 1].1;
    Some((line_of(at), line_of(at + needle.len() - 1)))
}

/// What differs between old_text's lines and the file lines they match when whitespace
/// is ignored: `indentation`, `line breaks` and/or `spaces inside lines`.
fn whitespace_differences(needle: &[&str], span: &[&str]) -> Vec<&'static str> {
    let squash = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    // Without blank lines at either end.
    let content = |lines: &[&str]| -> Vec<String> {
        let blank = |l: &&str| l.trim().is_empty();
        let first = lines.iter().position(|l| !blank(l)).unwrap_or(lines.len());
        let end = lines
            .iter()
            .rposition(|l| !blank(l))
            .map_or(first, |e| e + 1);
        lines[first..end]
            .iter()
            .map(|l| l.trim_end().to_string())
            .collect()
    };
    let (needle, span) = (content(needle), content(span));
    if needle.len() != span.len() {
        return vec!["line breaks"];
    }
    let (mut indentation, mut inner) = (false, false);
    let last = needle.len().saturating_sub(1);
    for (i, (p, f)) in needle.iter().zip(&span).enumerate() {
        let (sp, sf) = (squash(p), squash(f));
        if sp.is_empty() || sf.is_empty() {
            if sp.is_empty() != sf.is_empty() {
                return vec!["line breaks"];
            }
        } else if sp == sf {
            // The first line may start partway into the file's indentation.
            indentation |= if i == 0 {
                !indent(f).ends_with(indent(p))
            } else {
                indent(p) != indent(f)
            };
            inner |= p.trim_start() != f.trim_start();
        } else if i == 0 && i == last && sf.contains(&sp) {
            // old_text is part of one line.
            inner |= !f.contains(p.trim());
        } else if i == 0 && sf.ends_with(&sp) {
            // old_text starts partway through the line.
            inner |= !f.ends_with(p.trim());
        } else if i == last && sf.starts_with(&sp) {
            // old_text ends partway through the line.
            indentation |= indent(p) != indent(f);
            inner |= !f.trim_start().starts_with(p.trim_start());
        } else {
            return vec!["line breaks"];
        }
    }
    let mut out = Vec::new();
    if indentation {
        out.push("indentation");
    }
    if inner {
        out.push("spaces inside lines");
    }
    if out.is_empty() {
        out.push("trailing whitespace or line endings");
    }
    out
}

/// The `needle.len()` file lines most like `needle`, compared without surrounding
/// whitespace, when at least half of its non-blank lines appear there: where they are, a
/// short diff, and other places that are as close.
fn closest_lines(bodies: &[&str], needle: &[&str]) -> Option<String> {
    let w = needle.len().min(bodies.len());
    // An id for each distinct non-blank line of `needle`, and how often each occurs.
    let mut ids: HashMap<&str, usize> = HashMap::new();
    let mut need: Vec<usize> = Vec::new();
    for line in needle.iter().map(|l| l.trim()).filter(|l| !l.is_empty()) {
        let id = *ids.entry(line).or_insert_with(|| {
            need.push(0);
            need.len() - 1
        });
        need[id] += 1;
    }
    let total: usize = need.iter().sum();
    if w == 0 || total == 0 {
        return None;
    }
    // Each window's score is how many of those lines it has (each counted at most as
    // often as in `needle`), kept up to date as the window slides one line at a time.
    let line_ids: Vec<Option<usize>> = bodies.iter().map(|l| ids.get(l.trim()).copied()).collect();
    let mut have = vec![0; need.len()];
    let mut score = 0;
    let mut scores = Vec::with_capacity(bodies.len() + 1 - w);
    for end in 0..bodies.len() {
        if end >= w
            && let Some(id) = line_ids[end - w]
        {
            if have[id] <= need[id] {
                score -= 1;
            }
            have[id] -= 1;
        }
        if let Some(id) = line_ids[end] {
            have[id] += 1;
            if have[id] <= need[id] {
                score += 1;
            }
        }
        if end + 1 >= w {
            scores.push(score);
        }
    }
    let best = scores.iter().copied().max()?;
    if best == 0 || best * 2 < total {
        return None;
    }

    // Overlapping windows with the best score are one place. In each, take the window
    // whose lines line up with `needle` best (the earliest on a tie); only the first 64
    // are compared, so a long run of ties stays cheap.
    let mut places: Vec<Vec<usize>> = Vec::new();
    for (start, _) in scores.iter().enumerate().filter(|&(_, &s)| s == best) {
        match places.last_mut() {
            Some(place) if start < place[place.len() - 1] + w => place.push(start),
            _ => places.push(vec![start]),
        }
    }
    let aligned = |place: &[usize]| {
        let mut pick = (0, place[0]);
        for &start in place.iter().take(64) {
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
    // Whether an unchanged line differs in whitespace (shown as in the file).
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
    if shown.len() > MAX_HINT_LINES {
        let more = shown.len() - MAX_HINT_LINES;
        shown.truncate(MAX_HINT_LINES);
        shown.push(format!("… {more} more lines"));
    }
    let ignored = if moved {
        " (indentation ignored; unchanged lines as in the file)"
    } else {
        ""
    };
    let mut msg = format!(
        "The closest part of the file is {} ({best} of old_text's {total} non-blank lines \
         appear there). Diff from old_text (-) to the file (+){ignored}:\n{}",
        lines_label(start, w),
        shown.join("\n")
    );
    let others: Vec<String> = places[1..]
        .iter()
        .take(5)
        .map(|p| span(aligned(p), w))
        .collect();
    if !others.is_empty() {
        let more = places.len() - 1 - others.len();
        let more = if more > 0 {
            format!(" and {more} more")
        } else {
            String::new()
        };
        msg.push_str(&format!("\nAs close: lines {}{more}.", others.join(", ")));
    }
    Some(msg)
}

/// `text` as whole lines without line endings: one trailing line break is dropped.
fn as_lines(text: &str) -> Vec<&str> {
    let text = text.strip_suffix('\n').unwrap_or(text);
    text.split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect()
}

/// `line` without its line ending.
fn body(line: &str) -> &str {
    let line = line.strip_suffix('\n').unwrap_or(line);
    line.strip_suffix('\r').unwrap_or(line)
}

/// The leading whitespace of `line`.
fn indent(line: &str) -> &str {
    &line[..line.len() - line.trim_start().len()]
}

/// Indentation in words: `4 spaces`, `1 tab`, `1 tab + 2 spaces`, `none`.
fn describe_indent(ws: &str) -> String {
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

/// `3` or `3–5`: `n` lines from the 0-based `start`.
fn span(start: usize, n: usize) -> String {
    if n <= 1 {
        (start + 1).to_string()
    } else {
        format!("{}–{}", start + 1, start + n)
    }
}

/// `line 3` or `lines 3–5`.
fn lines_label(start: usize, n: usize) -> String {
    let s = if n <= 1 { "" } else { "s" };
    format!("line{s} {}", span(start, n))
}

/// One line of a hint, shortened in the middle when long.
fn short(line: &str) -> String {
    truncate_middle(line, 160).replace('\n', " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hint(file: &str, needle: &str) -> String {
        let bodies: Vec<&str> = file.split_inclusive('\n').map(str::trim_end).collect();
        not_found_hint("a.rs", &bodies, needle, false)
    }

    #[test]
    fn hints() {
        let hay = "fn main() {\n    let x = 1;\n    let y = 2;\n}\n";
        let h = hint(hay, "let x = 1;\nlet y = 2;");
        assert!(
            h.contains("lines 2–3 if whitespace is ignored; what differs: indentation."),
            "{h}"
        );
        let h = hint(hay, "    let x = 1;\n    let y = 3;");
        assert!(h.contains("closest part of the file is lines 2–3"), "{h}");
        assert!(h.contains("-    let y = 3;\n+    let y = 2;"), "{h}");
        let h = hint(hay, "nothing like it");
        assert_eq!(h, format!("old_text was not found in `a.rs`. {ADVICE}"));
    }

    #[test]
    fn whitespace_differences_are_named() {
        let hay = "fn f() {\n    let x  = 1;\n    g(a,\n      b);\n}\n";
        let h = hint(hay, "let x = 1;");
        assert!(h.contains("line 2 if"), "{h}");
        assert!(h.contains("what differs: spaces inside lines."), "{h}");
        let h = hint(hay, "g(a, b);");
        assert!(h.contains("lines 3–4 if"), "{h}");
        assert!(h.contains("what differs: line breaks."), "{h}");
        let h = hint(hay, "  let x  = 1;\n  g(a,\n    b);");
        assert!(h.contains("what differs: indentation."), "{h}");
        // Starts partway through a line.
        let h = hint(hay, "x  = 1;\n  g(a,");
        assert!(h.contains("lines 2–3 if"), "{h}");
        assert!(h.contains("what differs: indentation."), "{h}");
    }

    #[test]
    fn closest_lines_caps_the_diff_and_long_lines() {
        let file: String = (0..200).map(|i| format!("line {i}\n")).collect();
        let bodies: Vec<&str> = file.lines().collect();
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
        let h = closest_lines(&bodies, &needle).unwrap();
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
        let bodies = ["a", "b", "c", "d"];
        assert!(closest_lines(&bodies, &["a", "x", "y"]).is_none());
        assert!(closest_lines(&bodies, &["a", "b", "y"]).is_some());
        assert!(closest_lines(&bodies, &["", " "]).is_none());
    }

    #[test]
    fn closest_lines_picks_the_aligned_window_and_lists_ties() {
        // Windows shifted by one line score the same; the aligned one is shown.
        let bodies = ["z", "a", "b", "c", "d", "q", "q", "q", "a", "b", "c", "e"];
        let h = closest_lines(&bodies, &["a", "b", "c", "x"]).unwrap();
        assert!(h.contains("lines 2–5"), "{h}");
        assert!(h.contains(":\n b\n c\n-x\n+d\n"), "{h}");
        assert!(h.contains("As close: lines 9–12."), "{h}");
    }

    #[test]
    fn pattern_rule() {
        let p = Pattern::new("  a\n    b\n\n  c\n").unwrap();
        assert_eq!((p.anchor, p.base, p.lines.len()), (0, "  ", 4));
        // Base 2 spaces -> 1 tab, nested remainder kept.
        assert_eq!(p.indent_at(&["\ta", "\t  b", "", "\tc"], 0), Some("\t"));
        // Blank lines pair with blank lines only.
        assert_eq!(p.indent_at(&["\ta", "\t  b", "x", "\tc"], 0), None);
        // The nested remainder must be the same.
        assert_eq!(p.indent_at(&["\ta", "\t\tb", "", "\tc"], 0), None);
        // Text after the indentation must be equal.
        assert_eq!(p.indent_at(&["\ta", "\t  B", "", "\tc"], 0), None);
        // No single prefix: a tab line and a space line.
        assert!(Pattern::new("\ta\n    b").is_none());
        assert!(Pattern::new("\n  \n").is_none());
    }

    #[test]
    fn indentation_in_words() {
        assert_eq!(describe_indent(""), "none");
        assert_eq!(describe_indent("    "), "4 spaces");
        assert_eq!(describe_indent("\t"), "1 tab");
        assert_eq!(describe_indent("\t  "), "1 tab + 2 spaces");
        assert_eq!(describe_indent("\u{a0}"), "1 × U+00A0");
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
