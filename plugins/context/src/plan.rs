//! What a trim shortens: the regions of the view, then candidates in priority order
//! until the projected view is small enough.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::PathBuf;

use ch_kernel::{Budget, ContentBlock, History, Message, Role, estimate_tokens};

use crate::config::ContextConfig;
use crate::log::{Edit, EditKind, Pos, Replacement, hash};
use crate::stub::{self, Hint, Saved};

/// How far a trim goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    /// The view passed `soft`: the middle, until the view is down to `trim_to`.
    Soft,
    /// `/compact`: every candidate in the middle, with the recent window cut to
    /// `min_recent_steps`.
    Manual,
    /// An overflow: every candidate in the middle, and tool results in the recent window
    /// except in the newest step, since the estimate was wrong.
    Overflow,
}

/// Where trimmed results can be saved, and how stubs tell the model to read them.
pub struct Files {
    /// The session directory, anchored as the file tools check it.
    pub base: Option<PathBuf>,
    pub hint: Hint,
}

impl Files {
    fn copy_path(&self, (event, block): Pos) -> Option<PathBuf> {
        let name = format!("{event}-{block}.txt");
        Some(self.base.as_ref()?.join("context/elided").join(name))
    }
}

/// A result to save before its stub is shown.
#[derive(Debug, Clone)]
pub struct Original {
    pub pos: Pos,
    pub path: PathBuf,
    pub content: String,
}

#[derive(Debug, Default)]
pub struct Plan {
    /// Sorted by position.
    pub edits: Vec<Edit>,
    pub originals: Vec<Original>,
}

/// Plans a trim of `view`, the history with the earlier records applied. Blocks in
/// `taken` were trimmed before and are left alone.
pub fn plan(
    history: &History,
    view: &[Message],
    taken: &HashSet<Pos>,
    budget: &Budget,
    config: &ContextConfig,
    reach: Reach,
    files: &Files,
) -> Plan {
    let mut plan = Plan::default();
    let Some(regions) = regions(view, budget, config, reach) else {
        return plan;
    };
    let events = history.events();
    let mut chars: Vec<usize> = view.iter().map(Message::char_len).collect();
    let mut total: u64 = chars.iter().map(|&c| estimate_tokens(c)).sum();
    let target = config.trim_to * budget.usable() as f64;
    let mut chosen = HashSet::new();
    for (i, b, kind) in candidates(history, &regions, config) {
        if reach == Reach::Soft && budget.calibrator.calibrate(total) as f64 <= target {
            break;
        }
        let pos = (events[i].id, b);
        if taken.contains(&pos) || !chosen.insert(pos) {
            continue;
        }
        let block = &events[i].message.content[b];
        let replacement = match (kind, block) {
            (EditKind::Superseded, ContentBlock::ToolUse { name, .. }) => {
                Replacement::Input(stub::superseded(name))
            }
            (EditKind::Input, ContentBlock::ToolUse { input, .. }) => {
                match stub::shortened_input(input, config.input_min_chars) {
                    Some(short) => Replacement::Input(short),
                    None => continue,
                }
            }
            (EditKind::Result, ContentBlock::ToolResult { content, .. }) => {
                let spill = files
                    .base
                    .as_ref()
                    .and_then(|base| stub::spill_file(content, &base.join("spill")));
                let copy = files.copy_path(pos);
                let saved = match (&spill, &copy) {
                    (Some(path), _) => Saved::Spill(path),
                    (None, Some(path)) => Saved::Copy(path),
                    (None, None) => Saved::Nowhere,
                };
                let text = stub::result(content, saved, files.hint);
                // A result just over the limit can be shorter than its stub.
                if text.len() >= content.len() {
                    continue;
                }
                if let (None, Some(path)) = (spill, copy) {
                    plan.originals.push(Original {
                        pos,
                        path,
                        content: content.clone(),
                    });
                }
                Replacement::Text(text)
            }
            (EditKind::Reminder, ContentBlock::Text { .. }) => {
                Replacement::Text(stub::REMINDER.into())
            }
            _ => continue,
        };
        let old = block.char_len();
        let new = match (block, &replacement) {
            (ContentBlock::ToolUse { name, .. }, Replacement::Input(input)) => {
                name.len() + input.to_string().len()
            }
            (_, Replacement::Text(text)) => text.len(),
            _ => old,
        };
        total -= estimate_tokens(chars[i]);
        chars[i] = chars[i].saturating_sub(old) + new;
        total += estimate_tokens(chars[i]);
        plan.edits.push(Edit {
            event: pos.0,
            block: pos.1,
            kind,
            hash: hash(block),
            replacement,
        });
    }
    plan.edits.sort_by_key(Edit::pos);
    plan
}

/// The message ranges a trim may edit.
struct Regions {
    /// Between the first message and the recent window.
    middle: Range<usize>,
    /// Where tool results may also be trimmed (on overflow).
    results_too: Range<usize>,
}

/// The preamble is the first message. The recent window is the newest steps (an
/// assistant message and the user message that answers it) holding at least `recent` of
/// the usable window and at least `min_recent_steps` steps; it starts at an assistant
/// message. The middle is what's in between.
fn regions(
    view: &[Message],
    budget: &Budget,
    config: &ContextConfig,
    reach: Reach,
) -> Option<Regions> {
    if view.first()?.role != Role::User {
        return None;
    }
    let steps: Vec<usize> = (1..view.len())
        .filter(|&i| view[i].role == Role::Assistant)
        .collect();
    let newest = *steps.last()?;
    let minimum = config.recent * budget.usable() as f64;
    let mut start = newest;
    let mut raw = 0;
    let mut end = view.len();
    for (taken, &step) in steps.iter().rev().enumerate() {
        start = step;
        raw += view[step..end]
            .iter()
            .map(|m| estimate_tokens(m.char_len()))
            .sum::<u64>();
        end = step;
        let enough_steps = taken + 1 >= config.min_recent_steps;
        let enough_tokens =
            reach == Reach::Manual || budget.calibrator.calibrate(raw) as f64 >= minimum;
        if enough_steps && enough_tokens {
            break;
        }
    }
    let results_too = if reach == Reach::Overflow {
        start..newest
    } else {
        start..start
    };
    Some(Regions {
        middle: 1..start,
        results_too,
    })
}

/// Every block a trim could shorten, in priority order: superseded calls, long tool
/// results, long tool inputs, then reminders, each oldest first.
fn candidates(
    history: &History,
    regions: &Regions,
    config: &ContextConfig,
) -> Vec<(usize, usize, EditKind)> {
    let events = history.events();
    let blocks = |range: Range<usize>, role: Role| {
        range
            .filter(move |&i| events[i].message.role == role)
            .flat_map(move |i| (0..events[i].message.content.len()).map(move |b| (i, b)))
            .map(move |(i, b)| (i, b, &events[i].message.content[b]))
    };

    // Each superseded tool's latest call, anywhere in the history.
    let mut latest: HashMap<&str, Pos> = HashMap::new();
    for event in events {
        for (b, block) in event.message.content.iter().enumerate() {
            if let ContentBlock::ToolUse { name, .. } = block
                && config.superseded_tools.iter().any(|t| t == name)
            {
                latest.insert(name, (event.id, b));
            }
        }
    }
    let mut out = Vec::new();
    for (i, b, block) in blocks(regions.middle.clone(), Role::Assistant) {
        if let ContentBlock::ToolUse { name, .. } = block
            && latest
                .get(name.as_str())
                .is_some_and(|&pos| pos != (events[i].id, b))
        {
            out.push((i, b, EditKind::Superseded));
        }
    }
    let results = blocks(regions.middle.clone(), Role::User)
        .chain(blocks(regions.results_too.clone(), Role::User));
    for (i, b, block) in results {
        if let ContentBlock::ToolResult { content, .. } = block
            && content.chars().count() > config.elide_min_chars
        {
            out.push((i, b, EditKind::Result));
        }
    }
    for (i, b, block) in blocks(regions.middle.clone(), Role::Assistant) {
        if matches!(block, ContentBlock::ToolUse { .. }) {
            out.push((i, b, EditKind::Input));
        }
    }
    for (i, b, block) in blocks(regions.middle.clone(), Role::User) {
        if let ContentBlock::Text { text } = block
            && text.starts_with("<system-reminder>")
            && !is_prompt(history, i, b)
        {
            out.push((i, b, EditKind::Reminder));
        }
    }
    out
}

/// Whether block `b` of event `i` is a turn's prompt: the first block of the turn's first
/// event. Notes and reminders come after it, or in later events.
fn is_prompt(history: &History, i: usize, b: usize) -> bool {
    let events = history.events();
    b == 0 && (i == 0 || events[i - 1].turn != events[i].turn)
}
