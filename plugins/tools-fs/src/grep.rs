//! `grep_text`.

use std::path::Path;

use ch_kernel::reexports::{CancellationToken, async_trait};
use ch_kernel::{Cx, Tool, ToolOutput, ToolSpec};
use grep_regex::RegexMatcherBuilder;
use grep_searcher::sinks::Lossy;
use grep_searcher::{BinaryDetection, SearcherBuilder};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::common::{FileGlob, FsContext, blocking, not_found, parse_input, walker};

/// Matching lines shown before the output is cut.
pub const MAX_MATCHES: usize = 200;
/// Matching lines longer than this are cut.
pub const MAX_LINE_CHARS: usize = 300;

const USAGE: &str = "{\"query\": string, \"path\"?: string, \"include\"?: string}";

/// Regex search over files.
pub struct GrepText {
    ctx: FsContext,
}

impl GrepText {
    pub fn new(ctx: FsContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct Input {
    query: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    include: Option<String>,
}

#[async_trait]
impl Tool for GrepText {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "grep_text".into(),
            description: format!(
                "Search file contents with a regular expression (ripgrep engine). Prints \
                 `path:line:text` for each matching line.\n\
                 When: finding where something is defined or used, error strings, TODOs. \
                 To find files by name use glob_files.\n\
                 Protocol: `query` is a Rust regex (`(?i)` for case-insensitive; escape \
                 `( ) [ ] {{ }} . * + ? | \\` to match them literally). `path` is a \
                 directory or a single file (default: the workspace root). `include` is a \
                 glob that limits which files are searched (`*.rs`, `src/**/*.ts`). \
                 Respects .gitignore, skips binary files and .git, node_modules, target and \
                 similar directories. At most {MAX_MATCHES} matches are shown.\n\
                 Error modes: invalid regex (escape special characters), invalid include \
                 glob, path missing or outside the workspace. No match is not an error.\n\
                 Side effects: none."
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Regular expression to search for."},
                    "path": {"type": "string", "description": "Directory or file to search, relative to the workspace root. Defaults to the root."},
                    "include": {"type": "string", "description": "Only search files matching this glob, e.g. `*.rs`."}
                },
                "required": ["query"]
            }),
            read_only: true,
        }
    }

    async fn call(&self, input: Value, cx: &Cx) -> ToolOutput {
        let input: Input = match parse_input("grep_text", input, USAGE) {
            Ok(i) => i,
            Err(e) => return e,
        };
        let ctx = self.ctx.clone();
        let cancel = cx.cancel.clone();
        blocking(move || grep(&ctx, input, &cancel)).await
    }

    fn summarize(&self, input: &Value) -> String {
        let query = input.get("query").and_then(Value::as_str).unwrap_or("");
        let mut s = format!("\"{}\"", ch_kernel::util::truncate_middle(query, 80));
        if let Some(p) = input.get("path").and_then(Value::as_str)
            && !p.is_empty()
            && p != "."
        {
            s.push_str(&format!(" in {p}"));
        }
        if let Some(i) = input.get("include").and_then(Value::as_str) {
            s.push_str(&format!(" ({i})"));
        }
        s
    }
}

fn grep(ctx: &FsContext, input: Input, cancel: &CancellationToken) -> ToolOutput {
    if input.query.is_empty() {
        return ToolOutput::error("query is empty. Give a regex to search for.");
    }
    let matcher = match RegexMatcherBuilder::new()
        .line_terminator(Some(b'\n'))
        .build(&input.query)
    {
        Ok(m) => m,
        Err(e) => {
            return ToolOutput::error(format!(
                "Invalid regex `{}`: {e}. Escape special characters ( ) [ ] {{ }} . * + ? | \\ \
                 with a backslash to match them literally, e.g. `foo\\(` for `foo(`.",
                input.query
            ));
        }
    };
    let include = match input.include.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(g) => match FileGlob::new(g) {
            Ok(m) => Some(m),
            Err(e) => return ToolOutput::error(e),
        },
    };
    let base = match ctx.resolve_or_root(input.path.as_deref()) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let rel_base = ctx.rel(&base);
    let meta = match std::fs::metadata(&base) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return not_found(ctx, &base, "Give an existing file or directory as `path`.");
        }
        Err(e) => return ToolOutput::error(format!("Could not search `{rel_base}`: {e}.")),
    };

    let mut searcher = SearcherBuilder::new()
        .binary_detection(BinaryDetection::quit(b'\x00'))
        .line_number(true)
        .build();
    let mut lines: Vec<String> = Vec::new();
    let mut total = 0usize;
    let mut search_file = |path: &Path, lines: &mut Vec<String>, total: &mut usize| {
        let rel = ctx.rel(path);
        // Errors on a single file (permissions, races) are skipped like ripgrep does.
        let _ = searcher.search_path(
            &matcher,
            path,
            Lossy(|lnum, line| {
                *total += 1;
                if lines.len() < MAX_MATCHES {
                    let text = line.trim_end_matches(['\n', '\r']);
                    let text = if text.chars().count() > MAX_LINE_CHARS {
                        format!("{}…", text.chars().take(MAX_LINE_CHARS).collect::<String>())
                    } else {
                        text.to_string()
                    };
                    lines.push(format!("{rel}:{lnum}:{text}"));
                }
                // Keep counting a little past the cap so the note can say "more".
                Ok(*total <= MAX_MATCHES * 5)
            }),
        );
    };

    if meta.is_file() {
        search_file(&base, &mut lines, &mut total);
    } else {
        for entry in walker(&base, None, None) {
            if cancel.is_cancelled() {
                return ToolOutput::error("Cancelled.");
            }
            if total > MAX_MATCHES * 5 {
                break;
            }
            let Ok(entry) = entry else { continue };
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            if let Some(inc) = &include {
                let rel = entry.path().strip_prefix(&base).unwrap_or(entry.path());
                if !inc.is_match(rel) {
                    continue;
                }
            }
            search_file(entry.path(), &mut lines, &mut total);
        }
    }

    if lines.is_empty() {
        let scope = match &input.include {
            Some(i) if !i.trim().is_empty() => format!("`{rel_base}` (files matching `{i}`)"),
            _ => format!("`{rel_base}`"),
        };
        return ToolOutput::ok(format!("No matches for `{}` in {scope}.", input.query))
            .with_metadata(json!({ "matches": 0 }));
    }
    let mut out = lines.join("\n");
    if total > lines.len() {
        let count = if total > MAX_MATCHES * 5 {
            format!("more than {}", MAX_MATCHES * 5)
        } else {
            total.to_string()
        };
        out.push_str(&format!(
            "\n\n(Showing the first {} of {count} matches. Narrow your search with `path`, \
             `include` or a more specific query.)",
            lines.len()
        ));
    }
    ToolOutput::ok(out).with_metadata(json!({ "matches": total }))
}
