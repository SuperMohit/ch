//! The compaction log: one record per trim, appended to
//! `<session dir>/context/log.jsonl`. A record holds everything needed to render its
//! effect, so the view is the same after a restart even if the settings or the tools
//! changed.

use std::collections::HashSet;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use ch_kernel::util::{anchored, restrict};
use ch_kernel::{CompactionKind, ContentBlock, EventId, History};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// A block of the history: its event and its index in the event's message.
pub type Pos = (EventId, usize);

/// What decided a compaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    /// The view passed `soft`.
    Soft,
    /// The provider rejected the request as too long.
    Overflow,
    /// `/compact`.
    Manual,
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

/// One compaction. Its edits are sorted by position; thinking blocks after the first
/// edit, through `at_event`, are dropped.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub kind: CompactionKind,
    pub trigger: Trigger,
    /// The newest history event when it was decided.
    pub at_event: EventId,
    pub edits: Vec<Edit>,
}

impl Record {
    /// Whether the thinking block at `pos` was written against what this record changed.
    pub fn drops_thinking_at(&self, pos: Pos) -> bool {
        self.edits
            .first()
            .is_some_and(|first| pos > first.pos() && pos.0 <= self.at_event)
    }

    /// Why this record can't be applied to `history`, if it can't: an event is missing,
    /// or a block isn't the one it was made from.
    pub fn check(&self, history: &History, taken: &HashSet<Pos>) -> Result<(), String> {
        let events = history.events();
        if self.edits.is_empty() {
            return Err("it has no edits".into());
        }
        if find(history, self.at_event).is_none() {
            return Err(format!("event {} is missing", self.at_event));
        }
        let mut last = None;
        for edit in &self.edits {
            if last.is_some_and(|l| l >= edit.pos()) || edit.event > self.at_event {
                return Err("its edits are out of order".into());
            }
            last = Some(edit.pos());
            if taken.contains(&edit.pos()) {
                return Err(format!(
                    "block {} of event {} is edited twice",
                    edit.block, edit.event
                ));
            }
            let Some(i) = find(history, edit.event) else {
                return Err(format!("event {} is missing", edit.event));
            };
            let Some(block) = events[i].message.content.get(edit.block) else {
                return Err(format!("event {} has no block {}", edit.event, edit.block));
            };
            let fits = matches!(
                (edit.kind, block, &edit.replacement),
                (
                    EditKind::Superseded | EditKind::Input,
                    ContentBlock::ToolUse { .. },
                    Replacement::Input(Value::Object(_))
                ) | (
                    EditKind::Result,
                    ContentBlock::ToolResult { .. },
                    Replacement::Text(_)
                ) | (
                    EditKind::Reminder,
                    ContentBlock::Text { .. },
                    Replacement::Text(_)
                )
            );
            if !fits || hash(block) != edit.hash {
                return Err(format!(
                    "block {} of event {} has changed",
                    edit.block, edit.event
                ));
            }
        }
        Ok(())
    }
}

/// The index of event `id` in `history`.
pub fn find(history: &History, id: EventId) -> Option<usize> {
    history.events().binary_search_by_key(&id, |e| e.id).ok()
}

/// A digest of `block`, to tell whether a record still matches the history.
pub fn hash(block: &ContentBlock) -> String {
    let bytes = serde_json::to_vec(block).expect("a content block serializes");
    format!("{:x}", Sha256::digest(bytes))
}

/// `<session dir>/context/log.jsonl`.
fn log_path(session_dir: &Path) -> PathBuf {
    session_dir.join("context").join("log.jsonl")
}

/// The records saved for a session, in order. A line that doesn't parse is returned as
/// an error message.
pub fn load(session_dir: &Path) -> Vec<Result<Record, String>> {
    let path = log_path(session_dir);
    match fs::symlink_metadata(&path) {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => return vec![Err(format!("{} is not a file", path.display()))],
        Err(_) => return Vec::new(),
    }
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) => return vec![Err(format!("reading {}: {e}", path.display()))],
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .enumerate()
        .map(|(i, line)| {
            serde_json::from_str(line).map_err(|e| format!("line {} doesn't parse: {e}", i + 1))
        })
        .collect()
}

/// Appends `record` to the session's log.
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
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut line = serde_json::to_vec(record).expect("a record serializes");
    line.push(b'\n');
    options.open(&path)?.write_all(&line)
}

/// `<session dir>/<parts…>`, each directory created `0700` if needed, with the session
/// directory anchored as the file tools check it. Fails if any of them is a symlink.
pub fn private_dir(session_dir: &Path, parts: &str) -> io::Result<PathBuf> {
    fs::create_dir_all(session_dir)?;
    let mut dir = anchored(session_dir)?;
    for part in parts.split('/') {
        dir = dir.join(part);
        match fs::create_dir(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        if !fs::symlink_metadata(&dir)?.is_dir() {
            return Err(io::Error::other(format!(
                "{} is not a directory (a symlink?)",
                dir.display()
            )));
        }
        restrict(&dir)?;
    }
    Ok(dir)
}
