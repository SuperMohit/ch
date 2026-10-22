//! Hooks: plugins that observe or change what happens at fixed points of the turn
//! pipeline.
//!
//! # How decisions combine
//!
//! Hooks registered for a point run in ascending `priority()` order (ties keep
//! registration order). Hooks see the event read-only; a `Rewrite` is the only way to
//! change it, and later hooks see the rewritten input.
//!
//! - `Deny` and `Stop` end the chain immediately; later hooks don't run.
//! - `Ask` is remembered and the chain continues; the result is `Ask` unless a later hook
//!   denies or stops. The first `Ask` reason (and key) is kept.
//! - `Rewrite(input)` replaces the tool input (pre_tool) or the prompt text (user_prompt,
//!   input must be a string) and the chain continues.
//! - `AppendNote(text)` notes are concatenated in order.
//!
//! If any hook rewrites the input, the chain runs a second time on the rewritten input
//! with rewrites ignored, and the second run's verdict and notes are the result. So a
//! hook that ran before the rewrite (such as the permission check, which runs first)
//! always decides on the input that is actually used. Hooks can therefore run twice for
//! one event; keep them free of side effects that must happen only once.
//!
//! What a result means depends on the point:
//!
//! | Point | Deny | Ask | Stop | Notes |
//! |---|---|---|---|---|
//! | user_prompt | turn ends (`PromptDenied`) | treated as Continue | turn ends | appended to the user message as `<system-reminder>` |
//! | before_model | treated as Stop | – | turn ends | added as reminders to this request |
//! | after_model | treated as Stop | – | turn ends | added as a reminder for the next call |
//! | pre_tool | call not run; reason returned as an error | approver asked | call not run, turn ends after this step | appended to the tool result |
//! | post_tool | – | – | turn ends after this step | appended to the tool result |
//! | turn_end (model finished) | – | – | turn ends | turn **continues**: notes are sent to the model as a user message |
//! | session_start / session_end | – | – | – | ignored |

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::cx::Cx;
use crate::model::{ModelRequest, ModelResponse};
use crate::tool::ToolOutput;
use crate::types::{TerminationReason, ToolCall};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookPoint {
    SessionStart,
    UserPrompt,
    BeforeModel,
    AfterModel,
    PreTool,
    PostTool,
    TurnEnd,
    SessionEnd,
}

impl HookPoint {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::SessionStart => "session_start",
            Self::UserPrompt => "user_prompt",
            Self::BeforeModel => "before_model",
            Self::AfterModel => "after_model",
            Self::PreTool => "pre_tool",
            Self::PostTool => "post_tool",
            Self::TurnEnd => "turn_end",
            Self::SessionEnd => "session_end",
        }
    }
}

/// The data a hook sees (and may change) at each point.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "point", rename_all = "snake_case")]
pub enum HookEvent {
    SessionStart,
    UserPrompt {
        text: String,
    },
    BeforeModel {
        request: ModelRequest,
    },
    AfterModel {
        response: ModelResponse,
    },
    PreTool {
        call: ToolCall,
        read_only: bool,
    },
    PostTool {
        call: ToolCall,
        output: ToolOutput,
        /// True when the call never ran: a hook or the user denied it, or the turn was
        /// stopped or cancelled before it.
        not_run: bool,
    },
    TurnEnd {
        /// `Done` when the model stopped calling tools; otherwise why the turn is ending.
        reason: TerminationReason,
        steps: u32,
        /// The last assistant text.
        final_text: String,
    },
    SessionEnd,
}

impl HookEvent {
    pub fn point(&self) -> HookPoint {
        match self {
            Self::SessionStart => HookPoint::SessionStart,
            Self::UserPrompt { .. } => HookPoint::UserPrompt,
            Self::BeforeModel { .. } => HookPoint::BeforeModel,
            Self::AfterModel { .. } => HookPoint::AfterModel,
            Self::PreTool { .. } => HookPoint::PreTool,
            Self::PostTool { .. } => HookPoint::PostTool,
            Self::TurnEnd { .. } => HookPoint::TurnEnd,
            Self::SessionEnd => HookPoint::SessionEnd,
        }
    }

    /// The tool call, for pre_tool and post_tool events.
    pub fn tool_call(&self) -> Option<&ToolCall> {
        match self {
            Self::PreTool { call, .. } | Self::PostTool { call, .. } => Some(call),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum HookDecision {
    Continue,
    Deny {
        reason: String,
    },
    /// Ask the user. `key` identifies what "always allow" would cover for the rest of
    /// the session (e.g. `bash:git commit`); `None` means it can't be remembered.
    Ask {
        reason: String,
        #[serde(default)]
        key: Option<String>,
    },
    Rewrite {
        input: Value,
    },
    AppendNote {
        text: String,
    },
    Stop {
        reason: String,
    },
}

impl HookDecision {
    pub fn deny(reason: impl Into<String>) -> Self {
        Self::Deny {
            reason: reason.into(),
        }
    }

    pub fn ask(reason: impl Into<String>, key: Option<String>) -> Self {
        Self::Ask {
            reason: reason.into(),
            key,
        }
    }

    pub fn note(text: impl Into<String>) -> Self {
        Self::AppendNote { text: text.into() }
    }

    pub fn stop(reason: impl Into<String>) -> Self {
        Self::Stop {
            reason: reason.into(),
        }
    }
}

#[async_trait]
pub trait Hook: Send + Sync {
    fn id(&self) -> &str;
    fn points(&self) -> &[HookPoint];
    /// Lower runs first. Safety/permission hooks use negative priorities.
    fn priority(&self) -> i32 {
        0
    }
    async fn on(&self, event: &HookEvent, cx: &Cx) -> HookDecision;
}

/// The combined verdict of all hooks at one point.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Continue,
    Deny {
        hook: String,
        reason: String,
    },
    Ask {
        hook: String,
        reason: String,
        key: Option<String>,
    },
    Stop {
        hook: String,
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct HookOutcome {
    pub verdict: Verdict,
    /// `(hook id, note)` in order.
    pub notes: Vec<(String, String)>,
}

impl HookOutcome {
    pub fn notes_text(&self) -> Option<String> {
        if self.notes.is_empty() {
            None
        } else {
            Some(
                self.notes
                    .iter()
                    .map(|(_, n)| n.as_str())
                    .collect::<Vec<_>>()
                    .join("\n\n"),
            )
        }
    }
}

/// Runs `hooks` (sorted by priority) over `event` and combines their decisions. When a
/// hook rewrites the input, the chain is checked again on the new input (see the module
/// docs).
pub async fn run_hooks(
    hooks: &[std::sync::Arc<dyn Hook>],
    event: &mut HookEvent,
    cx: &Cx,
) -> HookOutcome {
    let (first, rewritten) = run_chain(hooks, event, cx, true).await;
    let ended = matches!(first.verdict, Verdict::Deny { .. } | Verdict::Stop { .. });
    if ended || !rewritten {
        return first;
    }
    run_chain(hooks, event, cx, false).await.0
}

async fn run_chain(
    hooks: &[std::sync::Arc<dyn Hook>],
    event: &mut HookEvent,
    cx: &Cx,
    allow_rewrite: bool,
) -> (HookOutcome, bool) {
    let point = event.point();
    let mut notes = Vec::new();
    let mut ask: Option<Verdict> = None;
    let mut rewritten = false;
    for hook in hooks.iter().filter(|h| h.points().contains(&point)) {
        let decision = hook.on(event, cx).await;
        let id = hook.id().to_string();
        match decision {
            HookDecision::Continue => {}
            HookDecision::Deny { reason } => {
                let verdict = Verdict::Deny { hook: id, reason };
                return (HookOutcome { verdict, notes }, rewritten);
            }
            HookDecision::Stop { reason } => {
                let verdict = Verdict::Stop { hook: id, reason };
                return (HookOutcome { verdict, notes }, rewritten);
            }
            HookDecision::Ask { reason, key } => {
                if ask.is_none() {
                    ask = Some(Verdict::Ask {
                        hook: id,
                        reason,
                        key,
                    });
                }
            }
            HookDecision::Rewrite { input } if allow_rewrite => {
                rewritten |= apply_rewrite(event, input, &id);
            }
            HookDecision::Rewrite { .. } => {
                tracing::debug!(hook = %id, "rewrite ignored while re-checking a rewritten input");
            }
            HookDecision::AppendNote { text } => notes.push((id, text)),
        }
    }
    let verdict = ask.unwrap_or(Verdict::Continue);
    (HookOutcome { verdict, notes }, rewritten)
}

fn apply_rewrite(event: &mut HookEvent, input: Value, hook: &str) -> bool {
    match event {
        HookEvent::PreTool { call, .. } => {
            let changed = call.input != input;
            call.input = input;
            changed
        }
        HookEvent::UserPrompt { text } => match input {
            Value::String(s) => {
                let changed = *text != s;
                *text = s;
                changed
            }
            other => {
                tracing::warn!(hook, "user_prompt rewrite must be a string, got {other}");
                false
            }
        },
        other => {
            tracing::warn!(hook, point = ?other.point(), "rewrite ignored at this point");
            false
        }
    }
}
