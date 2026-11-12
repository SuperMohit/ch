//! The `ContextStrategy`: one session's compaction log, the trim decision, and saving a
//! trim.

use std::collections::HashSet;
use std::fs;
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
use crate::log::{self, Edit, EditKind, Kind, Pos, Record, Replacement, Trigger};
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
/// paths and sizes) and input markers.
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

    /// The records of `ccx`'s session. They are read from its directory, and checked
    /// against `history`, when the session changes (`/clear`, `/resume`, a new process).
    fn records(&self, history: &History, ccx: &ContextCx) -> Vec<Record> {
        let mut slot = self.shared.lock();
        let id = &*ccx.cx.session_id;
        if Shared::of(&slot, id).is_none() {
            let mut session = SessionLog {
                id: id.to_string(),
                records: Vec::new(),
                written: HashSet::new(),
            };
            for record in load(history, ccx) {
                session.add(record);
            }
            *slot = Some(session);
        }
        Shared::of(&slot, id)
            .map(|s| s.records.clone())
            .unwrap_or_default()
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
        let record = Record::new(Kind::Elided, trigger, history, plan.edits)?;
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
            tools: files.tools,
            before,
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
    ) -> Option<Trimmed> {
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
            let appended = tokio::task::spawn_blocking(move || log::append(&dir, &line)).await;
            if let Err(e) = appended.map_err(std::io::Error::other).and_then(|r| r) {
                warn(
                    ccx,
                    format!("couldn't save a trim to the compaction log, so it wasn't made: {e}"),
                );
                return None;
            }
        }
        if let Some(s) = self
            .shared
            .lock()
            .as_mut()
            .filter(|s| s.id == *ccx.cx.session_id)
        {
            s.add(record.clone());
        }
        records.push(record.clone());
        let view = render(history, &records);
        let after = budget.measure(&view);
        report(ccx, &record, current, &view, before, after);
        Some(Trimmed {
            view,
            record,
            before,
            after,
        })
    }

    async fn trim(
        &self,
        history: &History,
        records: Vec<Record>,
        current: &[Message],
        budget: &Budget,
        ccx: &ContextCx,
        trigger: Trigger,
    ) -> Option<Trimmed> {
        let files = Files {
            base: ccx.cx.session_dir.as_deref().and_then(|d| anchor(d)),
            tools: ReadTools::of(&ccx.cx),
        };
        let decision = self.decide(history, &records, current, budget, trigger, &files)?;
        self.commit(history, records, decision, current, budget, ccx)
            .await
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
        let records = self.records(history, cx);
        let current = render(history, &records);
        let over = budget.measure(&current) as f64 > self.config.soft * budget.usable() as f64;
        if self.config.strategy == Strategy::Elide
            && over
            && let Some(trimmed) = self
                .trim(history, records, &current, &budget, cx, Trigger::Soft)
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
        let records = self.records(history, cx);
        let current = render(history, &records);
        let trigger = match request.reason {
            CompactReason::Manual => Trigger::Manual,
            CompactReason::Overflow => Trigger::Overflow,
        };
        Ok(
            match self
                .trim(history, records, &current, &budget, cx, trigger)
                .await
            {
                Some(t) => CompactOutcome {
                    changed: true,
                    message: describe(&t),
                },
                None => CompactOutcome {
                    changed: false,
                    message: "nothing to trim: no tool result, tool input or reminder before \
                              the last steps is long enough"
                        .into(),
                },
            },
        )
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
/// - A record decided on events that the history doesn't have (it was saved before a
///   crash, after the log), or on events the model never answered after, affects no
///   reply in the history: it and the records after it are dropped from the log.
/// - Any other record that doesn't match means a view that was sent can't be rendered
///   again. The records from it on are dropped and a reset record takes their place:
///   every thinking block so far is left out, which the API accepts, and the log keeps
///   that so later requests repeat it.
fn load(history: &History, ccx: &ContextCx) -> Vec<Record> {
    let Some(dir) = ccx.cx.session_dir.as_deref() else {
        return Vec::new();
    };
    let (lines, unreadable) = match log::load(dir) {
        Ok(lines) => (lines, false),
        Err(why) => (vec![Err(why)], true),
    };
    let count = lines.len();
    let mut taken = HashSet::new();
    let mut records = Vec::new();
    let mut cut = None;
    for (i, line) in lines.into_iter().enumerate() {
        let (record, why) = match line {
            Err(why) => (None, why),
            Ok(record) => match record.check(history, &taken) {
                Ok(()) => {
                    taken.extend(record.edits.iter().map(Edit::pos));
                    records.push(record);
                    continue;
                }
                Err(why) => (Some(record), why),
            },
        };
        cut = Some((i, record, why));
        break;
    }
    let Some((i, record, why)) = cut else {
        return records;
    };
    // A line that doesn't parse is harmless only as the torn end of the log.
    let harmless = match &record {
        Some(record) => !log::answered_after(history, record.at_event),
        None => i + 1 == count && !unreadable,
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
    } else if let Some(reset) = Record::new(Kind::Reset, Trigger::Recovery, history, Vec::new()) {
        warn(
            ccx,
            format!(
                "the compaction log doesn't match this session (record {}: {why}); the \
                 model's earlier reasoning is left out from now on",
                i + 1
            ),
        );
        records.push(reset);
    }
    if let Err(e) = log::rewrite(dir, &records) {
        warn(ccx, format!("couldn't rewrite the compaction log: {e}"));
    }
    records
}

/// Saves the originals of trimmed results off the async runtime: they can be large.
/// Returns those that couldn't be saved, and why.
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
        originals
            .into_iter()
            .filter_map(|o| {
                let path = dir.join(o.path.file_name()?);
                save_original(&path, &o.content)
                    .err()
                    .map(|e| (o, e.to_string()))
            })
            .collect()
    });
    match task.await {
        Ok(failed) => failed,
        Err(e) => all.into_iter().map(|o| (o, e.to_string())).collect(),
    }
}

/// Writes a new file readable only by the user. One already there (from a trim that
/// crashed before its record) is kept if it is private and holds the same text.
fn save_original(path: &Path, content: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    match create_private(path) {
        Ok(mut file) => file.write_all(content.as_bytes()).inspect_err(|_| {
            drop(file);
            let _ = fs::remove_file(path);
        }),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
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
                Err(std::io::Error::other("a different file is already there"))
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
