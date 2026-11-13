//! `edit_file`.

mod hints;
mod lines;
mod whitespace;

use std::path::Path;

use ch_kernel::reexports::{CancellationToken, async_trait};
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
                 exactly; if it matches one place ignoring whitespace and differs there only \
                 in indentation and trailing whitespace, it is applied re-indented); old_text \
                 found several times (add context or use replace_all); \
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

    async fn call(&self, input: Value, cx: &Cx) -> ToolOutput {
        let input: Input = match parse_input("edit_file", input, USAGE) {
            Ok(i) => i,
            Err(e) => return failed(e, "bad_input", None),
        };
        self.edit(input, &cx.cancel)
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
    fn edit(&self, input: Input, cancel: &CancellationToken) -> ToolOutput {
        self.edit_with(input, cancel, || {})
    }

    /// [`EditFile::edit`], running `before_write` just before the file is re-read and
    /// written (tests change the file there).
    fn edit_with(
        &self,
        input: Input,
        cancel: &CancellationToken,
        before_write: impl FnOnce(),
    ) -> ToolOutput {
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
            match whitespace::edit(
                &rel,
                &old,
                &input.old_text,
                &input.new_text,
                input.replace_all,
                cancel,
            ) {
                Ok((new, note)) => (new, Some(note)),
                Err(refusal) => return fail(refusal.reason, ToolOutput::error(refusal.message)),
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
        if cancel.is_cancelled() {
            return fail("cancelled", ToolOutput::error("Cancelled."));
        }
        before_write();
        match write_checked(&path, old.as_bytes(), &new) {
            Ok(()) => {}
            Err(Unwritten::Changed) => {
                return fail(
                    "changed",
                    ToolOutput::error(format!(
                        "`{rel}` changed on disk while the edit was being made (maybe by bash \
                         or the user), so nothing was written. Read it again before editing."
                    )),
                );
            }
            Err(Unwritten::Failed(e)) => {
                return fail(
                    "write_failed",
                    ToolOutput::error(format!("Could not write `{rel}`: {e}.")),
                );
            }
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

/// Why [`write_checked`] wrote nothing.
enum Unwritten {
    /// The file no longer holds the checked bytes.
    Changed,
    Failed(std::io::Error),
}

/// Writes `new` to `path` if the file still holds `checked`, the bytes the
/// read-before-write check verified and the edit was made from.
fn write_checked(path: &Path, checked: &[u8], new: &str) -> Result<(), Unwritten> {
    match std::fs::read(path) {
        Ok(now) if now == checked => {}
        Ok(_) => return Err(Unwritten::Changed),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(Unwritten::Changed),
        Err(e) => return Err(Unwritten::Failed(e)),
    }
    std::fs::write(path, new).map_err(Unwritten::Failed)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ch_safety::{FileState, PathGuard};

    use super::*;

    #[test]
    fn writes_only_what_was_checked() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "now\n").unwrap();
        // Changed since the check: nothing is written.
        assert!(matches!(
            write_checked(&path, b"then\n", "new\n"),
            Err(Unwritten::Changed)
        ));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "now\n");
        assert!(write_checked(&path, b"now\n", "new\n").is_ok());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new\n");
        // Deleted since the check.
        std::fs::remove_file(&path).unwrap();
        assert!(matches!(
            write_checked(&path, b"new\n", "x"),
            Err(Unwritten::Changed)
        ));
        assert!(!path.exists());
    }

    #[test]
    fn edits_write_only_what_was_checked() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join("a.txt");
        std::fs::write(&path, "x = 1\n").unwrap();
        let state = Arc::new(FileState::with_root(&root));
        state.record_read(&path, b"x = 1\n", true);
        let ctx = FsContext::new(Arc::new(PathGuard::new(&root).unwrap()), state);
        let input = || Input {
            path: "a.txt".into(),
            old_text: "x = 1".into(),
            new_text: "x = 2".into(),
            replace_all: false,
        };
        // Changed between the check and the write: kept as it is now.
        let out = EditFile::new(ctx).edit_with(input(), &CancellationToken::new(), || {
            std::fs::write(&path, "x = 1 # changed\n").unwrap();
        });
        assert_eq!(out.metadata["failure"], "changed", "{}", out.content);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "x = 1 # changed\n");
    }
}
