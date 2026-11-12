//! The `ContextStrategy`: one session's compaction log, the trim decision, and saving a
//! trim.

use std::collections::HashSet;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use ch_kernel::reexports::async_trait;
use ch_kernel::util::{ReadTools, anchored, create_private, private_dir, thousands};
use ch_kernel::{
    AgentEvent, Budget, CompactOutcome, CompactReason, CompactRequest, CompactionKind, ContextCx,
    ContextStrategy, History, Message, View,
};
use serde_json::json;

use crate::config::{ContextConfig, Strategy};
use crate::hook::MarkerHook;
use crate::log::{self, Edit, EditKind, Kind, Lines, Pos, Record, Replacement, Trigger};
use crate::plan::{self, Files, Original};
use crate::render::{is_thinking, render};
use crate::stub::{self, Saved};

/// Strategy id, also the plugin id.
pub const ID: &str = "context";

/// The `context` strategy.
pub struct Context {
    config: ContextConfig,
    shared: Arc<Shared>,
}

/// One session at a time: its records and the stub texts they wrote, which the marker
/// hook reads too.
#[derive(Default)]
pub(crate) struct Shared {
    session: Mutex<Option<SessionLog>>,
}

pub(crate) struct SessionLog {
    id: String,
    records: Vec<Record>,
    /// Result stubs and input markers in the records, which a tool call must not copy.
    pub(crate) written: HashSet<String>,
    /// Why no more trims are made in this session: its log may hold what wasn't sent.
    frozen: Option<String>,
}

impl Shared {
    pub(crate) fn lock(&self) -> MutexGuard<'_, Option<SessionLog>> {
        // Every change is one push or assignment, so a poisoned lock is still consistent.
        self.session
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The session log of `id`, if it is the one loaded.
    pub(crate) fn of<'a>(
        guard: &'a MutexGuard<'_, Option<SessionLog>>,
        id: &str,
    ) -> Option<&'a SessionLog> {
        guard.as_ref().filter(|s| s.id == id)
    }
}

impl SessionLog {
    fn add(&mut self, record: Record) {
        self.written.extend(written_by(&record));
        self.records.push(record);
    }
}

/// The texts `record` put in the view that only it writes: result stubs (with their
/// paths and sizes) and input markers (with their sizes).
fn written_by(record: &Record) -> Vec<String> {
    record
        .edits
        .iter()
        .flat_map(|e| match (&e.kind, &e.replacement) {
            (EditKind::Result, Replacement::Text(stub)) => vec![stub.clone()],
            (EditKind::Input, Replacement::Input(input)) => stub::markers_in(input),
            _ => Vec::new(),
        })
        .collect()
}

/// A session's records as loaded, and whether trims must stop.
struct Loaded {
    records: Vec<Record>,
    frozen: Option<String>,
}

/// A trim that has been decided but not saved.
struct Decision {
    record: Record,
    originals: Vec<Original>,
    before: u64,
    /// How the stubs tell the model to read the files, for a copy that fails.
    tools: ReadTools,
}

/// A trim that was made.
struct Trimmed {
    view: Vec<Message>,
    record: Record,
    before: u64,
    after: u64,
}

/// What came of trying to trim.
enum Trim {
    Made(Trimmed),
    /// Nothing to trim, or not enough.
    Nothing,
    /// Decided, but it couldn't be saved, so it wasn't made.
    NotSaved(String),
    /// Trims are off for this session.
    Frozen(String),
}

impl Context {
    pub fn new(config: ContextConfig) -> Self {
        Self {
            config,
            shared: Arc::default(),
        }
    }

    /// The `pre_tool` hook that refuses a call copying what this strategy wrote.
    pub fn marker_hook(&self) -> MarkerHook {
        MarkerHook::new(self.shared.clone())
    }

    /// The records of `ccx`'s session, and why trims are off if they are. They are read
    /// from its directory, and checked against `history`, when the session changes
    /// (`/clear`, `/resume`, a new process); the lock isn't held while reading.
    async fn records(&self, history: &History, ccx: &ContextCx) -> Loaded {
        let id = &*ccx.cx.session_id;
        let loaded = |slot: &MutexGuard<'_, Option<SessionLog>>| {
            Shared::of(slot, id).map(|s| Loaded {
                records: s.records.clone(),
                frozen: s.frozen.clone(),
            })
        };
        if let Some(l) = loaded(&self.shared.lock()) {
            return l;
        }
        let fresh = load(history, ccx).await;
        let mut slot = self.shared.lock();
        if Shared::of(&slot, id).is_none() {
            let mut session = SessionLog {
                id: id.to_string(),
                records: Vec::new(),
                written: HashSet::new(),
                frozen: fresh.frozen,
            };
            for record in fresh.records {
                session.add(record);
            }
            *slot = Some(session);
        }
        loaded(&slot).expect("just loaded")
    }

    fn freeze(&self, ccx: &ContextCx, why: String) {
        warn(ccx, format!("no more trims in this session: {why}"));
        if let Some(s) = (self.shared.lock().as_mut()).filter(|s| s.id == *ccx.cx.session_id) {
            s.frozen = Some(why);
        }
    }

    /// Plans a trim of `current`; `None` when there is nothing to trim or, above `soft`,
    /// when it wouldn't free `min_free`. Writes nothing.
    fn decide(
        &self,
        history: &History,
        records: &[Record],
        current: &[Message],
        budget: &Budget,
        trigger: Trigger,
        files: &Files,
    ) -> Option<Decision> {
        let taken: HashSet<Pos> = records
            .iter()
            .flat_map(|r| r.edits.iter().map(Edit::pos))
            .collect();
        let plan = plan::plan(
            history,
            current,
            &taken,
            budget,
            &self.config,
            trigger,
            files,
        );
        if plan.edits.is_empty() {
            return None;
        }
        let record = Record::elided(trigger, history, plan.edits)?;
        let before = budget.measure(current);
        let mut all = records.to_vec();
        all.push(record.clone());
        let freed = before.saturating_sub(budget.measure(&render(history, &all)));
        if trigger == Trigger::Soft
            && (freed as f64) < self.config.min_free * budget.usable() as f64
        {
            return None;
        }
        Some(Decision {
            record,
            originals: plan.originals,
            before,
            tools: files.tools,
        })
    }

    /// Saves a decided trim: the copies of its results, then its record. With a
    /// session directory it takes effect only once its record is durable, so a restart
    /// renders every view that was sent.
    async fn commit(
        &self,
        history: &History,
        mut records: Vec<Record>,
        decision: Decision,
        current: &[Message],
        budget: &Budget,
        ccx: &ContextCx,
    ) -> Trim {
        let Decision {
            mut record,
            originals,
            before,
            tools,
        } = decision;
        let session_dir = ccx.cx.session_dir.as_deref().map(PathBuf::as_path);
        let failed = match session_dir {
            Some(dir) => save_originals(dir.to_path_buf(), originals).await,
            None => Vec::new(),
        };
        for (original, why) in &failed {
            warn(
                ccx,
                format!("couldn't save {}: {why}", original.path.display()),
            );
            if let Some(edit) = record.edits.iter_mut().find(|e| e.pos() == original.pos) {
                edit.replacement =
                    Replacement::Text(stub::result(&original.content, Saved::Nowhere, tools));
            }
        }
        if let Some(dir) = session_dir {
            let (dir, line) = (dir.to_path_buf(), record.clone());
            let appended = tokio::task::spawn_blocking(move || log::append(&dir, &line))
                .await
                .unwrap_or_else(|e| {
                    Err(log::AppendFailed {
                        error: io::Error::other(e),
                        intact: false,
                    })
                });
            if let Err(failed) = appended {
                let why = format!("couldn't save it to the compaction log: {}", failed.error);
                warn(ccx, format!("a trim wasn't made: {why}"));
                if !failed.intact {
                    self.freeze(
                        ccx,
                        "the compaction log may keep part of a trim that wasn't sent".into(),
                    );
                }
                return Trim::NotSaved(why);
            }
        }
        if let Some(s) = (self.shared.lock().as_mut()).filter(|s| s.id == *ccx.cx.session_id) {
            s.add(record.clone());
        }
        records.push(record.clone());
        let view = render(history, &records);
        let after = budget.measure(&view);
        report(ccx, &record, current, &view, before, after);
        Trim::Made(Trimmed {
            view,
            record,
            before,
            after,
        })
    }

    async fn trim(
        &self,
        history: &History,
        loaded: Loaded,
        current: &[Message],
        budget: &Budget,
        ccx: &ContextCx,
        trigger: Trigger,
    ) -> Trim {
        if let Some(why) = loaded.frozen {
            return Trim::Frozen(why);
        }
        let files = Files {
            base: ccx.cx.session_dir.as_deref().and_then(|d| anchor(d)),
            tools: ReadTools::of(&ccx.cx),
        };
        let records = loaded.records;
        match self.decide(history, &records, current, budget, trigger, &files) {
            Some(decision) => {
                self.commit(history, records, decision, current, budget, ccx)
                    .await
            }
            None => Trim::Nothing,
        }
    }
}

#[async_trait]
impl ContextStrategy for Context {
    fn id(&self) -> &str {
        ID
    }

    async fn view(
        &self,
        history: &History,
        budget: Budget,
        cx: &ContextCx,
    ) -> anyhow::Result<View> {
        // Under `none` too: a session trimmed earlier must keep rendering the same bytes.
        let loaded = self.records(history, cx).await;
        let current = render(history, &loaded.records);
        let over = budget.measure(&current) as f64 > self.config.soft * budget.usable() as f64;
        if self.config.strategy == Strategy::Elide
            && over
            && let Trim::Made(trimmed) = self
                .trim(history, loaded, &current, &budget, cx, Trigger::Soft)
                .await
        {
            return Ok(View {
                messages: trimmed.view,
            });
        }
        Ok(View { messages: current })
    }

    async fn compact(
        &self,
        history: &History,
        budget: Budget,
        request: &CompactRequest,
        cx: &ContextCx,
    ) -> anyhow::Result<CompactOutcome> {
        if self.config.strategy == Strategy::None {
            return Ok(CompactOutcome {
                changed: false,
                message: "context management is off (plugins.context.strategy = \"none\")".into(),
            });
        }
        let loaded = self.records(history, cx).await;
        let current = render(history, &loaded.records);
        let trigger = match request.reason {
            CompactReason::Manual => Trigger::Manual,
            CompactReason::Overflow => Trigger::Overflow,
        };
        let message = match self
            .trim(history, loaded, &current, &budget, cx, trigger)
            .await
        {
            Trim::Made(t) => {
                return Ok(CompactOutcome {
                    changed: true,
                    message: describe(&t),
                });
            }
            Trim::Nothing => "nothing to trim: no tool result, tool input or reminder before \
                              the last steps is long enough"
                .into(),
            Trim::NotSaved(why) => format!("the trim couldn't be saved, so it wasn't made: {why}"),
            Trim::Frozen(why) => format!("trims are off for this session: {why}"),
        };
        Ok(CompactOutcome {
            changed: false,
            message,
        })
    }
}

/// The session directory anchored as the file tools check it, also before it exists;
/// `None` if it is a symlink.
fn anchor(dir: &Path) -> Option<PathBuf> {
    match fs::symlink_metadata(dir) {
        Ok(_) => anchored(dir).ok(),
        Err(_) => Some(fs::canonicalize(dir.parent()?).ok()?.join(dir.file_name()?)),
    }
}

/// The session's records, applied up to the first one that doesn't match `history`.
///
/// - A line an append left unfinished, or a trim decided on events the history doesn't
///   have or that the model never answered after (the log is ahead of a save that a
///   crash prevented), affects no reply in the history: it and the records after it are
///   dropped from the log.
/// - Any other record that doesn't match, or a reset that doesn't, means a view that was
///   sent can't be rendered again. The records from it on are dropped and a fresh reset
///   takes their place: every thinking block so far is left out, which the API accepts,
///   and the log keeps that so later requests repeat it.
///
/// When the log can't be rewritten, trims stop for the session: it may still hold
/// records that weren't applied.
async fn load(history: &History, ccx: &ContextCx) -> Loaded {
    let mut loaded = Loaded {
        records: Vec::new(),
        frozen: None,
    };
    let Some(dir) = ccx.cx.session_dir.as_deref().cloned() else {
        return loaded;
    };
    let read = {
        let dir = dir.clone();
        tokio::task::spawn_blocking(move || log::load(&dir))
            .await
            .unwrap_or_else(|e| Err(e.to_string()))
    };
    let (Lines { entries, torn }, unreadable) = match read {
        Ok(lines) => (lines, false),
        Err(why) => (
            Lines {
                entries: vec![Err(why)],
                torn: false,
            },
            true,
        ),
    };
    let count = entries.len();
    let later_reset = |from: usize| {
        entries[from..]
            .iter()
            .any(|e| e.as_ref().is_ok_and(|r| r.kind == Kind::Reset))
    };
    let mut taken = HashSet::new();
    let mut cut = None;
    for (i, entry) in entries.iter().enumerate() {
        if torn && i + 1 == count {
            cut = Some((i, true, "its last line is unfinished".to_string()));
            break;
        }
        let why = match entry {
            Err(why) => why.clone(),
            Ok(record) => match record.check(history, &taken) {
                Ok(()) => {
                    taken.extend(record.edits.iter().map(Edit::pos));
                    loaded.records.push(record.clone());
                    continue;
                }
                Err(why) => {
                    let harmless = record.kind != Kind::Reset
                        && !later_reset(i + 1)
                        && !log::answered_after(history, record.at_event);
                    cut = Some((i, harmless && !unreadable, why));
                    break;
                }
            },
        };
        cut = Some((i, false, why));
        break;
    }
    let Some((i, harmless, why)) = cut else {
        return loaded;
    };
    if harmless {
        warn(
            ccx,
            format!(
                "ignoring compaction records from {} on: {why}; they are newer than the \
                 saved session",
                i + 1
            ),
        );
    } else {
        warn(
            ccx,
            format!(
                "the compaction log doesn't match this session (record {}: {why}); the \
                 model's earlier reasoning is left out from now on",
                i + 1
            ),
        );
        loaded.records.extend(Record::reset(history));
    }
    let records = loaded.records.clone();
    let rewritten = tokio::task::spawn_blocking(move || log::rewrite(&dir, &records))
        .await
        .unwrap_or_else(|e| Err(io::Error::other(e)));
    if let Err(e) = rewritten {
        let why = format!("couldn't rewrite the compaction log: {e}");
        warn(ccx, format!("no more trims in this session: {why}"));
        loaded.frozen = Some(why);
    }
    loaded
}

/// Saves the originals of trimmed results off the async runtime: they can be large.
/// The copies and their directories are made durable before the record that names them
/// is written. Returns those that couldn't be saved, and why.
async fn save_originals(session_dir: PathBuf, originals: Vec<Original>) -> Vec<(Original, String)> {
    if originals.is_empty() {
        return Vec::new();
    }
    let all = originals.clone();
    let task = tokio::task::spawn_blocking(move || {
        let dir = match private_dir(&session_dir, "context/elided") {
            Ok(dir) => dir,
            Err(e) => {
                let why = e.to_string();
                return originals.into_iter().map(|o| (o, why.clone())).collect();
            }
        };
        let mut failed: Vec<(Original, String)> = Vec::new();
        for o in originals {
            let Some(name) = o.path.file_name() else {
                continue;
            };
            if let Err(e) = save_original(&dir.join(name), &o.content, &|f, b| f.write_all(b)) {
                failed.push((o, e.to_string()));
            }
        }
        // `elided/`, `context/` and the session directory may all be new.
        let synced = log::sync_dir(&dir)
            .and_then(|()| dir.parent().map_or(Ok(()), log::sync_dir))
            .and_then(|()| {
                dir.parent()
                    .and_then(Path::parent)
                    .map_or(Ok(()), log::sync_dir)
            });
        if let Err(e) = synced {
            tracing::warn!("syncing {}: {e}", dir.display());
        }
        failed
    });
    match task.await {
        Ok(failed) => failed,
        Err(e) => all.into_iter().map(|o| (o, e.to_string())).collect(),
    }
}

/// Writes a new file readable only by the user, made durable; a failed write leaves no
/// file. One already there (from a trim that crashed before its record) is kept if it
/// is private and holds the same text.
fn save_original(
    path: &Path,
    content: &str,
    write: &dyn Fn(&mut fs::File, &[u8]) -> io::Result<()>,
) -> io::Result<()> {
    match create_private(path) {
        Ok(mut file) => write(&mut file, content.as_bytes())
            .and_then(|()| file.sync_all())
            .inspect_err(|_| {
                drop(file);
                let _ = fs::remove_file(path);
            }),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            let meta = fs::symlink_metadata(path)?;
            #[cfg(unix)]
            let private = {
                use std::os::unix::fs::PermissionsExt as _;
                meta.permissions().mode() & 0o077 == 0
            };
            #[cfg(not(unix))]
            let private = true;
            if meta.is_file() && private && fs::read(path)? == content.as_bytes() {
                Ok(())
            } else {
                Err(io::Error::other("a different file is already there"))
            }
        }
        Err(e) => Err(e),
    }
}

/// Emits the trim's events.
fn report(
    ccx: &ContextCx,
    record: &Record,
    before_view: &[Message],
    view: &[Message],
    before: u64,
    after: u64,
) {
    let count = |kind| record.edits.iter().filter(|e| e.kind == kind).count();
    let thinking = |v: &[Message]| {
        v.iter()
            .flat_map(|m| &m.content)
            .filter(|b| is_thinking(b))
            .count()
    };
    ccx.cx.emit(AgentEvent::ContextCompacted {
        kind: CompactionKind::Elided,
        tokens_before: before,
        tokens_after: after,
    });
    ccx.cx.emit_custom(
        ID,
        "compaction",
        json!({
            "trigger": record.trigger,
            "results_elided": count(EditKind::Result),
            "inputs_shortened": count(EditKind::Input),
            "reminders_elided": count(EditKind::Reminder),
            "superseded_calls": count(EditKind::Superseded),
            "thinking_dropped": thinking(before_view).saturating_sub(thinking(view)),
            "first_edited_event": record.edits[0].event,
            "at_event": record.at_event,
            "tokens_before": before,
            "tokens_after": after,
        }),
    );
}

/// `/compact`'s answer.
fn describe(t: &Trimmed) -> String {
    let count = |kind| t.record.edits.iter().filter(|e| e.kind == kind).count();
    let parts: Vec<String> = [
        (EditKind::Result, "tool result"),
        (EditKind::Input, "tool input"),
        (EditKind::Reminder, "reminder"),
        (EditKind::Superseded, "superseded call"),
    ]
    .into_iter()
    .filter_map(|(kind, name)| match count(kind) {
        0 => None,
        1 => Some(format!("1 {name}")),
        n => Some(format!("{n} {name}s")),
    })
    .collect();
    let list = match parts.as_slice() {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    };
    format!(
        "Trimmed {list}: {} → {} tokens.",
        thousands(t.before),
        thousands(t.after)
    )
}

fn warn(ccx: &ContextCx, message: String) {
    tracing::warn!("{message}");
    ccx.cx.emit(AgentEvent::Warning { message });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_copy_that_fails_part_way_leaves_no_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("3-0.txt");
        let half = |f: &mut fs::File, bytes: &[u8]| {
            f.write_all(&bytes[..bytes.len() / 2])?;
            Err(io::Error::other("disk full"))
        };
        assert!(save_original(&path, "a long result", &half).is_err());
        assert!(!path.exists());
        save_original(&path, "a long result", &|f, b| f.write_all(b)).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "a long result");
    }

    #[cfg(unix)]
    #[test]
    fn a_copy_left_behind_is_reused_only_if_it_is_private_and_the_same() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("3-0.txt");
        let write = |f: &mut fs::File, b: &[u8]| f.write_all(b);
        save_original(&path, "text", &write).unwrap();
        assert!(save_original(&path, "text", &write).is_ok());
        assert!(save_original(&path, "other", &write).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(save_original(&path, "text", &write).is_err());
    }
}
