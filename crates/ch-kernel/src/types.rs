//! Provider-independent conversation types.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
        /// Opaque provider signature that must be sent back unchanged.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    /// Thinking the provider returned encrypted. Must be sent back unchanged.
    RedactedThinking {
        data: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default)]
        is_error: bool,
    },
}

impl ContentBlock {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    /// Characters this block contributes to the prompt (for token estimates).
    pub fn char_len(&self) -> usize {
        match self {
            Self::Text { text } => text.len(),
            Self::Thinking { thinking, .. } => thinking.len(),
            Self::RedactedThinking { data } => data.len(),
            Self::ToolUse { name, input, .. } => name.len() + input.to_string().len(),
            Self::ToolResult { content, .. } => content.len(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentBlock::text(text)],
        }
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: vec![ContentBlock::text(text)],
        }
    }

    /// Concatenated text blocks.
    pub fn text(&self) -> String {
        let mut out = String::new();
        for block in &self.content {
            if let ContentBlock::Text { text } = block {
                out.push_str(text);
            }
        }
        out
    }

    pub fn tool_calls(&self) -> Vec<ToolCall> {
        self.content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolUse { id, name, input } => Some(ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                }),
                _ => None,
            })
            .collect()
    }

    pub fn char_len(&self) -> usize {
        self.content.iter().map(ContentBlock::char_len).sum()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    StopSequence,
    /// The model declined to continue. Tool calls in the message are not run.
    Refusal,
    /// The response was cut off because the context window filled up.
    ContextWindowExceeded,
    Other(String),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub cache_write_tokens: u64,
}

impl Usage {
    pub fn add(&mut self, other: &Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_read_tokens += other.cache_read_tokens;
        self.cache_write_tokens += other.cache_write_tokens;
    }

    /// The tokens used since `before` was taken.
    pub fn since(&self, before: &Usage) -> Usage {
        Usage {
            input_tokens: self.input_tokens.saturating_sub(before.input_tokens),
            output_tokens: self.output_tokens.saturating_sub(before.output_tokens),
            cache_read_tokens: self
                .cache_read_tokens
                .saturating_sub(before.cache_read_tokens),
            cache_write_tokens: self
                .cache_write_tokens
                .saturating_sub(before.cache_write_tokens),
        }
    }

    /// Total prompt-side tokens, including cached ones.
    pub fn prompt_tokens(&self) -> u64 {
        self.input_tokens + self.cache_read_tokens + self.cache_write_tokens
    }
}

/// Why a turn ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TerminationReason {
    /// The model stopped calling tools.
    Done,
    /// `agent.max_steps` model calls were made without the model finishing.
    StepLimit,
    /// The request no longer fits in the model's context window.
    ContextOverflow { message: String },
    /// A hook asked to stop (for example stuck detection).
    HookStop { source: String, reason: String },
    /// A hook denied the user prompt.
    PromptDenied { reason: String },
    /// The user cancelled the turn.
    Cancelled,
    /// An unrecoverable error (provider failure after retries, ...).
    Error { message: String },
}

impl TerminationReason {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::StepLimit => "step_limit",
            Self::ContextOverflow { .. } => "context_overflow",
            Self::HookStop { .. } => "hook_stop",
            Self::PromptDenied { .. } => "prompt_denied",
            Self::Cancelled => "cancelled",
            Self::Error { .. } => "error",
        }
    }

    pub fn is_success(&self) -> bool {
        matches!(self, Self::Done)
    }
}
