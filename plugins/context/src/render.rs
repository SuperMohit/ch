//! The view: the history with the compaction log applied. It uses no clock, randomness
//! or estimate, so the same history and log always give the same bytes.

use ch_kernel::{ContentBlock, History, Message, Role};

use crate::log::{Record, Replacement, find};

/// `history` with each record's replacements in place, then without the thinking
/// blocks the records invalidated. Messages are never removed, so the view's indices
/// are the history's.
pub fn render(history: &History, records: &[Record]) -> Vec<Message> {
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
    if records.is_empty() {
        return messages;
    }
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
    messages
}

pub fn is_thinking(block: &ContentBlock) -> bool {
    matches!(
        block,
        ContentBlock::Thinking { .. } | ContentBlock::RedactedThinking { .. }
    )
}
