//! `edit_file`.

use ch_kernel::reexports::async_trait;
use ch_kernel::{Cx, Tool, ToolOutput, ToolSpec};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::common::{FsContext, diff_for_model, input_path, not_found, parse_input, unified_diff};

const USAGE: &str =
    "{\"path\": string, \"old_text\": string, \"new_text\": string, \"replace_all\"?: boolean}";

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
                 exactly); old_text found several times (add context or use replace_all); \
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
            Err(e) => return e,
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
            return ToolOutput::error(
                "old_text is empty. To create a file use write_file; to insert text, put a \
                 nearby line in old_text and repeat it in new_text with your addition.",
            );
        }
        if input.old_text == input.new_text {
            return ToolOutput::error(
                "old_text and new_text are identical, so nothing would change. Put the \
                 changed text in new_text.",
            );
        }
        let path = match ctx.guard.resolve(&input.path) {
            Ok(p) => p,
            Err(e) => return ToolOutput::error(e),
        };
        let rel = ctx.rel(&path);
        match std::fs::metadata(&path) {
            Ok(m) if m.is_dir() => {
                return ToolOutput::error(format!(
                    "`{rel}` is a directory; edit_file edits files. Use list_files to see \
                     what it contains."
                ));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return not_found(ctx, &path, "Use write_file to create a new file.");
            }
            Err(e) => return ToolOutput::error(format!("Could not open `{rel}`: {e}.")),
        }
        let bytes = match ctx.state.check_writable(&path) {
            Ok(Some(b)) => b,
            Ok(None) => return not_found(ctx, &path, "Use write_file to create a new file."),
            Err(msg) => return ToolOutput::error(msg),
        };
        if ch_kernel::util::looks_binary(&bytes) {
            return ToolOutput::error(format!(
                "`{rel}` looks like a binary file; edit_file only edits text."
            ));
        }
        let Ok(old) = String::from_utf8(bytes) else {
            return ToolOutput::error(format!(
                "`{rel}` is not valid UTF-8; edit_file only edits UTF-8 text. Use bash if you \
                 must change it."
            ));
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
        if positions.is_empty() {
            return ToolOutput::error(not_found_hint(&rel, &old, &input.old_text));
        }
        if positions.len() > 1 && !input.replace_all {
            let lines: Vec<String> = positions
                .iter()
                .take(10)
                .map(|&i| (old[..i].matches('\n').count() + 1).to_string())
                .collect();
            return ToolOutput::error(format!(
                "old_text appears {} times in `{rel}` (starting at lines {}). Include more \
                 surrounding lines to make it unique, or set replace_all=true to replace every \
                 occurrence.",
                positions.len(),
                lines.join(", ")
            ));
        }
        let new = if input.replace_all {
            old.replace(&old_text, &new_text)
        } else {
            old.replacen(&old_text, &new_text, 1)
        };
        if let Err(e) = std::fs::write(&path, &new) {
            return ToolOutput::error(format!("Could not write `{rel}`: {e}."));
        }
        ctx.state.record_write(&path, new.as_bytes());

        let (diff, added, removed) = unified_diff(&rel, &old, &new);
        let what = if positions.len() == 1 {
            String::new()
        } else {
            format!(", {} occurrences", positions.len())
        };
        let content = format!(
            "Edited `{rel}` (+{added} -{removed}{what}).\n{}",
            diff_for_model(&diff)
        );
        ToolOutput::ok(content).with_metadata(json!({ "path": rel, "diff": diff }))
    }
}

/// Explains why `needle` wasn't found and points at the closest line, if any.
fn not_found_hint(rel: &str, haystack: &str, needle: &str) -> String {
    let base = format!("old_text was not found in `{rel}`.");
    let advice = "Re-read the file with read_file and copy the text exactly; whitespace and \
                  indentation must match, and line-number prefixes are not part of the file.";
    let squash = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let squashed_needle = squash(needle);
    if !squashed_needle.is_empty() && squash(haystack).contains(&squashed_needle) {
        return format!(
            "{base} It does match if whitespace is ignored, so the indentation, line breaks \
             or spaces differ. {advice}"
        );
    }
    let first = needle.lines().map(str::trim).find(|l| !l.is_empty());
    if let Some(first) = first {
        let hit = haystack
            .lines()
            .enumerate()
            .find(|(_, l)| l.trim() == first)
            .or_else(|| {
                haystack
                    .lines()
                    .enumerate()
                    .find(|(_, l)| first.len() >= 8 && l.contains(first))
            });
        if let Some((n, line)) = hit {
            return format!(
                "{base} Its first line matches line {} (`{}`), so a later line differs. \
                 {advice}",
                n + 1,
                ch_kernel::util::truncate_middle(line.trim(), 120)
            );
        }
    }
    format!("{base} {advice}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hints() {
        let hay = "fn main() {\n    let x = 1;\n    let y = 2;\n}\n";
        let h = not_found_hint("a.rs", hay, "let x = 1;\nlet y = 2;");
        assert!(h.contains("whitespace is ignored"), "{h}");
        let h = not_found_hint("a.rs", hay, "    let x = 1;\n    let y = 3;");
        assert!(h.contains("line 2"), "{h}");
        let h = not_found_hint("a.rs", hay, "nothing like it");
        assert!(h.contains("Re-read the file"), "{h}");
    }
}
