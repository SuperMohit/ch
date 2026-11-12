//! The compaction log: one record per trim, appended to
//! `<session dir>/context/log.jsonl`. A record holds everything needed to render its
//! effect, so the view is the same after a restart even if the settings or the tools
//! changed, and it is bound to the history it was decided on.

use std::collections::HashSet;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use ch_kernel::util::{create_private, private_dir};
use ch_kernel::{ContentBlock, EventId, History, Role};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::stub::{self, REMINDER, RESULT_PREFIX};

/// A block of the history: its event and its index in the event's message.
pub type Pos = (EventId, usize);

/// What a record does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Shortens blocks, and drops the thinking blocks after the first one, through
    /// `at_event`.
    Elided,
    /// Drops every thinking block through `at_event`: written when the log couldn't be
    /// applied, so the history goes on without reasoning that no longer matches it.
    Reset,
}

/// What decided a record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    /// The view passed `soft`.
    Soft,
    /// The provider rejected the request as too long.
    Overflow,
    /// `/compact`.
    Manual,
    /// The log didn't match the history.
    Recovery,
}

/// What an edit shortened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EditKind {
    /// A call to a superseded tool that a later call replaced: its whole input.
    Superseded,
    /// A tool result.
    Result,
    /// Long strings in a tool call's input.
    Input,
    /// A `<system-reminder>` block.
    Reminder,
}

/// The new contents of an edited block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Replacement {
    /// A tool call's input.
    Input(Value),
    /// A tool result's content or a text block's text.
    Text(String),
}

/// One block's contents replaced. The block stays, so every `tool_use` keeps its
/// `tool_result`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Edit {
    pub event: EventId,
    pub block: usize,
    pub kind: EditKind,
    /// [`hash`] of the original block.
    pub hash: String,
    pub replacement: Replacement,
}

impl Edit {
    pub fn pos(&self) -> Pos {
        (self.event, self.block)
    }
}

/// One entry of the log. An `elided` record's edits are sorted by position.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub kind: Kind,
    pub trigger: Trigger,
    /// The newest history event when it was decided.
    pub at_event: EventId,
    /// How many blocks `at_event` had then. The session adds the call's reminders to
    /// that message after the view, so only these count.
    pub at_blocks: usize,
    /// [`hash_blocks`] of those blocks: event ids are reused after a crash, so the id
    /// alone doesn't say it is the same history.
    pub at_hash: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub edits: Vec<Edit>,
}

impl Record {
    /// A record bound to `history`'s newest event, or `None` for an empty history.
    pub fn new(kind: Kind, trigger: Trigger, history: &History, edits: Vec<Edit>) -> Option<Self> {
        let last = history.last()?;
        Some(Self {
            kind,
            trigger,
            at_event: last.id,
            at_blocks: last.message.content.len(),
            at_hash: hash_blocks(&last.message.content),
            edits,
        })
    }

    /// Whether the thinking block at `pos` was written against what this record changed.
    pub fn drops_thinking_at(&self, pos: Pos) -> bool {
        pos.0 <= self.at_event
            && match self.kind {
                Kind::Reset => true,
                Kind::Elided => self.edits.first().is_some_and(|first| pos > first.pos()),
            }
    }

    /// Why this record can't be applied to `history`, if it can't. Blocks in `taken`
    /// were edited by an earlier record.
    pub fn check(&self, history: &History, taken: &HashSet<Pos>) -> Result<(), String> {
        let events = history.events();
        let Some(at) = find(history, self.at_event) else {
            return Err(format!("event {} is missing", self.at_event));
        };
        let blocks = &events[at].message.content;
        if blocks.len() < self.at_blocks || hash_blocks(&blocks[..self.at_blocks]) != self.at_hash {
            return Err(format!(
                "event {} isn't the one it was made on",
                self.at_event
            ));
        }
        if self.kind == Kind::Reset {
            return if self.edits.is_empty() {
                Ok(())
            } else {
                Err("a reset has edits".into())
            };
        }
        if self.edits.is_empty() {
            return Err("it has no edits".into());
        }
        let mut last = None;
        for edit in &self.edits {
            if last.is_some_and(|l| l >= edit.pos()) {
                return Err("its edits are out of order".into());
            }
            last = Some(edit.pos());
            if taken.contains(&edit.pos()) {
                return Err(format!(
                    "block {} of event {} is edited twice",
                    edit.block, edit.event
                ));
            }
            if edit.event >= self.at_event {
                return Err(format!(
                    "event {} is not before event {}",
                    edit.event, self.at_event
                ));
            }
            let i = find(history, edit.event)
                .ok_or_else(|| format!("event {} is missing", edit.event))?;
            fits(history, i, edit)?;
        }
        Ok(())
    }
}

/// Whether `edit` is one the planner makes, on event `i` of `history`: never on the
/// first message; a superseded call's input becomes its marker; an input keeps its
/// shape, with long strings replaced by their markers; a result becomes a stub; a
/// reminder (not a turn's prompt) becomes the reminder stub. The block must be the one
/// the edit was made from.
pub fn fits(history: &History, i: usize, edit: &Edit) -> Result<(), String> {
    let event = &history.events()[i];
    let at = || format!("block {} of event {}", edit.block, edit.event);
    if i == 0 {
        return Err(format!("{} is in the first message", at()));
    }
    let Some(block) = event.message.content.get(edit.block) else {
        return Err(format!("{} is missing", at()));
    };
    let ok = match (edit.kind, block, &edit.replacement) {
        (EditKind::Superseded, ContentBlock::ToolUse { name, .. }, Replacement::Input(new)) => {
            *new == stub::superseded(name)
        }
        (EditKind::Input, ContentBlock::ToolUse { input, .. }, Replacement::Input(new)) => {
            stub::is_shortened(input, new)
        }
        (EditKind::Result, ContentBlock::ToolResult { .. }, Replacement::Text(new)) => {
            new.starts_with(RESULT_PREFIX)
        }
        (EditKind::Reminder, ContentBlock::Text { text }, Replacement::Text(new)) => {
            event.message.role == Role::User
                && text.starts_with("<system-reminder>")
                && !is_prompt(history, i, edit.block)
                && new == REMINDER
        }
        _ => false,
    };
    if !ok || hash(block) != edit.hash {
        return Err(format!("{} has changed", at()));
    }
    Ok(())
}

/// Whether block `b` of event `i` is a turn's prompt: the first block of the turn's first
/// event. Notes and reminders come after it, or in later events.
pub fn is_prompt(history: &History, i: usize, b: usize) -> bool {
    let events = history.events();
    b == 0 && (i == 0 || events[i - 1].turn != events[i].turn)
}

/// The index of event `id` in `history`.
pub fn find(history: &History, id: EventId) -> Option<usize> {
    history.events().binary_search_by_key(&id, |e| e.id).ok()
}

/// Whether the model answered after event `id`: then what it was sent then is in the
/// history, signed reasoning included.
pub fn answered_after(history: &History, id: EventId) -> bool {
    history
        .events()
        .iter()
        .any(|e| e.id > id && e.message.role == Role::Assistant)
}

/// A digest of `block`, to tell whether a record still matches the history.
pub fn hash(block: &ContentBlock) -> String {
    let bytes = serde_json::to_vec(block).expect("a content block serializes");
    format!("{:x}", Sha256::digest(bytes))
}

/// A digest of `blocks`.
pub fn hash_blocks(blocks: &[ContentBlock]) -> String {
    let bytes = serde_json::to_vec(blocks).expect("content blocks serialize");
    format!("{:x}", Sha256::digest(bytes))
}

/// `<session dir>/context/log.jsonl`.
fn log_path(session_dir: &Path) -> PathBuf {
    session_dir.join("context").join("log.jsonl")
}

/// The lines of a session's log, each parsed or not. `Err` when the log can't be read.
pub fn load(session_dir: &Path) -> Result<Vec<Result<Record, String>>, String> {
    let path = log_path(session_dir);
    match fs::symlink_metadata(&path) {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => return Err(format!("{} is not a file", path.display())),
        Err(_) => return Ok(Vec::new()),
    }
    let text = fs::read_to_string(&path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    Ok(text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .enumerate()
        .map(|(i, line)| {
            serde_json::from_str(line).map_err(|e| format!("line {} doesn't parse: {e}", i + 1))
        })
        .collect())
}

/// Appends `record` to the session's log and makes it durable before returning, so a
/// view is never sent with a trim that a restart wouldn't render.
pub fn append(session_dir: &Path, record: &Record) -> io::Result<()> {
    let dir = private_dir(session_dir, "context")?;
    let path = dir.join("log.jsonl");
    if let Ok(meta) = fs::symlink_metadata(&path)
        && !meta.is_file()
    {
        return Err(io::Error::other(format!(
            "{} is not a file (a symlink?)",
            path.display()
        )));
    }
    let mut options = fs::OpenOptions::new();
    options.append(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut line = serde_json::to_vec(record).expect("a record serializes");
    line.push(b'\n');
    let mut file = options.open(&path)?;
    file.write_all(&line)?;
    file.sync_data()
}

/// Replaces the session's log with `records`: a new file, made durable, then renamed
/// over the old one, so a crash leaves one or the other.
pub fn rewrite(session_dir: &Path, records: &[Record]) -> io::Result<()> {
    let dir = private_dir(session_dir, "context")?;
    let tmp = dir.join("log.jsonl.tmp");
    // One left by a crash: it was never renamed, so it holds nothing needed.
    match fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let mut text = Vec::new();
    for record in records {
        text.extend(serde_json::to_vec(record).expect("a record serializes"));
        text.push(b'\n');
    }
    let written = create_private(&tmp).and_then(|mut f| {
        f.write_all(&text)?;
        f.sync_all()
    });
    if let Err(e) = written.and_then(|()| fs::rename(&tmp, dir.join("log.jsonl"))) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    // The rename itself.
    #[cfg(unix)]
    fs::File::open(&dir)?.sync_all()?;
    Ok(())
}
