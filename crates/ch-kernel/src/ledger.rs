//! The tools a conversation has been told about (ADR 0006).
//!
//! A model's signed reasoning is checked against the tool list it was written under, so
//! once a tool definition is sent it stays in every later request of the conversation:
//! entries are only ever appended. A tool added after the first request is *deferred*
//! (providers that can send it with `defer_loading` and announce it with a
//! `ToolsChanged` block); a tool withdrawn later stays as an inactive entry.

use serde::{Deserialize, Serialize};

use crate::model::ToolChanges;
use crate::tool::ToolSpec;
use crate::types::ContentBlock;

/// Most entries a conversation's ledger holds. Tools past it wait for `/clear`.
pub const MAX_LEDGER: usize = 512;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LedgerEntry {
    /// The definition as first sent. Never changed for the conversation.
    pub spec: ToolSpec,
    /// The tool source that offered it; `None` for a built-in tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Added after the conversation's first request.
    #[serde(default)]
    pub deferred: bool,
    /// Whether the model was last told the tool is available.
    pub active: bool,
}

/// A tool currently offered by a built-in plugin or a tool source, in precedence order.
#[derive(Debug, Clone, PartialEq)]
pub struct Offered {
    pub spec: ToolSpec,
    pub source: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolLedger {
    entries: Vec<LedgerEntry>,
    /// Whether the conversation's first request was sent: until then the ledger is
    /// empty and the first request takes every offered tool as it is.
    #[serde(default)]
    started: bool,
}

/// The ledger a model call would send, and what it changes. Committed only once the
/// call's additions are stored in history.
#[derive(Debug, Clone, PartialEq)]
pub struct Staged {
    pub ledger: ToolLedger,
    pub added: Vec<String>,
    pub removed: Vec<String>,
    /// Tools offered now under a name whose first-sent definition differs. The first
    /// definition stays for the conversation.
    pub changed: Vec<String>,
    /// Offered tools that are not announced: the model can't take new tools in this
    /// conversation, or the ledger is full.
    pub waiting: Vec<String>,
}

impl Staged {
    /// The block announcing this call's changes, if there are any.
    pub fn block(&self) -> Option<ContentBlock> {
        (!self.added.is_empty() || !self.removed.is_empty()).then(|| ContentBlock::ToolsChanged {
            added: self.added.clone(),
            removed: self.removed.clone(),
        })
    }
}

impl ToolLedger {
    /// A ledger for a conversation saved before ledgers existed: its requests sent
    /// `offered`, so they count as the first request's tools.
    pub fn sent(offered: &[Offered]) -> Self {
        Self {
            entries: offered.iter().map(|o| entry(o, false)).collect(),
            started: true,
        }
    }

    pub fn entries(&self) -> &[LedgerEntry] {
        &self.entries
    }

    pub fn is_started(&self) -> bool {
        self.started
    }

    pub fn get(&self, name: &str) -> Option<&LedgerEntry> {
        self.entries.iter().find(|e| e.spec.name == name)
    }

    /// The active tools, in ledger order.
    pub fn active_specs(&self) -> Vec<ToolSpec> {
        (self.entries.iter())
            .filter(|e| e.active)
            .map(|e| e.spec.clone())
            .collect()
    }

    /// The tools sent with every request in full: the first request's, withdrawn or
    /// not. What a request sends when tool changes can't be announced.
    pub fn sent_specs(&self) -> Vec<ToolSpec> {
        (self.entries.iter())
            .filter(|e| !e.deferred)
            .map(|e| e.spec.clone())
            .collect()
    }

    /// Whether tools were added after the first request.
    pub fn has_deferred(&self) -> bool {
        self.entries.iter().any(|e| e.deferred)
    }

    /// The ledger the next model call sends when `offered` are the tools available
    /// now, for a provider that handles changes as `mode` says:
    /// - `Native`: changes are announced; definitions never change once sent.
    /// - `Replace`: changes are announced, and a changed definition replaces the old one
    ///   (the model doesn't check its reasoning against them).
    /// - `Frozen`: after the first request nothing is added or withdrawn: additions
    ///   wait, and a withdrawn tool keeps its entry (calling it fails).
    pub fn stage(&self, offered: &[Offered], mode: ToolChanges) -> Staged {
        let apply = mode != ToolChanges::Frozen;
        let mut staged = Staged {
            ledger: self.clone(),
            added: Vec::new(),
            removed: Vec::new(),
            changed: Vec::new(),
            waiting: Vec::new(),
        };
        if !self.started {
            staged.ledger = Self {
                entries: (offered.iter())
                    .take(MAX_LEDGER)
                    .map(|o| entry(o, false))
                    .collect(),
                started: true,
            };
            staged.waiting = (offered.iter().skip(MAX_LEDGER))
                .map(|o| o.spec.name.clone())
                .collect();
            return staged;
        }
        let ledger = &mut staged.ledger;
        for o in offered {
            let name = &o.spec.name;
            let full = ledger.entries.len() >= MAX_LEDGER;
            match ledger.entries.iter_mut().find(|e| e.spec.name == *name) {
                Some(e) => {
                    if e.spec != o.spec {
                        if mode == ToolChanges::Replace {
                            e.spec = o.spec.clone();
                        } else {
                            staged.changed.push(name.clone());
                        }
                    }
                    if !e.active {
                        if apply {
                            e.active = true;
                            staged.added.push(name.clone());
                        } else {
                            staged.waiting.push(name.clone());
                        }
                    }
                }
                None if apply && !full => {
                    ledger.entries.push(entry(o, true));
                    staged.added.push(name.clone());
                }
                None => staged.waiting.push(name.clone()),
            }
        }
        if apply {
            for e in ledger.entries.iter_mut().filter(|e| e.active) {
                if !offered.iter().any(|o| o.spec.name == e.spec.name) {
                    e.active = false;
                    staged.removed.push(e.spec.name.clone());
                }
            }
        }
        staged
    }
}

fn entry(o: &Offered, deferred: bool) -> LedgerEntry {
    LedgerEntry {
        spec: o.spec.clone(),
        source: o.source.clone(),
        deferred,
        active: true,
    }
}

/// The `ToolsChanged` blocks in `blocks`, combined into one: for each tool, what its
/// first and last change agree on (an addition then a removal is no change). Names keep
/// the order they first appear in. `None` when nothing is left.
pub fn net<'a>(blocks: impl IntoIterator<Item = &'a ContentBlock>) -> Option<ContentBlock> {
    // name → (first change was an addition, last change was an addition)
    let mut changes: Vec<(&str, bool, bool)> = Vec::new();
    for block in blocks {
        let ContentBlock::ToolsChanged { added, removed } = block else {
            continue;
        };
        let ops = (added.iter().map(|n| (n, true))).chain(removed.iter().map(|n| (n, false)));
        for (name, add) in ops {
            match changes.iter_mut().find(|c| c.0 == name.as_str()) {
                Some(c) => c.2 = add,
                None => changes.push((name.as_str(), add, add)),
            }
        }
    }
    let (mut added, mut removed) = (Vec::new(), Vec::new());
    for (name, first, last) in changes {
        if first == last {
            if last { &mut added } else { &mut removed }.push(name.to_string());
        }
    }
    (!added.is_empty() || !removed.is_empty())
        .then_some(ContentBlock::ToolsChanged { added, removed })
}

/// How a `ToolsChanged` block reads as text, for providers and transcripts that can't
/// render it natively.
pub fn note(added: &[String], removed: &[String]) -> String {
    let mut parts = Vec::new();
    if !added.is_empty() {
        parts.push(format!("Tools now available: {}.", added.join(", ")));
    }
    if !removed.is_empty() {
        parts.push(format!(
            "Tools no longer available (don't call them): {}.",
            removed.join(", ")
        ));
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const NATIVE: ToolChanges = ToolChanges::Native;
    const FROZEN: ToolChanges = ToolChanges::Frozen;

    fn spec(name: &str, desc: &str) -> ToolSpec {
        ToolSpec {
            name: name.into(),
            description: desc.into(),
            input_schema: json!({"type": "object"}),
            read_only: false,
        }
    }

    fn offered(names: &[&str]) -> Vec<Offered> {
        names
            .iter()
            .map(|n| Offered {
                spec: spec(n, "d"),
                source: n.starts_with("mcp__").then(|| "src".to_string()),
            })
            .collect()
    }

    fn names(l: &ToolLedger) -> Vec<(String, bool, bool)> {
        (l.entries.iter())
            .map(|e| (e.spec.name.clone(), e.deferred, e.active))
            .collect()
    }

    #[test]
    fn the_first_request_takes_every_tool_and_later_ones_only_append() {
        let first = ToolLedger::default().stage(&offered(&["a", "b"]), NATIVE);
        assert!(first.block().is_none());
        let l = first.ledger;
        assert!(l.is_started());

        let s = l.stage(&offered(&["a", "mcp__x"]), NATIVE);
        assert_eq!(s.added, ["mcp__x"]);
        assert_eq!(s.removed, ["b"]);
        assert_eq!(
            names(&s.ledger),
            [
                ("a".into(), false, true),
                ("b".into(), false, false),
                ("mcp__x".into(), true, true)
            ]
        );

        // Back again: the entry is reused, not appended.
        let s = s.ledger.stage(&offered(&["a", "b", "mcp__x"]), NATIVE);
        assert_eq!(s.added, ["b"]);
        assert!(s.removed.is_empty());
        assert_eq!(s.ledger.entries.len(), 3);
    }

    #[test]
    fn a_changed_definition_keeps_the_first_one() {
        let l = ToolLedger::default().stage(&offered(&["a"]), NATIVE).ledger;
        let mut now = offered(&["a"]);
        now[0].spec.description = "new".into();
        let s = l.stage(&now, NATIVE);
        assert_eq!(s.changed, ["a"]);
        assert_eq!(s.ledger.entries[0].spec.description, "d");
        assert!(s.block().is_none());
    }

    #[test]
    fn replace_mode_takes_the_new_definition() {
        let l = ToolLedger::default()
            .stage(&offered(&["a"]), ToolChanges::Replace)
            .ledger;
        let mut now = offered(&["a"]);
        now[0].spec.description = "new".into();
        let s = l.stage(&now, ToolChanges::Replace);
        assert!(s.changed.is_empty());
        assert_eq!(s.ledger.entries[0].spec.description, "new");
    }

    #[test]
    fn without_tool_changes_nothing_is_added_or_withdrawn_after_the_first_request() {
        let l = ToolLedger::default()
            .stage(&offered(&["a", "b"]), FROZEN)
            .ledger;
        assert_eq!(l.entries.len(), 2);
        let s = l.stage(&offered(&["a", "mcp__x"]), FROZEN);
        assert_eq!(s.waiting, ["mcp__x"]);
        assert!(s.block().is_none());
        assert_eq!(s.ledger, l);
    }

    #[test]
    fn a_full_ledger_makes_new_tools_wait() {
        let many: Vec<String> = (0..MAX_LEDGER + 2).map(|i| format!("t{i}")).collect();
        let all: Vec<&str> = many.iter().map(String::as_str).collect();
        let s = ToolLedger::default().stage(&offered(&all), NATIVE);
        assert_eq!(s.ledger.entries.len(), MAX_LEDGER);
        assert_eq!(s.waiting.len(), 2);
        let s = s.ledger.stage(&offered(&all), NATIVE);
        assert!(s.added.is_empty());
        assert_eq!(s.waiting.len(), 2);
    }

    #[test]
    fn netting_keeps_what_the_first_and_last_change_agree_on() {
        let block = |a: &[&str], r: &[&str]| ContentBlock::ToolsChanged {
            added: a.iter().map(|s| s.to_string()).collect(),
            removed: r.iter().map(|s| s.to_string()).collect(),
        };
        let blocks = [
            block(&["x", "y"], &["z"]),
            ContentBlock::text("other"),
            block(&["z"], &["y"]),
            block(&["w"], &[]),
        ];
        assert_eq!(net(&blocks), Some(block(&["x", "w"], &[])));
        assert_eq!(net(&[block(&["x"], &[]), block(&[], &["x"])]), None);
        assert_eq!(net(&[ContentBlock::text("t")]), None);
    }

    #[test]
    fn the_note_names_both_lists() {
        assert_eq!(
            note(&["a".into()], &["b".into(), "c".into()]),
            "Tools now available: a. Tools no longer available (don't call them): b, c."
        );
    }
}
