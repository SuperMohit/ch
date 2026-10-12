//! `list_files`.

use std::sync::Arc;

use ch_kernel::reexports::{CancellationToken, async_trait};
use ch_kernel::{Cx, Tool, ToolOutput, ToolSpec};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::common::{
    FsContext, SKIP_DIRS, Skipped, blocking, human_size, input_path, not_found, parse_input, walker,
};

/// Entries shown before the listing is cut.
pub const MAX_ENTRIES: usize = 500;

const USAGE: &str = "{\"path\"?: string, \"recursive\"?: boolean}";

/// Lists a directory as a tree with sizes.
pub struct ListFiles {
    ctx: FsContext,
}

impl ListFiles {
    pub fn new(ctx: FsContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct Input {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    recursive: bool,
}

#[async_trait]
impl Tool for ListFiles {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "list_files".into(),
            description: format!(
                "List a directory as an indented tree, with file sizes.\n\
                 When: getting oriented in a project or a directory. To find files by name \
                 use glob_files; to find text use grep_text.\n\
                 Protocol: `path` defaults to the workspace root. `recursive` (default false) \
                 lists subdirectories too. Respects .gitignore and never descends into {}. \
                 At most {MAX_ENTRIES} entries are shown.\n\
                 Error modes: path missing, path is a file (use read_file), path outside the \
                 workspace.\n\
                 Side effects: none.",
                SKIP_DIRS.join(", ")
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Directory to list, relative to the workspace root. Defaults to the root."},
                    "recursive": {"type": "boolean", "default": false, "description": "Include subdirectories."}
                }
            }),
            read_only: true,
        }
    }

    async fn call(&self, input: Value, cx: &Cx) -> ToolOutput {
        let input: Input = match parse_input("list_files", input, USAGE) {
            Ok(i) => i,
            Err(e) => return e,
        };
        let ctx = self.ctx.clone();
        let cancel = cx.cancel.clone();
        blocking(move || list(&ctx, input, &cancel)).await
    }

    fn summarize(&self, input: &Value) -> String {
        let recursive = input
            .get("recursive")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let path = input_path(input);
        if recursive {
            format!("{path} (recursive)")
        } else {
            path.to_string()
        }
    }
}

fn list(ctx: &FsContext, input: Input, cancel: &CancellationToken) -> ToolOutput {
    let base = match ctx.resolve_or_root(input.path.as_deref()) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let rel_base = ctx.rel(&base);
    match std::fs::metadata(&base) {
        Ok(m) if !m.is_dir() => {
            return ToolOutput::error(format!(
                "`{rel_base}` is a file, not a directory. Use read_file to read it."
            ));
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return not_found(
                ctx,
                &base,
                "Use list_files on a parent directory to look around.",
            );
        }
        Err(e) => return ToolOutput::error(format!("Could not list `{rel_base}`: {e}.")),
    }

    let skipped: Skipped = Arc::default();
    let depth = if input.recursive { None } else { Some(1) };
    let mut lines = Vec::new();
    let mut total = 0usize;
    for entry in walker(&base, depth, Some(skipped.clone())) {
        if cancel.is_cancelled() {
            return ToolOutput::error("Cancelled.");
        }
        let Ok(entry) = entry else { continue };
        if entry.depth() == 0 {
            continue;
        }
        total += 1;
        if lines.len() >= MAX_ENTRIES {
            continue;
        }
        let indent = "  ".repeat(entry.depth() - 1);
        let name = entry.file_name().to_string_lossy();
        let ft = entry.file_type();
        let line = if ft.is_some_and(|t| t.is_symlink()) {
            match std::fs::read_link(entry.path()) {
                Ok(t) => format!("{indent}{name} -> {}", t.display()),
                Err(_) => format!("{indent}{name} (symlink)"),
            }
        } else if ft.is_some_and(|t| t.is_dir()) {
            format!("{indent}{name}/")
        } else {
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            format!("{indent}{name} ({})", human_size(size))
        };
        lines.push(line);
    }

    let header = if rel_base == "." {
        "./".to_string()
    } else {
        format!("{rel_base}/")
    };
    let mut out = header;
    out.push('\n');
    if lines.is_empty() {
        out.push_str("(empty)\n");
    } else {
        out.push_str(&lines.join("\n"));
        out.push('\n');
    }
    if total > lines.len() {
        out.push_str(&format!(
            "\n(Showing {} of {total} entries. List a subdirectory, or use glob_files to find \
             specific files.)",
            lines.len()
        ));
    }
    let skipped = skipped.lock().map(|s| s.clone()).unwrap_or_default();
    if !skipped.is_empty() {
        out.push_str(&format!(
            "\n(Not listed: {}.)",
            skipped
                .iter()
                .map(|s| format!("{s}/"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    ToolOutput::ok(out)
}
