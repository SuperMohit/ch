//! An example external ch plugin: a `scan` tool that finds TODO and FIXME comments in
//! the workspace, a system prompt line saying so, and a `/todos` command.
//!
//! ch runs it as `ext__todo-scanner__scan`. See `plugin.toml` next to this crate.

use std::path::{Path, PathBuf};

use ch_plugin_sdk::Plugin;
use ch_plugin_sdk::protocol::CommandRunResult;
use serde_json::json;

/// Files larger than this are skipped.
const MAX_FILE: u64 = 1024 * 1024;
const DEFAULT_LIMIT: usize = 100;
/// Most results one call returns, whatever `limit` asks for.
const MAX_LIMIT: usize = 1000;

/// Directories never searched.
const SKIP: &[&str] = &[".git", "target", "node_modules", ".venv", "__pycache__"];

fn main() -> std::io::Result<()> {
    Plugin::new()
        .tool(
            "scan",
            "Lists TODO and FIXME comments in the workspace as `path:line: text`. \
             `path` limits the search to a directory inside the workspace; `limit` caps \
             the number of results (default 100, at most 1000). Read-only.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "A directory inside the workspace"},
                    "limit": {"type": "integer", "minimum": 1}
                }
            }),
            |input, cx| {
                let root = PathBuf::from(cx.workspace());
                let root = root.canonicalize().unwrap_or(root);
                let limit = input["limit"]
                    .as_u64()
                    .map_or(DEFAULT_LIMIT, |n| (n as usize).clamp(1, MAX_LIMIT));
                let start = match input["path"].as_str() {
                    Some(p) => inside(&root, p)?,
                    None => root.clone(),
                };
                let mut found = Vec::new();
                scan(&root, &start, limit, &mut found);
                if found.is_empty() {
                    Ok("No TODO or FIXME comments found.".into())
                } else {
                    let more = if found.len() >= limit {
                        format!("\n(stopped at {limit}; pass a larger `limit` or a `path`)")
                    } else {
                        String::new()
                    };
                    Ok(format!("{}{more}", found.join("\n")))
                }
            },
        )
        .system_prompt(|_| {
            vec!["To find TODO and FIXME comments, use `ext__todo-scanner__scan` rather than grepping.".into()]
        })
        .command("todos", |args, _| CommandRunResult {
            message: None,
            prompt: Some(format!(
                "List the TODO and FIXME comments in this workspace with the scan tool, \
                 group them by area, and suggest which to do first. {args}"
            )),
        })
        .run()
}

/// `p` resolved inside `root`, or why not.
fn inside(root: &Path, p: &str) -> Result<PathBuf, String> {
    let real_root = root.canonicalize().map_err(|e| e.to_string())?;
    let real = root
        .join(p)
        .canonicalize()
        .map_err(|e| format!("`{p}`: {e}"))?;
    if real.starts_with(&real_root) {
        Ok(real)
    } else {
        Err(format!("`{p}` is outside the workspace"))
    }
}

fn scan(root: &Path, dir: &Path, limit: usize, found: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        if found.len() >= limit {
            return;
        }
        let path = entry.path();
        let name = entry.file_name();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            if !SKIP.iter().any(|s| name == *s) {
                scan(root, &path, limit, found);
            }
            continue;
        }
        if !kind.is_file() {
            continue;
        }
        let Some(text) = read_regular(&path) else {
            continue;
        };
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .display()
            .to_string();
        for (i, line) in text.lines().enumerate() {
            if line.contains("TODO") || line.contains("FIXME") {
                found.push(format!("{rel}:{}: {}", i + 1, line.trim()));
                if found.len() >= limit {
                    return;
                }
            }
        }
    }
}

/// The file's text, if it is still a regular file of at most [`MAX_FILE`] bytes when
/// opened. It is opened without following a symlink and without blocking, then checked
/// through the open handle: a file swapped for a link (to outside the workspace) or a
/// FIFO after the directory was read is skipped, not read or waited on.
fn read_regular(path: &Path) -> Option<String> {
    use std::io::Read as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() || meta.len() > MAX_FILE {
        return None;
    }
    let mut text = String::new();
    file.read_to_string(&mut text).ok()?;
    Some(text)
}
