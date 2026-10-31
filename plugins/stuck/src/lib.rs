//! Stuck detection (plugin `stuck`): notices a model repeating itself and steers it out,
//! or ends the turn.
//!
//! Calls are compared by tool and input, and only consecutive identical calls form a
//! streak. Calls a permission check or the user refused don't count.
//!
//! - After `remind_after` identical calls in a row, the result gets a one-time note
//!   (`prompts/stuck-repeat.md`, or `prompts/stuck-failing.md` if none succeeded).
//! - A call that failed `refuse_after` times in a row with the same error is refused
//!   before it runs (`prompts/stuck-refused.md`, with that error), so the model changes
//!   approach. A check whose output changes, such as polling a build, is not refused.
//! - `stop_after` identical calls in a row that failed or were refused end the turn.
//! - `malformed_steps` model responses in a row whose tool calls all have malformed
//!   input (unparseable JSON, or a required key missing, null or of the wrong type) end
//!   the turn.
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

use anyhow::bail;

use ch_kernel::reexports::async_trait;
use ch_kernel::{
    ContentBlock, Cx, Hook, HookDecision, HookEvent, HookPoint, Plugin, PluginManifest, RegisterCx,
    StopReason,
};
use serde::Deserialize;

use input::is_malformed;
use tracker::{Outcome, Tracker};

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

impl StuckConfig {
    /// Rejects settings under which a limit could never take effect or would fire on
    /// every call.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.remind_after == 1 {
            bail!(
                "plugins.stuck.remind_after = 1 would add the reminder to every call; use 2 \
                 or more, or 0 to turn it off"
            );
        }
        if self.refuse_after > 0 && self.stop_after > 0 && self.refuse_after >= self.stop_after {
            bail!(
                "plugins.stuck.refuse_after ({}) must be below stop_after ({}), or the turn \
                 ends before a call is ever refused",
                self.refuse_after,
                self.stop_after
            );
        }
        Ok(())
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
        config.validate()?;
        cx.add_hook(StuckHook::new(config));
        Ok(())
    }
}

/// The `user_prompt`, `after_model`, `pre_tool` and `post_tool` hook.
pub struct StuckHook {
    config: StuckConfig,
    tracker: Mutex<Tracker>,
}

impl StuckHook {
    pub fn new(config: StuckConfig) -> Self {
        Self {
            config,
            tracker: Mutex::default(),
        }
    }

    fn tracker(&self) -> MutexGuard<'_, Tracker> {
        // Every update leaves the counts consistent, so a poisoned lock is still usable.
        self.tracker.lock().unwrap_or_else(|p| p.into_inner())
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
            // Every turn starts here, also after `/clear` or `/resume`.
            HookEvent::UserPrompt { .. } => {
                *self.tracker() = Tracker::default();
                HookDecision::Continue
            }
            // A response cut off by the output limit is the kernel's to handle.
            HookEvent::AfterModel { response }
                if response.stop_reason != StopReason::MaxTokens
                    && !response.message.tool_calls().is_empty() =>
            {
                let all_malformed = all_malformed(&response.message.content, cx);
                match self.tracker().response(all_malformed, cfg) {
                    Some(reason) => HookDecision::stop(reason),
                    None => HookDecision::Continue,
                }
            }
            HookEvent::PreTool { call, .. } => {
                self.tracker().check(&call.id, &call.name, &call.input, cfg)
            }
            HookEvent::PostTool {
                call,
                output,
                not_run,
            } => {
                let mut tracker = self.tracker();
                let outcome = match (*not_run, output.is_error) {
                    (true, _) if tracker.take_refused(&call.id) => Outcome::Refused,
                    (true, _) => return HookDecision::Continue,
                    (false, true) => Outcome::Failed(&output.content),
                    (false, false) => Outcome::Ok,
                };
                tracker.record(&call.name, &call.input, outcome, cfg)
            }
            _ => HookDecision::Continue,
        }
    }
}

/// Whether every tool call in `content` has malformed input. A call to an unknown tool
/// isn't malformed: the kernel answers it with the list of tools.
fn all_malformed(content: &[ContentBlock], cx: &Cx) -> bool {
    content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolUse { name, input, .. } => Some((name, input)),
            _ => None,
        })
        .all(|(name, input)| {
            cx.tools
                .iter()
                .find(|t| t.name == *name)
                .is_some_and(|t| is_malformed(input, &t.input_schema))
        })
}

#[cfg(test)]
mod tests {
    use ch_kernel::{Message, ModelResponse, ToolSpec, Usage};
    use serde_json::json;

    use super::*;

    fn response(stop_reason: StopReason) -> HookEvent {
        HookEvent::AfterModel {
            response: ModelResponse {
                message: Message {
                    role: ch_kernel::Role::Assistant,
                    content: vec![ContentBlock::ToolUse {
                        id: "t".into(),
                        name: "check".into(),
                        input: json!({"_invalid_json": "{"}),
                    }],
                },
                stop_reason,
                usage: Usage::default(),
            },
        }
    }

    #[tokio::test]
    async fn cut_off_responses_leave_the_malformed_count_alone() {
        let hook = StuckHook::new(StuckConfig::default());
        let mut cx = Cx::detached("/ws");
        cx.tools = Arc::new(vec![ToolSpec {
            name: "check".into(),
            description: String::new(),
            input_schema: json!({"type": "object"}),
            read_only: true,
        }]);
        for _ in 0..5 {
            let d = hook.on(&response(StopReason::MaxTokens), &cx).await;
            assert_eq!(d, HookDecision::Continue);
        }
        hook.on(&response(StopReason::ToolUse), &cx).await;
        hook.on(&response(StopReason::ToolUse), &cx).await;
        let d = hook.on(&response(StopReason::ToolUse), &cx).await;
        assert!(matches!(d, HookDecision::Stop { .. }), "{d:?}");
    }

    #[test]
    fn settings_that_cannot_work_are_rejected() {
        let with = |f: fn(&mut StuckConfig)| {
            let mut c = StuckConfig::default();
            f(&mut c);
            c.validate()
        };
        assert!(with(|_| {}).is_ok());
        assert!(with(|c| c.remind_after = 1).is_err());
        assert!(with(|c| c.refuse_after = 8).is_err());
        assert!(with(|c| c.stop_after = 0).is_ok());
        assert!(with(|c| c.refuse_after = 0).is_ok());
    }
}
