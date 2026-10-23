//! `write_file`.

use ch_kernel::reexports::async_trait;
use ch_kernel::{Cx, Tool, ToolOutput, ToolSpec};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::common::{FsContext, diff_for_model, input_path, parse_input, unified_diff};

const USAGE: &str = "{\"path\": string, \"content\": string, \"overwrite\"?: boolean}";

/// Creates a file, or replaces one with `overwrite`.
pub struct WriteFile {
    ctx: FsContext,
}

impl WriteFile {
    pub fn new(ctx: FsContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct Input {
    path: String,
    content: String,
    #[serde(default)]
    overwrite: bool,
}

#[async_trait]
impl Tool for WriteFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "write_file".into(),
            description: "Create a new file, or replace an existing file's whole content.\n\
                 When: creating files. For changes to an existing file use edit_file \
                 instead; only rewrite a whole file when most of it changes.\n\
                 Protocol: `path` is relative to the workspace root; missing parent \
                 directories are created. To replace an existing file, read it fully with \
                 read_file first and set `overwrite` to true. `content` is written exactly \
                 as given (end it with a newline for source files).\n\
                 Error modes: file exists and overwrite is not true; existing file not read, \
                 or changed on disk since your last read; path is a directory or outside the \
                 workspace.\n\
                 Side effects: creates or overwrites the file (and parent directories)."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path, relative to the workspace root."},
                    "content": {"type": "string", "description": "The complete file content."},
                    "overwrite": {"type": "boolean", "default": false, "description": "Must be true to replace an existing file."}
                },
                "required": ["path", "content"]
            }),
            read_only: false,
        }
    }

    async fn call(&self, input: Value, _cx: &Cx) -> ToolOutput {
        let input: Input = match parse_input("write_file", input, USAGE) {
            Ok(i) => i,
            Err(e) => return e,
        };
        self.write(input)
    }

    fn summarize(&self, input: &Value) -> String {
        input_path(input).to_string()
    }
}

impl WriteFile {
    fn write(&self, input: Input) -> ToolOutput {
        let ctx = &self.ctx;
        let path = match ctx.guard.resolve(&input.path) {
            Ok(p) => p,
            Err(e) => return ToolOutput::error(e),
        };
        let rel = ctx.rel(&path);
        let previous = match std::fs::metadata(&path) {
            Ok(m) if m.is_dir() => {
                return ToolOutput::error(format!(
                    "`{rel}` is a directory. Give a file path inside it, like `{rel}/name.ext`."
                ));
            }
            Ok(_) => {
                if !input.overwrite {
                    return ToolOutput::error(format!(
                        "`{rel}` already exists. To change part of it use edit_file; to \
                         replace all of it, read it with read_file and call write_file again \
                         with overwrite=true."
                    ));
                }
                match ctx.state.check_writable(&path) {
                    Ok(b) => b.map(|b| String::from_utf8_lossy(&b).into_owned()),
                    Err(msg) => return ToolOutput::error(msg),
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return ToolOutput::error(format!("Could not open `{rel}`: {e}.")),
        };
        if let Some(parent) = path.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            return ToolOutput::error(format!(
                "Could not create the directory `{}`: {e}.",
                ctx.rel(parent)
            ));
        }
        if let Err(e) = std::fs::write(&path, &input.content) {
            return ToolOutput::error(format!("Could not write `{rel}`: {e}."));
        }
        ctx.state.record_write(&path, input.content.as_bytes());

        let old = previous.as_deref().unwrap_or("");
        let (diff, added, removed) = unified_diff(&rel, old, &input.content);
        let content = match &previous {
            None => format!(
                "Created `{rel}` ({} lines, {} bytes).",
                input.content.lines().count(),
                input.content.len()
            ),
            Some(p) if p == &input.content => {
                format!("`{rel}` already had exactly this content; nothing changed.")
            }
            Some(_) => format!(
                "Overwrote `{rel}` (+{added} -{removed}).\n{}",
                diff_for_model(&diff)
            ),
        };
        ToolOutput::ok(content).with_metadata(json!({ "path": rel, "diff": diff }))
    }
}
