//! File and search tools: `read_file`, `edit_file`, `write_file`, `list_files`,
//! `glob_files` and `grep_text`.
//!
//! Every path goes through the `safety` plugin's [`PathGuard`], and edits and
//! overwrites go through its [`FileState`] (read-before-write, with the content hash
//! re-checked at edit time so changes made through bash can't slip past it).

pub mod common;
pub mod edit;
pub mod glob;
pub mod grep;
pub mod list;
pub mod read;
pub mod write;

use std::sync::Arc;

use anyhow::Context as _;
use ch_kernel::{Plugin, PluginManifest, RegisterCx, Tool};
use ch_safety::{FileState, PathGuard};

pub use common::FsContext;
pub use edit::EditFile;
pub use glob::GlobFiles;
pub use grep::GrepText;
pub use list::ListFiles;
pub use read::ReadFile;
pub use write::WriteFile;

/// Names of the tools this plugin registers.
pub const TOOL_NAMES: [&str; 6] = [
    "read_file",
    "edit_file",
    "write_file",
    "list_files",
    "glob_files",
    "grep_text",
];

/// The `tools-fs` plugin.
pub struct ToolsFsPlugin;

/// The plugin as a trait object, for the CLI's plugin list.
pub fn plugin() -> Arc<dyn Plugin> {
    Arc::new(ToolsFsPlugin)
}

/// All six tools sharing `ctx`.
pub fn tools(ctx: &FsContext) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(ReadFile::new(ctx.clone())),
        Arc::new(EditFile::new(ctx.clone())),
        Arc::new(WriteFile::new(ctx.clone())),
        Arc::new(ListFiles::new(ctx.clone())),
        Arc::new(GlobFiles::new(ctx.clone())),
        Arc::new(GrepText::new(ctx.clone())),
    ]
}

impl Plugin for ToolsFsPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest::new(
            "tools-fs",
            "File and search tools: read_file, edit_file, write_file, list_files, glob_files, grep_text",
        )
        .requires(&["safety"])
        .provides(&TOOL_NAMES)
    }

    fn register(&self, cx: &mut RegisterCx<'_>) -> anyhow::Result<()> {
        let guard = cx.service::<PathGuard>().context(
            "tools-fs needs the PathGuard service from the `safety` plugin; enable `safety`",
        )?;
        let state = cx.service::<FileState>().context(
            "tools-fs needs the FileState service from the `safety` plugin; enable `safety`",
        )?;
        let mut ctx = FsContext::new(guard, state);
        ctx.max_output_chars = cx.full_config().kernel()?.agent.tool_result_max_chars;
        for tool in tools(&ctx) {
            cx.add_tool_arc(tool);
        }
        Ok(())
    }
}
