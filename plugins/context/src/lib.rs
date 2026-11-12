//! Context management (plugin `context`): keeps a long conversation inside the model's
//! window by trimming old, bulky content in rare, large batches. The design is in
//! `docs/adr/0004-context-management.md`.
//!
//! The history is never changed. The view sent to the model is computed from it and an
//! append-only compaction log, `<session dir>/context/log.jsonl`, so between trims each
//! request repeats the one before it and only adds to it, and the prompt cache and the
//! model's signed thinking stay valid.
//!
//! - The first message (the task) is never edited, and neither are the newest steps:
//!   at least `recent` of the usable window and at least `min_recent_steps` steps
//!   (except their thinking, and on overflow their results but the newest step's).
//! - When the view passes `soft` of the usable window, the middle is trimmed, in this
//!   order: every call to a `superseded_tools` tool but the latest, tool results longer
//!   than `elide_min_chars`, strings in tool inputs longer than `input_min_chars`, then
//!   old `<system-reminder>` blocks. The trim goes down to `trim_to`, and is made only if
//!   it frees at least `min_free`: a trim rewrites the prompt cache from its first edit,
//!   so it must be rare and worth it.
//! - A trimmed result's stub names a file the model can read back: spill's file, or a
//!   copy in `<session dir>/context/elided/`. Without a session directory it says to
//!   run the command again.
//! - Text the user wrote is never changed.
//! - A trim drops the thinking blocks it invalidates: those after its first edit.
//! - `/compact` and an overflow trim every candidate.
//! - A call whose input repeats a marker or a stub of this session is refused.
//! - Each record is bound to the history it was decided on and is saved durably before
//!   its trim is sent. On load, the log is applied up to its first record that doesn't
//!   match; when the model had answered after that record, a reset record leaves out
//!   all earlier thinking instead (see the ADR's implementation notes).
//!
//! ```toml
//! [plugins.context]
//! strategy = "elide"             # none | elide; summarize and staged act as elide for now
//! soft = 0.60                    # 0.30–0.90
//! hard = 0.85                    # 0.50–0.95, at least soft + 0.05 (for summaries)
//! recent = 0.30                  # 0.10–0.50, below soft
//! min_recent_steps = 2           # 1–10
//! trim_to = 0.40                 # 0.20 up to soft − 0.05
//! min_free = 0.10                # 0.02–0.30
//! elide_min_chars = 500          # 200–20000
//! input_min_chars = 1000         # 200–20000
//! superseded_tools = ["update_plan"]
//! summary_max_words = 1000       # 200–3000 (for summaries)
//! max_summary_share = 0.15       # 0.05–0.30 (for summaries)
//! ```
//!
//! A value out of range is moved into it, with a warning.

mod config;
mod hook;
mod log;
mod plan;
mod render;
mod strategy;
mod stub;

use std::sync::Arc;

use ch_kernel::{Plugin, PluginManifest, RegisterCx};

pub use config::{ContextConfig, Strategy};
pub use hook::{MarkerHook, REFUSED};
pub use strategy::{Context, ID};

/// Registers the strategy and the hook.
pub struct ContextPlugin;

/// The plugin, for the CLI's plugin list.
pub fn plugin() -> Arc<dyn Plugin> {
    Arc::new(ContextPlugin)
}

impl Plugin for ContextPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest::new(
            ID,
            "Context management: trims old, bulky content so long sessions fit the window",
        )
        .version(env!("CARGO_PKG_VERSION"))
        .provides(&["context:context", "hook:context"])
        .project_settings(ContextConfig::KEYS)
    }

    fn register(&self, cx: &mut RegisterCx<'_>) -> anyhow::Result<()> {
        let config: ContextConfig = cx.config()?;
        let (config, warnings) = config.checked();
        for warning in warnings {
            tracing::warn!("{warning}");
        }
        let context = Context::new(config);
        cx.add_hook(context.marker_hook());
        cx.add_context(context);
        Ok(())
    }
}
