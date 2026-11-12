//! The compaction log: one record per trim or summary, appended to
//! `<session dir>/context/log.jsonl`. A record holds everything needed to render its
//! effect, so the view is the same after a restart even if the settings or the tools
//! changed, and it is bound to the history it was decided on.

use std::collections::HashSet;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use ch_kernel::util::{create_private, private_dir};
use ch_kernel::{ContentBlock, Event, EventId, History, Role};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::stub::{self, REMINDER, RESULT_PREFIX};
use crate::summary::Summary;

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
    /// Replaces a span of the history with a summary appended to the first message, and
    /// drops every thinking block through `at_event`.
    Summarized,
}

/// What decided a record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    /// The view passed `soft`.
    Soft,
    /// The view passed `hard`.
    Hard,
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
    /// A `summarized` record's summary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<Summary>,
}

/// What the records so far have done: the blocks they edited and the summaries in the
/// first message, oldest first.
#[derive(Debug, Clone, Default)]
pub struct Applied {
    pub taken: HashSet<Pos>,
    pub summaries: Vec<Summary>,
    /// The highest summary number so far, merged ones included.
    pub last_n: usize,
}

impl Applied {
    pub fn of(records: &[Record]) -> Self {
        let mut applied = Self::default();
        for record in records {
            applied.add(record);
        }
        applied
    }

    pub fn add(&mut self, record: &Record) {
        self.taken.extend(record.edits.iter().map(Edit::pos));
        if let Some(s) = &record.summary {
            if s.merged {
                self.summaries.drain(..2.min(self.summaries.len()));
                self.summaries.insert(0, s.clone());
            } else {
                self.summaries.push(s.clone());
            }
            self.last_n = self.last_n.max(s.n);
        }
    }

    /// The history index of the first event after the summarized span: 1 with no
    /// summary.
    pub fn start(&self, history: &History) -> usize {
        self.summaries
            .last()
            .and_then(|s| find(history, s.to))
            .map_or(1, |i| i + 1)
    }
}

impl Record {
    /// A trim bound to `history`'s newest event, or `None` for an empty history.
    pub fn elided(trigger: Trigger, history: &History, edits: Vec<Edit>) -> Option<Self> {
        Self::bound(Kind::Elided, trigger, history.events().last()?, edits, None)
    }

    /// A summary bound to `history`'s newest event. A summary has a span, so the history
    /// isn't empty.
    pub fn summarized(trigger: Trigger, history: &History, summary: Summary) -> Self {
        let last = history.events().last().expect("a summarized history");
        Self::bound(Kind::Summarized, trigger, last, Vec::new(), Some(summary))
            .expect("bound to an event")
    }

    /// A reset of every thinking block so far, bound to the newest assistant message:
    /// that one is in the saved history whenever the log is read, while a newer prompt
    /// is saved only at the end of its turn. `None` when the model hasn't answered yet,
    /// so there is no thinking to leave out.
    pub fn reset(history: &History) -> Option<Self> {
        let last = (history.events().iter())
            .rev()
            .find(|e| e.message.role == Role::Assistant)?;
        Self::bound(Kind::Reset, Trigger::Recovery, last, Vec::new(), None)
    }

    fn bound(
        kind: Kind,
        trigger: Trigger,
        at: &Event,
        edits: Vec<Edit>,
        summary: Option<Summary>,
    ) -> Option<Self> {
        Some(Self {
            kind,
            trigger,
            at_event: at.id,
            at_blocks: at.message.content.len(),
            at_hash: hash_blocks(&at.message.content),
            edits,
            summary,
        })
    }

    /// Whether the thinking block at `pos` was written against what this record changed.
    /// A summary changes the first message, so all thinking before it goes.
    pub fn drops_thinking_at(&self, pos: Pos) -> bool {
        pos.0 <= self.at_event
            && match self.kind {
                Kind::Reset | Kind::Summarized => true,
                Kind::Elided => self.edits.first().is_some_and(|first| pos > first.pos()),
            }
    }

    /// Why this record can't be applied to `history` after the records that did
    /// `applied`, if it can't.
    pub fn check(&self, history: &History, applied: &Applied) -> Result<(), String> {
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
        match (self.kind, &self.summary) {
            (Kind::Reset, None) if self.edits.is_empty() => Ok(()),
            (Kind::Summarized, Some(s)) if self.edits.is_empty() => s.check(history, applied, at),
            (Kind::Elided, None) => self.check_edits(history, applied),
            _ => Err(format!("a {:?} record has the wrong parts", self.kind)),
        }
    }

    fn check_edits(&self, history: &History, applied: &Applied) -> Result<(), String> {
        if self.edits.is_empty() {
            return Err("it has no edits".into());
        }
        let start = applied.start(history);
        let mut last = None;
        for edit in &self.edits {
            if last.is_some_and(|l| l >= edit.pos()) {
                return Err("its edits are out of order".into());
            }
            last = Some(edit.pos());
            if applied.taken.contains(&edit.pos()) {
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
            if i < start {
                return Err(format!("event {} was summarized", edit.event));
            }
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

/// A session's log as read: each line parsed or not.
#[derive(Debug, Default)]
pub struct Lines {
    pub entries: Vec<Result<Record, String>>,
    /// The last line has no newline: an append stopped part way, so it was never used.
    pub torn: bool,
}

/// The lines of a session's log. `Err` when the log can't be read.
pub fn load(session_dir: &Path) -> Result<Lines, String> {
    let path = log_path(session_dir);
    match fs::symlink_metadata(&path) {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => return Err(format!("{} is not a file", path.display())),
        Err(_) => return Ok(Lines::default()),
    }
    let bytes = fs::read(&path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    let torn = !bytes.is_empty() && !bytes.ends_with(b"\n");
    // Bytes, not text: a torn line can end inside a character.
    let entries = bytes
        .split(|&b| b == b'\n')
        .filter(|l| !l.iter().all(u8::is_ascii_whitespace))
        .enumerate()
        .map(|(i, line)| {
            serde_json::from_slice(line).map_err(|e| format!("line {} doesn't parse: {e}", i + 1))
        })
        .collect();
    Ok(Lines { entries, torn })
}

/// Why an append failed, and whether the log is as it was before it.
#[derive(Debug)]
pub struct AppendFailed {
    pub error: io::Error,
    /// The partial record was taken back out. When it couldn't be, a later process may
    /// find a record whose trim was never sent.
    pub intact: bool,
}

/// Appends `record` to the session's log and makes it durable before returning, so a
/// view is never sent with a trim that a restart wouldn't render. On failure the log is
/// cut back to its old length.
pub fn append(session_dir: &Path, record: &Record) -> Result<(), AppendFailed> {
    append_with(session_dir, record, &Steps::REAL)
}

/// The steps of an append that can fail; tests replace them.
struct Steps {
    write: fn(&mut fs::File, &[u8]) -> io::Result<()>,
    sync: fn(&fs::File) -> io::Result<()>,
    cut: fn(&fs::File, u64) -> io::Result<()>,
}

impl Steps {
    const REAL: Self = Self {
        write: |f, bytes| f.write_all(bytes),
        sync: fs::File::sync_data,
        cut: |f, len| {
            f.set_len(len)?;
            f.sync_data()
        },
    };
}

fn append_with(session_dir: &Path, record: &Record, steps: &Steps) -> Result<(), AppendFailed> {
    let opened = private_dir(session_dir, "context").and_then(|dir| {
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
        let file = options.open(&path)?;
        let len = file.metadata()?.len();
        Ok((dir, file, len))
    });
    let (dir, mut file, len) = opened.map_err(|error| AppendFailed {
        error,
        intact: true,
    })?;
    let mut line = serde_json::to_vec(record).expect("a record serializes");
    line.push(b'\n');
    if let Err(error) = (steps.write)(&mut file, &line).and_then(|()| (steps.sync)(&file)) {
        let intact = (steps.cut)(&file, len).is_ok();
        return Err(AppendFailed { error, intact });
    }
    // The log's own directory entry, the first time.
    sync_dir(&dir).map_err(|error| AppendFailed {
        error,
        intact: true,
    })
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
    sync_dir(&dir)
}

/// Makes `dir`'s entries durable: files created or renamed in it. Does nothing off Unix.
pub fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    fs::File::open(dir)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

#[cfg(test)]
mod tests {
    use ch_kernel::{Message, ToolCall};
    use serde_json::json;

    use super::*;

    fn record(n: u64) -> Record {
        Record {
            kind: Kind::Reset,
            trigger: Trigger::Recovery,
            at_event: n,
            at_blocks: 1,
            at_hash: "h".into(),
            edits: Vec::new(),
            summary: None,
        }
    }

    fn lines(session: &Path) -> Lines {
        load(session).unwrap()
    }

    #[test]
    fn a_failed_write_or_sync_is_taken_back_out_of_the_log() {
        let tmp = tempfile::tempdir().unwrap();
        let session = tmp.path().join("s1");
        append(&session, &record(1)).unwrap();
        let half: Steps = Steps {
            write: |f, bytes| {
                f.write_all(&bytes[..bytes.len() / 2])?;
                Err(io::Error::other("disk full"))
            },
            ..Steps::REAL
        };
        let failed = append_with(&session, &record(2), &half).unwrap_err();
        assert!(failed.intact);
        let no_sync: Steps = Steps {
            sync: |_| Err(io::Error::other("sync failed")),
            ..Steps::REAL
        };
        let failed = append_with(&session, &record(3), &no_sync).unwrap_err();
        assert!(failed.intact);
        // The next append follows the first record cleanly.
        append(&session, &record(4)).unwrap();
        let read = lines(&session);
        assert!(!read.torn);
        let at: Vec<u64> = read
            .entries
            .iter()
            .map(|e| e.as_ref().unwrap().at_event)
            .collect();
        assert_eq!(at, [1, 4]);
    }

    #[test]
    fn a_log_that_can_t_be_cut_back_is_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let session = tmp.path().join("s1");
        let stuck: Steps = Steps {
            sync: |_| Err(io::Error::other("sync failed")),
            cut: |_, _| Err(io::Error::other("read-only")),
            ..Steps::REAL
        };
        assert!(
            !append_with(&session, &record(1), &stuck)
                .unwrap_err()
                .intact
        );
    }

    #[test]
    fn only_an_unterminated_last_line_is_torn() {
        let tmp = tempfile::tempdir().unwrap();
        let session = tmp.path().join("s1");
        append(&session, &record(1)).unwrap();
        let path = log_path(&session);
        let mut whole = fs::read(&path).unwrap();
        // Cut inside a multi-byte character, as a crash can.
        let mut torn = whole.clone();
        torn.extend("{\"kind\":\"reset\",\"trigger\":\"é".as_bytes());
        torn.pop();
        fs::write(&path, &torn).unwrap();
        let read = lines(&session);
        assert!(read.torn);
        assert_eq!(read.entries.len(), 2);
        assert!(read.entries[0].is_ok() && read.entries[1].is_err());
        // The same garbage with a newline is a damaged line, not a torn one.
        whole.extend(b"not json\n");
        fs::write(&path, &whole).unwrap();
        let read = lines(&session);
        assert!(!read.torn);
        assert!(read.entries[1].is_err());
    }

    /// A history: a task (with a note), a call, its result with a reminder, a second turn
    /// whose prompt looks like a reminder, and the model's answer.
    fn history() -> History {
        let mut h = History::new();
        h.push(
            1,
            Message {
                role: Role::User,
                content: vec![
                    ContentBlock::text("task"),
                    ContentBlock::text("<system-reminder>\nnote\n</system-reminder>"),
                ],
            },
        );
        h.push(
            1,
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "c1".into(),
                    name: "write_file".into(),
                    input: json!({"path": "a.rs", "content": "x".repeat(2000)}),
                }],
            },
        );
        h.push(
            1,
            Message {
                role: Role::User,
                content: vec![
                    ContentBlock::ToolResult {
                        tool_use_id: "c1".into(),
                        content: "y".repeat(800),
                        is_error: false,
                    },
                    ContentBlock::text("<system-reminder>\ncheck\n</system-reminder>"),
                ],
            },
        );
        h.push(1, Message::assistant("<system-reminder> said by the model"));
        h.push(2, Message::user("<system-reminder> typed by the user"));
        h
    }

    fn edit(h: &History, i: usize, block: usize, kind: EditKind, replacement: Replacement) -> Edit {
        let event = &h.events()[i];
        Edit {
            event: event.id,
            block,
            kind,
            hash: hash(&event.message.content[block]),
            replacement,
        }
    }

    #[test]
    fn each_kind_of_edit_must_be_one_the_planner_makes() {
        let h = history();
        let ToolCall { input, .. } = h.events()[1].message.tool_calls().remove(0);
        let short = stub::shortened_input(&input, 1000).unwrap();
        let fits =
            |i, block, kind, replacement| fits(&h, i, &edit(&h, i, block, kind, replacement));

        // Inputs: only long strings, each replaced by its own marker.
        assert!(fits(1, 0, EditKind::Input, Replacement::Input(short.clone())).is_ok());
        let mut moved = short.clone();
        moved["path"] = json!("b.rs");
        assert!(fits(1, 0, EditKind::Input, Replacement::Input(moved)).is_err());
        // Superseded calls: exactly the marker.
        let sup = stub::superseded("write_file");
        assert!(fits(1, 0, EditKind::Superseded, Replacement::Input(sup)).is_ok());
        let other = stub::superseded("bash");
        assert!(fits(1, 0, EditKind::Superseded, Replacement::Input(other)).is_err());
        // Results: a stub.
        let ok = format!("{RESULT_PREFIX}1 line, 800 characters. Run the command again.]");
        assert!(fits(2, 0, EditKind::Result, Replacement::Text(ok)).is_ok());
        let bad = "[Output removed]".to_string();
        assert!(fits(2, 0, EditKind::Result, Replacement::Text(bad)).is_err());
        // Reminders: a user message's reminder that isn't a prompt, and only the stub.
        let stub = || Replacement::Text(REMINDER.into());
        assert!(fits(2, 1, EditKind::Reminder, stub()).is_ok());
        assert!(fits(2, 1, EditKind::Reminder, Replacement::Text("[gone]".into())).is_err());
        assert!(
            fits(3, 0, EditKind::Reminder, stub()).is_err(),
            "the model's text"
        );
        assert!(
            fits(4, 0, EditKind::Reminder, stub()).is_err(),
            "a turn's prompt"
        );
        // Never the first message, and never a block that changed.
        assert!(fits(0, 1, EditKind::Reminder, stub()).is_err());
        let mut changed = edit(&h, 2, 1, EditKind::Reminder, stub());
        changed.hash = "0000".into();
        assert!(super::fits(&h, 2, &changed).is_err());
    }
}
