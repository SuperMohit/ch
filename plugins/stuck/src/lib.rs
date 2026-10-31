//! Stuck detection (plugin `stuck`): notices a model repeating itself and steers it out,
//! or ends the turn.
//!
//! Calls are compared by tool and canonical input, and only consecutive identical calls
//! form a streak. Calls a permission check or the user refused don't count.
//!
//! - After `remind_after` identical calls in a row, the result gets a one-time note
//!   (`prompts/stuck-repeat.md`, or `prompts/stuck-failing.md` if they all failed).
//! - A call that failed `refuse_after` times in a row is refused before it runs, with
//!   the last error repeated, so the model changes approach.
//! - `stop_after` identical failures in a row (refusals included) end the turn.
//! - `malformed_steps` model responses in a row whose tool calls all have malformed
//!   input (unparseable JSON, or input that breaks the tool's schema) end the turn.
//!
//! Every count starts again with each user prompt.
//!
//! ```toml
//! [plugins.stuck]
//! remind_after = 5     # 0 turns each limit off
//! refuse_after = 2
//! stop_after = 8
//! malformed_steps = 3
//! ```

mod input;
mod tracker;

use std::sync::{Arc, Mutex, MutexGuard};

use ch_kernel::reexports::async_trait;
use ch_kernel::{
    ContentBlock, Cx, Hook, HookDecision, HookEvent, HookPoint, Plugin, PluginManifest, RegisterCx,
    StopReason,
};
use serde::Deserialize;

use input::{call_key, is_malformed};
use tracker::{After, Outcome, Tracker};

/// Plugin id, also the hook id.
pub const ID: &str = "stuck";

/// `[plugins.stuck]`. Zero turns a limit off.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StuckConfig {
    /// Identical calls in a row before the one-time reminder.
    pub remind_after: u32,
    /// Identical failures in a row after which the same call is refused.
    pub refuse_after: u32,
    /// Identical failures in a row (refusals included) that end the turn.
    pub stop_after: u32,
    /// Responses in a row with only malformed tool calls that end the turn.
    pub malformed_steps: u32,
}

impl Default for StuckConfig {
    fn default() -> Self {
        Self {
            remind_after: 5,
            refuse_after: 2,
            stop_after: 8,
            malformed_steps: 3,
        }
    }
}

/// Registers the hook.
pub struct StuckPlugin;

/// The plugin, for the CLI's plugin list.
pub fn plugin() -> Arc<dyn Plugin> {
    Arc::new(StuckPlugin)
}

impl Plugin for StuckPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest::new(
            ID,
            "Stuck detection: steers the model out of repeated calls and ends hopeless turns",
        )
        .version(env!("CARGO_PKG_VERSION"))
        .provides(&["hook:stuck"])
        .project_settings(&[
            "remind_after",
            "refuse_after",
            "stop_after",
            "malformed_steps",
        ])
    }

    fn register(&self, cx: &mut RegisterCx<'_>) -> anyhow::Result<()> {
        let config: StuckConfig = cx.config()?;
        cx.add_hook(StuckHook::new(config));
        Ok(())
    }
}

/// The `user_prompt`, `after_model`, `pre_tool` and `post_tool` hook.
pub struct StuckHook {
    config: StuckConfig,
    /// The session the counts belong to, and the counts.
    state: Mutex<(String, Tracker)>,
}

impl StuckHook {
    pub fn new(config: StuckConfig) -> Self {
        Self {
            config,
            state: Mutex::default(),
        }
    }

    /// The counts for `cx`'s session; another session (after `/clear` or `/resume`)
    /// starts from zero.
    fn tracker(&self, cx: &Cx) -> MutexGuard<'_, (String, Tracker)> {
        // Every update leaves the counts consistent, so a poisoned lock is still usable.
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.0 != *cx.session_id {
            *state = (cx.session_id.to_string(), Tracker::default());
        }
        state
    }
}

#[async_trait]
impl Hook for StuckHook {
    fn id(&self) -> &str {
        ID
    }

    fn points(&self) -> &[HookPoint] {
        &[
            HookPoint::UserPrompt,
            HookPoint::AfterModel,
            HookPoint::PreTool,
            HookPoint::PostTool,
        ]
    }

    async fn on(&self, event: &HookEvent, cx: &Cx) -> HookDecision {
        let cfg = &self.config;
        match event {
            HookEvent::UserPrompt { .. } => {
                self.tracker(cx).1 = Tracker::default();
                HookDecision::Continue
            }
            HookEvent::AfterModel { response } => {
                // A response cut off by the output limit is the kernel's to handle.
                let all_malformed = (response.stop_reason != StopReason::MaxTokens)
                    .then(|| all_malformed(&response.message.content, cx));
                match self.tracker(cx).1.response(all_malformed, cfg) {
                    Some(reason) => HookDecision::stop(reason),
                    None => HookDecision::Continue,
                }
            }
            HookEvent::PreTool { call, .. } => {
                let key = call_key(&call.name, &call.input);
                match self.tracker(cx).1.refusal(&call.name, &key, cfg) {
                    Some(reason) => HookDecision::deny(reason),
                    None => HookDecision::Continue,
                }
            }
            HookEvent::PostTool {
                call,
                output,
                not_run,
            } => {
                let key = call_key(&call.name, &call.input);
                let mut state = self.tracker(cx);
                let tracker = &mut state.1;
                let outcome = match (*not_run, output.is_error) {
                    (true, _) if tracker.refusal(&call.name, &key, cfg).is_some() => {
                        Outcome::Refused
                    }
                    (true, _) => Outcome::Skipped,
                    (false, true) => Outcome::Failed(&output.content),
                    (false, false) => Outcome::Ok,
                };
                match tracker.record(&call.name, key, outcome, cfg) {
                    After::Nothing => HookDecision::Continue,
                    After::Note(text) => HookDecision::note(text),
                    After::Stop(reason) => HookDecision::stop(reason),
                }
            }
            _ => HookDecision::Continue,
        }
    }
}

/// Whether `content` calls tools and every call has malformed input. A call to an
/// unknown tool isn't malformed: the kernel answers it with the list of tools.
fn all_malformed(content: &[ContentBlock], cx: &Cx) -> bool {
    let mut calls = content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolUse { name, input, .. } => Some((name, input)),
            _ => None,
        })
        .peekable();
    calls.peek().is_some()
        && calls.all(|(name, input)| {
            cx.tools
                .iter()
                .find(|t| t.name == *name)
                .is_some_and(|t| is_malformed(input, &t.input_schema))
        })
}
