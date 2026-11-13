//! The view: the history with the compaction log applied. It uses no clock, randomness
//! or estimate, so the same history and log always give the same bytes.

use ch_kernel::{ContentBlock, History, Message, Role};

use crate::log::{Applied, Record, Replacement, find};

/// The history with the records applied, message for message, and where the part after
/// the summaries starts.
#[derive(Debug, Clone)]
pub struct Rendered {
    /// One per history event: the first one with the summaries appended, the others
    /// with the trims' replacements and without the thinking the records invalidated.
    pub messages: Vec<Message>,
    /// The index of the first message after the summarized span: 1 with no summary.
    pub start: usize,
}

impl Rendered {
    /// What is sent: the first message, then everything after the summarized span.
    /// The span starts at a reply and ends just before one, so roles still alternate.
    pub fn view(&self) -> Vec<Message> {
        let Some(first) = self.messages.first() else {
            return Vec::new();
        };
        let mut view = vec![first.clone()];
        view.extend(
            self.messages[self.start.min(self.messages.len())..]
                .iter()
                .cloned(),
        );
        view
    }
}

/// `history` with each record applied: the trims' replacements in place, the summaries
/// appended to the first message, and the thinking blocks the records invalidated left
/// out.
pub fn render(history: &History, records: &[Record]) -> Rendered {
    let events = history.events();
    let mut messages = history.messages();
    for edit in records.iter().flat_map(|r| &r.edits) {
        let Some(block) =
            find(history, edit.event).and_then(|i| messages[i].content.get_mut(edit.block))
        else {
            continue;
        };
        match (block, &edit.replacement) {
            (ContentBlock::ToolUse { input, .. }, Replacement::Input(new)) => *input = new.clone(),
            (ContentBlock::ToolResult { content, .. }, Replacement::Text(new))
            | (ContentBlock::Text { text: content }, Replacement::Text(new)) => {
                *content = new.clone()
            }
            _ => {}
        }
    }
    let applied = Applied::of(records);
    if !records.is_empty() {
        for (event, message) in events.iter().zip(&mut messages) {
            if message.role != Role::Assistant {
                continue;
            }
            let mut index = 0;
            message.content.retain(|block| {
                let pos = (event.id, index);
                index += 1;
                !(is_thinking(block) && records.iter().any(|r| r.drops_thinking_at(pos)))
            });
        }
    }
    let start = applied.start(history);
    // Tool changes announced inside the summarized span, combined into one: without it
    // a tool added there would stay hidden from the model, and one withdrawn there
    // would look available again.
    let span = &messages[1.min(messages.len())..start.min(messages.len())];
    let changes = ch_kernel::ledger::net(span.iter().flat_map(|m| &m.content));
    if let Some(first) = messages.first_mut() {
        first.content.extend(
            applied
                .summaries
                .iter()
                .map(|s| ContentBlock::text(s.block())),
        );
        first.content.extend(changes);
    }
    Rendered { start, messages }
}

pub fn is_thinking(block: &ContentBlock) -> bool {
    matches!(
        block,
        ContentBlock::Thinking { .. } | ContentBlock::RedactedThinking { .. }
    )
}
