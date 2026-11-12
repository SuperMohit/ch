//! What a trim shortens: the regions of the view, then candidates in priority order
//! until the projected view is small enough.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::PathBuf;

use ch_kernel::util::ReadTools;
use ch_kernel::{Budget, ContentBlock, History, Role, estimate_tokens};

use crate::config::ContextConfig;
use crate::log::{Edit, EditKind, Pos, Replacement, Trigger, fits, hash, is_prompt};
use crate::render::{Rendered, is_thinking};
use crate::stub::{self, Saved};

/// Where trimmed results can be saved, and how stubs tell the model to read them.
pub struct Files {
    /// The session directory, anchored as the file tools check it.
    pub base: Option<PathBuf>,
    pub tools: ReadTools,
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
///
/// - `Soft`: the middle, until the view is down to `trim_to`; then every other candidate
///   between the first and last edit, which costs no further cache rewrite.
/// - `Manual`: every candidate in the middle, with the recent window cut to
///   `min_recent_steps`.
/// - `Overflow`: every candidate in the middle, and tool results in the recent window
///   except in the newest step, since the estimate was wrong.
pub fn plan(
    history: &History,
    rendered: &Rendered,
    taken: &HashSet<Pos>,
    budget: &Budget,
    config: &ContextConfig,
    trigger: Trigger,
    files: &Files,
) -> Plan {
    let Some(regions) = regions(rendered, budget, config, trigger) else {
        return Plan::default();
    };
    let mut p = Planner::new(history, rendered, taken, config, files);
    let target = config.trim_to * budget.usable() as f64;
    let candidates = candidates(history, &regions, config);
    for &(i, b, kind) in &candidates {
        if trigger == Trigger::Soft && budget.calibrator.calibrate(p.total) as f64 <= target {
            break;
        }
        p.add(i, b, kind);
    }
    if let (Some(first), Some(last)) = (
        p.plan.edits.iter().map(Edit::pos).min(),
        p.plan.edits.iter().map(Edit::pos).max(),
    ) {
        for &(i, b, kind) in &candidates {
            let pos = (history.events()[i].id, b);
            if first < pos && pos < last {
                p.add(i, b, kind);
            }
        }
    }
    let mut plan = p.plan;
    plan.edits.sort_by_key(Edit::pos);
    plan
}

/// The edits chosen so far, and the size the view will have with them.
struct Planner<'a> {
    history: &'a History,
    taken: &'a HashSet<Pos>,
    config: &'a ContextConfig,
    files: &'a Files,
    /// Characters of each message of the view, with the chosen edits.
    chars: Vec<usize>,
    /// Characters of the thinking in each message of the view.
    thinking: Vec<usize>,
    /// Raw tokens of the whole view, with the chosen edits and the thinking they drop.
    total: u64,
    /// The message of the first edit: thinking in later messages is dropped.
    first: Option<usize>,
    chosen: HashSet<Pos>,
    plan: Plan,
}

impl<'a> Planner<'a> {
    fn new(
        history: &'a History,
        rendered: &Rendered,
        taken: &'a HashSet<Pos>,
        config: &'a ContextConfig,
        files: &'a Files,
    ) -> Self {
        // The summarized span isn't sent.
        let sent = |i: usize| i == 0 || i >= rendered.start;
        let chars: Vec<usize> = (rendered.messages.iter().enumerate())
            .map(|(i, m)| if sent(i) { m.char_len() } else { 0 })
            .collect();
        let thinking = (rendered.messages.iter().enumerate())
            .map(|(i, m)| {
                let thinking = m.content.iter().filter(|b| is_thinking(b));
                if sent(i) {
                    thinking.map(ContentBlock::char_len).sum()
                } else {
                    0
                }
            })
            .collect();
        let total = chars.iter().map(|&c| estimate_tokens(c)).sum();
        Self {
            history,
            taken,
            config,
            files,
            chars,
            thinking,
            total,
            first: None,
            chosen: HashSet::new(),
            plan: Plan::default(),
        }
    }

    /// Message `i` has `chars` characters now.
    fn resize(&mut self, i: usize, chars: usize) {
        self.total -= estimate_tokens(self.chars[i]);
        self.chars[i] = chars;
        self.total += estimate_tokens(chars);
    }

    /// Adds the edit of `kind` to block `b` of event `i`, if it is one.
    fn add(&mut self, i: usize, b: usize, kind: EditKind) {
        let event = &self.history.events()[i];
        let pos = (event.id, b);
        if self.taken.contains(&pos) || self.chosen.contains(&pos) {
            return;
        }
        let block = &event.message.content[b];
        let mut original = None;
        let replacement = match (kind, block) {
            (EditKind::Superseded, ContentBlock::ToolUse { name, .. }) => {
                Replacement::Input(stub::superseded(name))
            }
            (EditKind::Input, ContentBlock::ToolUse { input, .. }) => {
                match stub::shortened_input(input, self.config.input_min_chars) {
                    Some(short) => Replacement::Input(short),
                    None => return,
                }
            }
            (
                EditKind::Result,
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    ..
                },
            ) => {
                let spill = (self.files.base.as_ref())
                    .and_then(|base| stub::spill_file(content, tool_use_id, &base.join("spill")));
                let copy = self.files.copy_path(pos);
                let saved = match (&spill, &copy) {
                    (Some((path, bytes)), _) => Saved::Spill {
                        path,
                        bytes: *bytes,
                    },
                    (None, Some(path)) => Saved::Copy(path),
                    (None, None) => Saved::Nowhere,
                };
                let text = stub::result(content, saved, self.files.tools);
                // A result just over the limit can be shorter than its stub.
                if text.len() >= content.len() {
                    return;
                }
                if let (None, Some(path)) = (spill, copy) {
                    original = Some(Original {
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
            _ => return,
        };
        let edit = Edit {
            event: pos.0,
            block: b,
            kind,
            hash: hash(block),
            replacement,
        };
        // The same rule as when the log is read back.
        if fits(self.history, i, &edit).is_err() {
            return;
        }
        let new = match (block, &edit.replacement) {
            (ContentBlock::ToolUse { name, .. }, Replacement::Input(input)) => {
                name.len() + input.to_string().len()
            }
            (_, Replacement::Text(text)) => text.len(),
            _ => block.char_len(),
        };
        self.resize(i, self.chars[i].saturating_sub(block.char_len()) + new);
        // The thinking this trim drops: in the messages after its first edit.
        if self.first.is_none_or(|f| i < f) {
            let end = self.first.map_or(self.chars.len(), |f| f + 1);
            for j in i + 1..end {
                let left = self.chars[j].saturating_sub(self.thinking[j]);
                self.resize(j, left);
            }
            self.first = Some(i);
        }
        self.chosen.insert(pos);
        self.plan.originals.extend(original);
        self.plan.edits.push(edit);
    }
}

/// The message ranges a trim may edit.
struct Regions {
    /// Between the first message and the recent window.
    middle: Range<usize>,
    /// Where tool results may also be trimmed (on overflow).
    results_too: Range<usize>,
}

/// The preamble is the first message, with the summaries. The recent window is the
/// newest steps (an assistant message and the user message that answers it) holding at
/// least `recent` of the usable window and at least `min_recent_steps` steps; it starts
/// at an assistant message. The middle is what's in between, after the summarized span.
/// Indices are the history's.
fn regions(
    rendered: &Rendered,
    budget: &Budget,
    config: &ContextConfig,
    trigger: Trigger,
) -> Option<Regions> {
    let window = window(rendered, budget, config, trigger == Trigger::Manual)?;
    let newest = (rendered.start..rendered.messages.len())
        .rev()
        .find(|&i| rendered.messages[i].role == Role::Assistant)?;
    let results_too = if trigger == Trigger::Overflow {
        window..newest
    } else {
        window..window
    };
    Some(Regions {
        middle: rendered.start..window,
        results_too,
    })
}

/// Where the recent window starts. `short`: it holds `min_recent_steps` steps however
/// small they are, as for `/compact`.
fn window(
    rendered: &Rendered,
    budget: &Budget,
    config: &ContextConfig,
    short: bool,
) -> Option<usize> {
    let messages = &rendered.messages;
    if messages.first()?.role != Role::User {
        return None;
    }
    let steps: Vec<usize> = (rendered.start.max(1)..messages.len())
        .filter(|&i| messages[i].role == Role::Assistant)
        .collect();
    let minimum = config.recent * budget.usable() as f64;
    let mut start = *steps.last()?;
    let mut raw = 0;
    let mut end = messages.len();
    for (taken, &step) in steps.iter().rev().enumerate() {
        start = step;
        raw += messages[step..end]
            .iter()
            .map(|m| estimate_tokens(m.char_len()))
            .sum::<u64>();
        end = step;
        let enough_steps = taken + 1 >= config.min_recent_steps;
        let enough_tokens = short || budget.calibrator.calibrate(raw) as f64 >= minimum;
        if enough_steps && enough_tokens {
            break;
        }
    }
    Some(start)
}

/// The span a summary replaces: the middle after the earlier summaries, up to the
/// recent window, as history indices. It starts at a reply and ends just before one.
/// `short`: the window holds only `min_recent_steps` steps (`/compact`, an overflow).
pub fn summary_span(
    rendered: &Rendered,
    budget: &Budget,
    config: &ContextConfig,
    short: bool,
) -> Option<Range<usize>> {
    let window = window(rendered, budget, config, short)?;
    (rendered.start < window).then_some(rendered.start..window)
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
