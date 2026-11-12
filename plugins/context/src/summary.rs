//! Summaries: what a summary record holds, the block it adds to the first message, the
//! request that writes it, and the parts the plugin copies word for word.

use std::ops::Range;

use ch_kernel::{
    ContentBlock, EventId, History, Message, ModelRequest, ModelResponse, Role, StopReason,
};
use serde::{Deserialize, Serialize};

use crate::log::is_prompt;

/// `prompts/summary.md`, with `{max_words}` and `{instructions}` to fill in.
pub const PROMPT: &str = include_str!("../../../prompts/summary.md");

/// Purpose of the summary calls, in their `ModelRequest` and `Usage` events.
pub const PURPOSE: &str = "context.summarize";

/// The tags a summary block uses; their closing tags are escaped in text put inside.
const TAGS: [&str; 3] = ["context-summary", "user-messages", "latest-call"];

/// One summary: what the model wrote for a span of the history, and what the plugin
/// copied from it word for word.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    /// Its number, shown in its block.
    pub n: usize,
    /// The span it replaced: history events `from` through `to`. It starts at an
    /// assistant message right after the previous span (or the first message) and ends
    /// just before an assistant message, so roles still alternate.
    pub from: EventId,
    pub to: EventId,
    /// The span as the model sees it, e.g. "turn 1, steps 41–163".
    pub covers: String,
    /// What the model wrote.
    pub text: String,
    /// The user's text in the span, each with where it was: "[turn 1, step 97] …".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub users: Vec<String>,
    /// The latest call to each superseded tool, if it was in the span.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub calls: Vec<LatestCall>,
    /// The model that wrote it.
    pub model: String,
    /// For a merge: how many of the oldest summaries it replaces. Its span is theirs.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub merges: usize,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// A superseded tool's latest call, copied into a summary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LatestCall {
    pub tool: String,
    /// Its input as JSON, as the model sent it.
    pub input: String,
}

impl Summary {
    /// The text block it adds to the first message.
    pub fn block(&self) -> String {
        let mut out = format!(
            "<context-summary n=\"{}\" covers=\"{}\">\n{}\n</context-summary>",
            self.n,
            self.covers,
            escape(self.text.trim())
        );
        if !self.users.is_empty() {
            out.push_str(&format!("\n<user-messages n=\"{}\">", self.n));
            for user in &self.users {
                out.push('\n');
                out.push_str(&escape(user));
            }
            out.push_str("\n</user-messages>");
        }
        for call in &self.calls {
            out.push_str(&format!(
                "\n<latest-call tool=\"{}\">{}</latest-call>",
                call.tool,
                escape(&call.input)
            ));
        }
        out
    }

    /// The summary that replaces `older` and `newer`, the two oldest, with `text`.
    pub fn merged(older: &Self, newer: &Self, text: String, model: String) -> Self {
        let mut calls = older.calls.clone();
        for call in &newer.calls {
            calls.retain(|c| c.tool != call.tool);
            calls.push(call.clone());
        }
        Self {
            n: older.n,
            from: older.from,
            to: newer.to,
            covers: format!("{}; {}", older.covers, newer.covers),
            text,
            users: older.users.iter().chain(&newer.users).cloned().collect(),
            calls,
            model,
            merges: 2,
        }
    }
}

/// `text` with every closing tag of a summary block made inert.
fn escape(text: &str) -> String {
    let mut out = text.to_string();
    for tag in TAGS {
        out = out.replace(&format!("</{tag}>"), &format!("<\\/{tag}>"));
    }
    out
}

/// The prompt, with the word limit and the user's instructions.
pub fn prompt(max_words: usize, instructions: Option<&str>) -> String {
    let instructions = instructions.map(str::trim).unwrap_or("");
    PROMPT
        .trim()
        .replace("{max_words}", &max_words.to_string())
        .replace("{instructions}", instructions)
        .trim_end()
        .to_string()
}

/// Where each history event is: its turn, and how many of the turn's model replies
/// came before it or are it (0 for the turn's prompt).
fn steps(history: &History) -> Vec<(u32, usize)> {
    let mut out = Vec::new();
    let (mut turn, mut step) = (0, 0);
    for event in history.events() {
        if event.turn != turn {
            (turn, step) = (event.turn, 0);
        }
        if event.message.role == Role::Assistant {
            step += 1;
        }
        out.push((turn, step));
    }
    out
}

/// What the plugin takes from `span` (history indices) word for word, and how it names
/// the span.
pub struct Facts {
    pub covers: String,
    pub users: Vec<String>,
    pub calls: Vec<LatestCall>,
}

pub fn facts(history: &History, span: Range<usize>, superseded: &[String]) -> Facts {
    let events = history.events();
    let at = steps(history);
    let (first, last) = (at[span.start], at[span.end - 1]);
    let covers = if first.0 == last.0 && first.1 == last.1 {
        format!("turn {}, step {}", first.0, first.1)
    } else if first.0 == last.0 {
        format!("turn {}, steps {}–{}", first.0, first.1, last.1)
    } else {
        format!(
            "turn {} step {} to turn {} step {}",
            first.0, first.1, last.0, last.1
        )
    };
    let mut users = Vec::new();
    for i in span.clone() {
        if events[i].message.role != Role::User {
            continue;
        }
        for (b, block) in events[i].message.content.iter().enumerate() {
            if let ContentBlock::Text { text } = block
                && (is_prompt(history, i, b) || !text.starts_with("<system-reminder>"))
            {
                users.push(format!("[turn {}, step {}] {text}", at[i].0, at[i].1));
            }
        }
    }
    let mut calls = Vec::new();
    for tool in superseded {
        let latest = events
            .iter()
            .enumerate()
            .rev()
            .flat_map(|(i, e)| e.message.content.iter().rev().map(move |b| (i, b)))
            .find_map(|(i, b)| match b {
                ContentBlock::ToolUse { name, input, .. } if name == tool => Some((i, input)),
                _ => None,
            });
        if let Some((i, input)) = latest
            && span.contains(&i)
        {
            calls.push(LatestCall {
                tool: tool.clone(),
                input: input.to_string(),
            });
        }
    }
    Facts {
        covers,
        users,
        calls,
    }
}

/// The agent's own request, replayed: the current view, with the prompt as the last
/// text block, so the history is read from the prompt cache.
pub fn replay(agent: &ModelRequest, view: &[Message], prompt: String) -> ModelRequest {
    let mut messages = view.to_vec();
    match messages.last_mut() {
        Some(last) if last.role == Role::User => last.content.push(ContentBlock::text(prompt)),
        _ => messages.push(Message::user(prompt)),
    }
    ModelRequest {
        messages,
        purpose: PURPOSE.into(),
        ..agent.clone()
    }
}

/// A request for a separate summarizer model: the first message (the task and earlier
/// summaries) and the span, as a plain-text transcript, then the prompt.
pub fn transcript(
    model: &str,
    agent: &ModelRequest,
    first: &Message,
    span: &[Message],
    prompt: String,
) -> ModelRequest {
    let mut text = String::from("The conversation so far, as a transcript:\n\n<task>\n");
    text.push_str(&plain(first));
    text.push_str("\n</task>\n");
    for message in span {
        text.push('\n');
        text.push_str(&plain(message));
    }
    text.push_str("\n\n");
    text.push_str(&prompt);
    ModelRequest {
        model: model.to_string(),
        system: Vec::new(),
        messages: vec![Message::user(text)],
        tools: Vec::new(),
        max_tokens: agent.max_tokens,
        temperature: agent.temperature,
        thinking_budget: None,
        purpose: PURPOSE.into(),
    }
}

/// A request that merges two summaries into one, with the same prompt.
pub fn merge(
    model: &str,
    agent: &ModelRequest,
    older: &Summary,
    newer: &Summary,
    max_words: usize,
) -> ModelRequest {
    let text = format!(
        "{}\n\n{}\n\n{}",
        older.block(),
        newer.block(),
        prompt(
            max_words,
            Some(
                "These are two summaries of consecutive parts of the conversation. Merge \
                 them into one summary of both, keeping what is still needed."
            )
        )
    );
    ModelRequest {
        model: model.to_string(),
        system: Vec::new(),
        messages: vec![Message::user(text)],
        tools: Vec::new(),
        max_tokens: agent.max_tokens,
        temperature: agent.temperature,
        thinking_budget: None,
        purpose: PURPOSE.into(),
    }
}

/// A message as transcript lines.
fn plain(message: &Message) -> String {
    let who = match message.role {
        Role::User => "[user]",
        Role::Assistant => "[assistant]",
    };
    let lines: Vec<String> = message
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(format!("{who} {text}")),
            ContentBlock::ToolUse { name, input, .. } => {
                Some(format!("[assistant calls {name}] {input}"))
            }
            ContentBlock::ToolResult { content, .. } => Some(format!("[tool result] {content}")),
            ContentBlock::Thinking { .. } | ContentBlock::RedactedThinking { .. } => None,
        })
        .collect();
    lines.join("\n")
}

/// The summary in a reply: its text, without tool calls or thinking. `None` when the
/// model refused or wrote nothing.
pub fn reply_text(response: &ModelResponse) -> Option<String> {
    if response.stop_reason == StopReason::Refusal {
        return None;
    }
    let text: Vec<&str> = response
        .message
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    let text = text.join("\n");
    (!text.trim().is_empty()).then(|| text.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary() -> Summary {
        Summary {
            n: 2,
            from: 5,
            to: 40,
            covers: "turn 1, steps 3–20".into(),
            text: "## Goal\nFix it. </context-summary> is not the end.".into(),
            users: vec!["[turn 1, step 4] stop at </user-messages>, please".into()],
            calls: vec![LatestCall {
                tool: "update_plan".into(),
                input: "{\"plan\":[]}".into(),
            }],
            model: "m".into(),
            merges: 0,
        }
    }

    #[test]
    fn a_block_has_the_summary_then_the_copied_parts_with_closing_tags_escaped() {
        assert_eq!(
            summary().block(),
            "<context-summary n=\"2\" covers=\"turn 1, steps 3–20\">\n## Goal\nFix it. \
             <\\/context-summary> is not the end.\n</context-summary>\n<user-messages n=\"2\">\n\
             [turn 1, step 4] stop at <\\/user-messages>, please\n</user-messages>\n\
             <latest-call tool=\"update_plan\">{\"plan\":[]}</latest-call>"
        );
    }

    #[test]
    fn the_prompt_has_the_limit_and_the_instructions() {
        let p = prompt(800, Some("focus on the parser work"));
        assert!(p.contains("at most 800 words"), "{p}");
        assert!(p.ends_with("focus on the parser work"), "{p}");
        assert!(!prompt(800, None).contains('{'));
    }

    #[test]
    fn a_merge_keeps_both_parts_copied_and_the_newer_latest_call() {
        let older = summary();
        let mut newer = summary();
        newer.n = 3;
        newer.from = 41;
        newer.to = 60;
        newer.users = vec!["[turn 2, step 0] next".into()];
        newer.calls[0].input = "{\"plan\":[1]}".into();
        let m = Summary::merged(&older, &newer, "both".into(), "m".into());
        assert_eq!((m.n, m.from, m.to, m.merges), (2, 5, 60, 2));
        assert_eq!(m.users.len(), 2);
        assert_eq!(m.calls.len(), 1);
        assert_eq!(m.calls[0].input, "{\"plan\":[1]}");
    }
}
