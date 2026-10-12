//! The full conversation log. Context strategies read it and produce a *view*; the log
//! itself is never rewritten except by explicit user actions (`/clear`).

use serde::{Deserialize, Serialize};

use crate::tokens::estimate_tokens;
use crate::types::{Message, Role};

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

    pub fn get(&self, id: EventId) -> Option<&Event> {
        self.events
            .binary_search_by_key(&id, |e| e.id)
            .ok()
            .map(|i| &self.events[i])
    }

    pub fn tokens_est(&self) -> u64 {
        self.events.iter().map(|e| e.tokens_est).sum()
    }

    /// Text of the first user message (the original task), if any.
    pub fn first_user_text(&self) -> Option<String> {
        self.events
            .iter()
            .find(|e| e.message.role == Role::User)
            .map(|e| e.message.text())
    }

    pub fn clear(&mut self) {
        self.events.clear();
    }

    /// Removes events from the end until the last remaining one has an id < `id`.
    pub fn truncate_from(&mut self, id: EventId) {
        self.events.retain(|e| e.id < id);
    }
}
