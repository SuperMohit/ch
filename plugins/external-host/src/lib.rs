//! External plugins (plugin `external-host`, roadmap 7.2, ADR 0006).
//!
//! An external plugin is a directory with a `plugin.toml` (see [`manifest`]) and a
//! program that speaks [`ch_plugin_protocol`] over stdio. ch starts every plugin in
//! `<ch home>/plugins/` with the session, in its own process group with a cleared
//! environment (`PATH`, `HOME`, `USER`, `LANG`, `TERM`, `TMPDIR`, `SHELL`,
//! `CH_WORKSPACE`, `CH_PLUGIN_DIR` and the manifest's `env`), in the plugin's
//! directory. What it declares at `initialize` becomes:
//!
//! - **tools** named `ext__<plugin>__<tool>`, through a kernel tool source, so they can
//!   change mid-session (`tools/changed`) and are withdrawn if the plugin exits;
//! - **hooks** at any point, with tool globs for `pre_tool` and `post_tool`. A
//!   `pre_tool` hook that fails or times out, or whose plugin stopped running, denies
//!   the call unless the manifest says `on_error = "allow"`; other points fail open. A
//!   plugin that never finished starting declared no hooks: list the tools it guards
//!   in the manifest's `guards` so those calls are refused until it runs;
//! - **prompt contributions**: system blocks once per conversation, and reminders;
//! - **event subscriptions**, sent as notifications. `session_start` and `session_end`
//!   events come before plugins start and after they stop, so plugins never see them;
//!   use the hooks at those points.
//!
//! Slash commands are declared in `plugin.toml`. A plugin that crashes has its tools
//! withdrawn and is reported once; ch keeps running.
//!
//! ```toml
//! [plugins.external-host]
//! disable = ["todo-scanner"]                   # plugins not to start
//! read_only_tools = ["ext__todo-scanner__scan"] # exact names that don't need approval
//! ```
//!
//! Project plugins (`.ch/plugins/`) are not loaded yet: they need the trust prompt that
//! comes with `ch plugins` (roadmap 7.6).

pub mod conn;
pub mod manifest;

mod adapters;
mod plugin;

use std::sync::Arc;

use ch_kernel::{ChHome, Plugin, PluginManifest, RegisterCx};
use serde::Deserialize;

pub use manifest::Manifest;
pub use plugin::ExternalPlugin;

/// `[plugins.external-host]`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HostConfig {
    /// Plugin names not to start.
    pub disable: Vec<String>,
    /// Exact tool names (`ext__<plugin>__<tool>`) that run without approval.
    pub read_only_tools: Vec<String>,
}

/// The `external-host` plugin.
pub struct ExternalHostPlugin;

impl Plugin for ExternalHostPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest::new(
            "external-host",
            "External plugins from <ch home>/plugins, speaking JSON-RPC over stdio",
        )
        .version(env!("CARGO_PKG_VERSION"))
        .provides(&[
            "tool-source:ext",
            "hooks:ext",
            "prompt:ext",
            "commands:ext",
            "sink:ext",
        ])
    }

    fn register(&self, cx: &mut RegisterCx<'_>) -> anyhow::Result<()> {
        let config: HostConfig = cx.config()?;
        let mut warnings = Vec::new();
        if cx.workspace().join(".ch").join("plugins").is_dir() {
            warnings.push(
                "this project has plugins in .ch/plugins; ch doesn't load project plugins \
                 yet (they will need your trust, roadmap 7.6)"
                    .to_string(),
            );
        }
        let Some(home) = cx.service::<ChHome>() else {
            return Ok(());
        };
        let (found, problems) = manifest::discover(&home.0.join("plugins"));
        warnings.extend(problems);
        let workspace = cx.workspace().to_path_buf();
        let mut commands_taken: Vec<(String, String)> = Vec::new();
        for m in found {
            if config.disable.contains(&m.name) {
                continue;
            }
            let prefix = m.prefix();
            let read_only: Vec<String> = config
                .read_only_tools
                .iter()
                .filter(|t| t.starts_with(&prefix))
                .cloned()
                .collect();
            let commands = m.commands.clone();
            let shared = plugin::Shared::new(m);
            cx.add_tool_source(Arc::new(ExternalPlugin::new(
                shared.clone(),
                workspace.clone(),
                read_only,
            )));
            cx.add_hook(adapters::ExtHook::new(shared.clone()));
            cx.add_prompt(adapters::ExtPrompt::new(shared.clone()));
            cx.add_sink(adapters::ExtSink::new(shared.clone()));
            for decl in commands {
                // Between plugins, the first (by directory name) keeps a name.
                if let Some(owner) = commands_taken.iter().find(|(n, _)| *n == decl.name) {
                    warnings.push(format!(
                        "the plugin `{}` declares /{}, which the plugin `{}` already has",
                        shared.m.name, decl.name, owner.1
                    ));
                    continue;
                }
                commands_taken.push((decl.name.clone(), shared.m.name.clone()));
                cx.add_command(adapters::ExtCommand::new(shared.clone(), decl));
            }
        }
        if !warnings.is_empty() {
            cx.add_hook(adapters::LoadWarnings(warnings));
        }
        Ok(())
    }
}

/// The plugin, for the CLI's plugin list.
pub fn plugin() -> Arc<dyn Plugin> {
    Arc::new(ExternalHostPlugin)
}
