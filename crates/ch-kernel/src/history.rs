//! The full conversation log. Context strategies read it and produce a *view*; the log
//! itself is never rewritten except by explicit user actions (`/clear`).

use serde::{Deserialize, Serialize};

use crate::tokens::estimate_tokens;
use crate::types::{ContentBlock, Message, Role};

/// Stable, monotonically increasing id of a history event.
pub type EventId = u64;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub id: EventId,
    /// User turn this event belongs to (1-based).
    pub turn: u32,
    pub message: Message,
    /// Raw estimate (chars ÷ 3.5) of this message's prompt tokens.
    pub tokens_est: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct History {
    events: Vec<Event>,
    next_id: EventId,
}

impl History {
    pub fn new() -> Self {
        Self::default()
    }

    /// A history continuing from saved events, keeping their ids.
    pub fn from_events(events: Vec<Event>) -> Self {
        let next_id = events.last().map_or(0, |e| e.id + 1);
        Self { events, next_id }
    }

    pub fn push(&mut self, turn: u32, message: Message) -> EventId {
        let id = self.next_id;
        self.next_id += 1;
        let tokens_est = estimate_tokens(message.char_len());
        self.events.push(Event {
            id,
            turn,
            message,
            tokens_est,
        });
        id
    }

    /// Adds `blocks` to the newest message if it is a user message, or as a new user
    /// message otherwise. The session uses it for a model call's reminders and hook
    /// notes: the newest user message hasn't been sent yet, so this appends to the
    /// conversation the model has seen instead of rewriting it.
    pub fn append_to_last_user(&mut self, turn: u32, blocks: Vec<ContentBlock>) {
        match self.events.last_mut() {
            Some(last) if last.message.role == Role::User => {
                last.message.content.extend(blocks);
                last.tokens_est = estimate_tokens(last.message.char_len());
            }
            _ => {
                self.push(
                    turn,
                    Message {
                        role: Role::User,
                        content: blocks,
                    },
                );
            }
        }
    }

    pub fn events(&self) -> &[Event] {
        &self.events
    }

    pub fn messages(&self) -> Vec<Message> {
        self.events.iter().map(|e| e.message.clone()).collect()
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    pub fn last(&self) -> Option<&Event> {
        self.events.last()
    }

    /// The first text block of the first user message (the original task), if any.
    /// Later blocks of that message are reminders and notes.
    pub fn first_user_text(&self) -> Option<String> {
        let first = self.events.iter().find(|e| e.message.role == Role::User)?;
        first.message.content.iter().find_map(|b| match b {
            ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
    }

    pub fn clear(&mut self) {
        self.events.clear();
    }
}
