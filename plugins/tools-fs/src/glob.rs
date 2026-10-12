//! `glob_files`.

use std::path::Component;
use std::time::SystemTime;

use ch_kernel::reexports::{CancellationToken, async_trait};
use ch_kernel::{Cx, Tool, ToolOutput, ToolSpec};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::common::{FileGlob, FsContext, blocking, not_found, parse_input, walker};

/// Matches shown before the list is cut.
pub const MAX_MATCHES: usize = 200;

const USAGE: &str = "{\"pattern\": string, \"path\"?: string}";

/// Finds files by glob pattern.
pub struct GlobFiles {
    ctx: FsContext,
}

impl GlobFiles {
    pub fn new(ctx: FsContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct Input {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
}

#[async_trait]
impl Tool for GlobFiles {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "glob_files".into(),
            description: format!(
                "Find files whose path matches a glob pattern, newest first.\n\
                 When: you know (part of) a file name or extension, e.g. `**/*.rs`, \
                 `src/**/test_*.py`, `Cargo.toml`. To search file contents use grep_text.\n\
                 Protocol: `pattern` is matched against paths relative to `path` (default: \
                 the workspace root). `*` stays within one directory, `**` crosses \
                 directories, `{{a,b}}` gives alternatives. A pattern without `/` matches \
                 file names at any depth. Only files are returned, sorted by modification \
                 time (newest first), at most {MAX_MATCHES}. Respects .gitignore and skips \
                 .git, node_modules, target and similar directories.\n\
                 Error modes: invalid glob, pattern with `..` or an absolute path (use \
                 `path` to choose the directory), path missing or outside the workspace. \
                 No match is not an error: you get an empty result.\n\
                 Side effects: none."
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Glob pattern, e.g. `**/*.rs` or `*.md`."},
                    "path": {"type": "string", "description": "Directory to search in, relative to the workspace root. Defaults to the root."}
                },
                "required": ["pattern"]
            }),
            read_only: true,
        }
    }

    async fn call(&self, input: Value, cx: &Cx) -> ToolOutput {
        let input: Input = match parse_input("glob_files", input, USAGE) {
            Ok(i) => i,
            Err(e) => return e,
        };
        let ctx = self.ctx.clone();
        let cancel = cx.cancel.clone();
        blocking(move || glob(&ctx, input, &cancel)).await
    }

    fn summarize(&self, input: &Value) -> String {
        let pattern = input.get("pattern").and_then(Value::as_str).unwrap_or("");
        match input.get("path").and_then(Value::as_str) {
            Some(p) if !p.is_empty() && p != "." => format!("{pattern} in {p}"),
            _ => pattern.to_string(),
        }
    }
}

fn glob(ctx: &FsContext, input: Input, cancel: &CancellationToken) -> ToolOutput {
    let pattern = input.pattern.trim();
    if pattern.is_empty() {
        return ToolOutput::error("pattern is empty. Give a glob such as `**/*.rs`.");
    }
    let p = std::path::Path::new(pattern);
    if p.is_absolute() || p.components().any(|c| c == Component::ParentDir) {
        return ToolOutput::error(format!(
            "pattern `{pattern}` must be relative and can't contain `..`. Put the directory \
             in `path` (inside the workspace) and keep the pattern relative to it."
        ));
    }
    let matcher = match FileGlob::new(pattern) {
        Ok(m) => m,
        Err(e) => return ToolOutput::error(e),
    };
    let base = match ctx.resolve_or_root(input.path.as_deref()) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let rel_base = ctx.rel(&base);
    match std::fs::metadata(&base) {
        Ok(m) if !m.is_dir() => {
            return ToolOutput::error(format!(
                "`{rel_base}` is a file, not a directory. Give a directory as `path`."
            ));
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return not_found(ctx, &base, "Give an existing directory as `path`.");
        }
        Err(e) => return ToolOutput::error(format!("Could not search `{rel_base}`: {e}.")),
    }

    let mut found: Vec<(SystemTime, String)> = Vec::new();
    for entry in walker(&base, None, None) {
        if cancel.is_cancelled() {
            return ToolOutput::error("Cancelled.");
        }
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let Ok(rel_to_base) = entry.path().strip_prefix(&base) else {
            continue;
        };
        if !matcher.is_match(rel_to_base) {
            continue;
        }
        let mtime = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        found.push((mtime, ctx.rel(entry.path())));
    }
    found.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let total = found.len();
    if total == 0 {
        return ToolOutput::ok(format!("No files match `{pattern}` in `{rel_base}`."))
            .with_metadata(json!({ "matches": 0 }));
    }
    let mut out: Vec<String> = found
        .into_iter()
        .take(MAX_MATCHES)
        .map(|(_, p)| p)
        .collect();
    if total > MAX_MATCHES {
        out.push(format!(
            "\n(Showing the {MAX_MATCHES} newest of {total} matches. Use a more specific \
             pattern or path.)"
        ));
    }
    ToolOutput::ok(out.join("\n")).with_metadata(json!({ "matches": total }))
}
