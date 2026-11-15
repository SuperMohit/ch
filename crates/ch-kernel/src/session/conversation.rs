//! [`Conversation`]: what `/clear` and `/resume` replace.

use std::collections::HashSet;

use chrono::{DateTime, Utc};

use crate::history::History;
use crate::ledger::ToolLedger;
use crate::session_id::SessionId;
use crate::store::{SavedPrompt, SessionSnapshot};
use crate::types::Usage;

/// One conversation: its id, history and what it has used and been sent. A
/// [`Session`](super::Session) holds one at a time; `/clear` replaces it with a new
/// one, `/resume` with a saved one, and the session itself (the process's registry,
/// provider, tools and totals) carries on. Keeping it in one value means a new
/// conversation can't keep a field of the old one by mistake.
#[derive(Debug)]
pub(crate) struct Conversation {
    /// The name it is saved under. Plugins see it as `Cx::session_id` and compare it to
    /// notice a new conversation.
    pub(crate) id: SessionId,
    pub(crate) created_at: DateTime<Utc>,
    /// Turns run in it, saved with it.
    pub(crate) turn: u32,
    pub(crate) history: History,
    /// What it has used: saved with it, zero for a new one.
    pub(crate) spend: Spend,
    /// Every tool definition it was sent (ADR 0006).
    pub(crate) ledger: ToolLedger,
    /// The system prompt of its first request, sent unchanged after.
    pub(crate) system: Option<Vec<String>>,
    /// Tool names already warned about in it (changed definitions, waiting, shadowed).
    pub(crate) tool_warnings: HashSet<String>,
}

impl Conversation {
    /// An empty conversation named `id`.
    pub(crate) fn new(id: SessionId) -> Self {
        Self {
            id,
            created_at: Utc::now(),
            turn: 0,
            history: History::new(),
            spend: Spend::default(),
            ledger: ToolLedger::default(),
            system: None,
            tool_warnings: HashSet::new(),
        }
    }

    /// A saved conversation, and the prompt it was sent, which the session applies
    /// against the tools it offers now.
    pub(crate) fn restored(snap: SessionSnapshot) -> (Self, Option<SavedPrompt>) {
        let conversation = Self {
            id: snap.meta.id,
            created_at: snap.meta.created_at,
            turn: snap.meta.turns,
            history: snap.history,
            spend: Spend {
                usage: snap.usage,
                cost: snap.cost,
            },
            ledger: ToolLedger::default(),
            system: None,
            tool_warnings: HashSet::new(),
        };
        (conversation, snap.prompt)
    }
}

/// Tokens and cost added up over model calls.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Spend {
    pub(crate) usage: Usage,
    pub(crate) cost: f64,
}

impl Spend {
    pub(crate) fn add(&mut self, usage: &Usage, cost: Option<f64>) {
        self.usage.add(usage);
        self.cost += cost.unwrap_or(0.0);
    }
}
