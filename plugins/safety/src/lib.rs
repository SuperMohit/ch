//! Path guard, file state and permission policy.
//!
//! The `safety` plugin provides two services other plugins rely on:
//!
//! - [`PathGuard`] resolves model-supplied paths and rejects anything outside the
//!   workspace (including `..` tricks and symlink escapes).
//! - [`FileState`] tracks what the agent has read, so edits require a full, current read.
//!
//! It also adds a `pre_tool` hook (priority -100) implementing the permission policy
//! from `[plugins.safety]`:
//!
//! ```toml
//! [plugins.safety]
//! mode = "ask"                 # "ask" | "accept-edits" | "bypass"
//! allow = ["bash:npm test *", "edit_file:src/**"]
//! ask = ["bash:git commit *"]
//! deny = ["web_fetch:http://*"]
//! ```
//!
//! Precedence: the built-in bash deny list > `deny` > `ask` > `allow` > the mode's
//! default. Defaults per mode:
//!
//! | | ask | accept-edits | bypass |
//! |---|---|---|---|
//! | read-only tools (`read_file`, `grep_text`, ...) | run | run | run |
//! | `edit_file` / `write_file` | ask | run | run |
//! | writes to `.git/`, `.env*`, `.ch/config.toml` | ask | ask | run |
//! | `bash` on the read-only allowlist | run | run | run |
//! | other `bash` | ask | ask | run |
//! | `web_fetch` | ask (per origin) | ask (per origin) | run |
//! | other non-read-only tools (MCP, ...) | ask | ask | run |
//! | built-in deny list, paths outside the workspace | deny | deny | deny |
//!
//! `web_fetch` asks in accept-edits mode too: fetching is not an edit, and a page can
//! carry instructions while a URL can carry data out. The key offered for "always
//! allow" covers the whole origin (`web_fetch:https://docs.rs/*`).
//!
//! A bare allow rule like `edit_file` does not cover sensitive paths; a rule naming the
//! path (`edit_file:.env`) does.

pub mod bash;
pub mod file_state;
pub mod path_guard;
pub mod policy;
pub mod rules;

use std::sync::Arc;

use anyhow::Context as _;
use ch_kernel::reexports::async_trait;
use ch_kernel::{Cx, Hook, HookDecision, HookEvent, HookPoint, Plugin, PluginManifest, RegisterCx};

pub use file_state::{FileState, content_hash};
pub use path_guard::PathGuard;
pub use policy::{Decision, Mode, Policy, SafetyConfig, is_sensitive};
pub use rules::Rule;

/// The `safety` plugin.
pub struct SafetyPlugin;

/// The plugin as a trait object, for the CLI's plugin list.
pub fn plugin() -> Arc<dyn Plugin> {
    Arc::new(SafetyPlugin)
}

impl Plugin for SafetyPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest::new(
            "safety",
            "Path guard, read-before-write tracking and the allow/ask/deny permission policy",
        )
        .provides(&["service:PathGuard", "service:FileState", "hook:pre_tool"])
    }

    fn register(&self, cx: &mut RegisterCx<'_>) -> anyhow::Result<()> {
        let config: SafetyConfig = cx.config().context("invalid [plugins.safety] config")?;
        let guard = Arc::new(PathGuard::new(cx.workspace()).with_context(|| {
            format!(
                "workspace root `{}` can't be resolved",
                cx.workspace().display()
            )
        })?);
        let state = Arc::new(FileState::with_root(guard.root()));
        let policy = Policy::new(&config, guard.clone())?;
        cx.provide_service(guard)?;
        cx.provide_service(state)?;
        cx.add_hook(PermissionHook::new(policy));
        Ok(())
    }
}

/// The `pre_tool` hook that applies a [`Policy`].
pub struct PermissionHook {
    policy: Policy,
}

impl PermissionHook {
    pub fn new(policy: Policy) -> Self {
        Self { policy }
    }
}

#[async_trait]
impl Hook for PermissionHook {
    fn id(&self) -> &str {
        "safety"
    }

    fn points(&self) -> &[HookPoint] {
        &[HookPoint::PreTool]
    }

    fn priority(&self) -> i32 {
        -100
    }

    async fn on(&self, event: &mut HookEvent, _cx: &Cx) -> HookDecision {
        let HookEvent::PreTool { call, read_only } = event else {
            return HookDecision::Continue;
        };
        match self.policy.decide(&call.name, &call.input, *read_only) {
            Decision::Allow => HookDecision::Continue,
            Decision::Ask { reason, key } => HookDecision::ask(reason, key),
            Decision::Deny { reason } => HookDecision::deny(reason),
        }
    }
}
