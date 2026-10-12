//! `read_file`.

use ch_kernel::reexports::async_trait;
use ch_kernel::{Cx, Tool, ToolOutput, ToolSpec};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::common::{FsContext, input_path, not_found, parse_input};

/// Lines returned when no `limit` is given.
pub const DEFAULT_LIMIT: usize = 2000;
/// Longer lines are cut.
pub const MAX_LINE_CHARS: usize = 2000;
/// Files larger than this are refused.
pub const MAX_FILE_BYTES: u64 = 20 * 1024 * 1024;

const USAGE: &str = "{\"path\": string, \"offset\"?: integer >= 1, \"limit\"?: integer >= 1}";

/// Reads a text file with line numbers.
pub struct ReadFile {
    ctx: FsContext,
}

impl ReadFile {
    pub fn new(ctx: FsContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct Input {
    path: String,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

#[async_trait]
impl Tool for ReadFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read_file".into(),
            description: format!(
                "Read a text file from the workspace. Output is numbered like `cat -n` \
                 (line number, tab, line).\n\
                 When: before editing a file (edit_file and write_file require a full read \
                 of the current content), and whenever you need to see code or text.\n\
                 Protocol: `path` is relative to the workspace root (absolute paths inside \
                 the workspace also work). Without offset/limit it returns up to \
                 {DEFAULT_LIMIT} lines; for longer files continue with `offset` (1-based \
                 line to start at) as the footer says. Lines over {MAX_LINE_CHARS} chars are \
                 cut. The line-number prefix is not part of the file: never copy it into \
                 edit_file's old_text.\n\
                 Error modes: missing file (check with glob_files), a directory (use \
                 list_files), binary files, paths outside the workspace, offset past the end.\n\
                 Side effects: none (records what you have read, for edit checks)."
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path, relative to the workspace root."},
                    "offset": {"type": "integer", "minimum": 1, "description": "1-based line number to start reading at."},
                    "limit": {"type": "integer", "minimum": 1, "description": "Maximum number of lines to return."}
                },
                "required": ["path"]
            }),
            read_only: true,
        }
    }

    async fn call(&self, input: Value, _cx: &Cx) -> ToolOutput {
        let input: Input = match parse_input("read_file", input, USAGE) {
            Ok(i) => i,
            Err(e) => return e,
        };
        self.read(input)
    }

    fn summarize(&self, input: &Value) -> String {
        let path = input_path(input);
        let offset = input.get("offset").and_then(Value::as_u64);
        let limit = input.get("limit").and_then(Value::as_u64);
        match (offset, limit) {
            (None, None) => path.to_string(),
            (o, Some(l)) => {
                let start = o.unwrap_or(1);
                format!("{path}:{start}-{}", start + l.saturating_sub(1))
            }
            (Some(o), None) => format!("{path}:{o}-"),
        }
    }
}

impl ReadFile {
    fn read(&self, input: Input) -> ToolOutput {
        let ctx = &self.ctx;
        let path = match ctx.guard.resolve(&input.path) {
            Ok(p) => p,
            Err(e) => return ToolOutput::error(e),
        };
        let rel = ctx.rel(&path);
        let meta = match std::fs::metadata(&path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return not_found(
                    ctx,
                    &path,
                    "Use glob_files or list_files to find the right path.",
                );
            }
            Err(e) => return ToolOutput::error(format!("Could not read `{rel}`: {e}.")),
        };
        if meta.is_dir() {
            return ToolOutput::error(format!(
                "`{rel}` is a directory. Use list_files to see what it contains."
            ));
        }
        if meta.len() > MAX_FILE_BYTES {
            return ToolOutput::error(format!(
                "`{rel}` is {} bytes, too large to read (limit {MAX_FILE_BYTES}). Use \
                 grep_text to find the part you need.",
                meta.len()
            ));
        }
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => return ToolOutput::error(format!("Could not read `{rel}`: {e}.")),
        };
        if ch_kernel::util::looks_binary(&bytes) {
            return ToolOutput::error(format!(
                "`{rel}` looks like a binary file ({} bytes); read_file only shows text. Use \
                 bash (`file`, `xxd | head`) if you need to inspect it.",
                bytes.len()
            ));
        }
        if bytes.is_empty() {
            ctx.state.record_read(&path, &bytes, true);
            return ToolOutput::ok(format!("`{rel}` is empty (0 bytes)."));
        }
        if input.offset == Some(0) || input.limit == Some(0) {
            return ToolOutput::error(
                "offset and limit start at 1 (offset is a 1-based line number). Omit them to \
                 read from the start.",
            );
        }

        let text = String::from_utf8_lossy(&bytes);
        let lines: Vec<&str> = text.lines().collect();
        let total = lines.len();
        let start = input.offset.unwrap_or(1);
        if start > total {
            return ToolOutput::error(format!(
                "offset {start} is past the end of `{rel}`, which has {total} lines. Use an \
                 offset between 1 and {total}."
            ));
        }
        let limit = input.limit.unwrap_or(DEFAULT_LIMIT);
        let budget = ctx.max_output_chars.saturating_sub(600).max(2_000);

        let mut out = String::new();
        let mut end = start - 1;
        let mut cut_lines = 0usize;
        let mut hit_budget = false;
        for (i, line) in lines.iter().enumerate().skip(start - 1).take(limit) {
            let shown: String;
            let line = if line.chars().count() > MAX_LINE_CHARS {
                cut_lines += 1;
                shown = format!(
                    "{}… [line cut at {MAX_LINE_CHARS} chars]",
                    line.chars().take(MAX_LINE_CHARS).collect::<String>()
                );
                shown.as_str()
            } else {
                line
            };
            let numbered = format!("{:>6}\t{line}\n", i + 1);
            if end >= start && out.len() + numbered.len() > budget {
                hit_budget = true;
                break;
            }
            out.push_str(&numbered);
            end = i + 1;
        }
        ctx.state.record_lines(&path, &bytes, start, end, total);

        let mut notes = Vec::new();
        if end < total {
            let why = if hit_budget {
                " (output size limit)"
            } else {
                ""
            };
            notes.push(format!(
                "Showing lines {start}-{end} of {total}{why}. Call read_file with offset={} \
                 to read more.",
                end + 1
            ));
        } else if start > 1 {
            notes.push(format!("Showing lines {start}-{end} of {total}."));
        }
        if cut_lines > 0 {
            notes.push(format!(
                "{cut_lines} line(s) longer than {MAX_LINE_CHARS} chars were cut."
            ));
        }
        if !notes.is_empty() {
            out.push_str(&format!("\n({})", notes.join(" ")));
        }
        ToolOutput::ok(out).with_metadata(json!({
            "path": rel,
            "start_line": start,
            "end_line": end,
            "total_lines": total,
        }))
    }
}
