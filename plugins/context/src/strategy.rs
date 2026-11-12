//! The `ContextStrategy`: one session's compaction log, the trim decision, and saving a
//! trim.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use ch_kernel::reexports::async_trait;
use ch_kernel::util::anchored;
use ch_kernel::{
    AgentEvent, Budget, CompactOutcome, CompactReason, CompactRequest, CompactionKind, ContextCx,
    ContextStrategy, History, Message, View,
};
use serde_json::json;

use crate::config::{ContextConfig, Strategy};
use crate::log::{self, Edit, EditKind, Pos, Record, Replacement, Trigger};
use crate::plan::{self, Files, Original, Reach};
use crate::render::{is_thinking, render};
use crate::stub::{self, Hint, Saved};

/// Strategy id, also the plugin id.
pub const ID: &str = "context";

/// The `context` strategy.
pub struct Context {
    config: ContextConfig,
    /// The valid records of one session at a time.
    session: Mutex<Option<SessionLog>>,
}

struct SessionLog {
    id: String,
    records: Vec<Record>,
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
            session: Mutex::default(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Option<SessionLog>> {
        // Every change is one push or assignment, so a poisoned lock is still consistent.
        self.session
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The records of `ccx`'s session. They are read from its directory, and checked
    /// against `history`, when the session changes (`/clear`, `/resume`, a new process).
    fn records(&self, history: &History, ccx: &ContextCx) -> Vec<Record> {
        let mut slot = self.lock();
        let id = &*ccx.cx.session_id;
        if slot.as_ref().is_none_or(|s| s.id != id) {
            *slot = Some(SessionLog {
                id: id.to_string(),
                records: load(history, ccx),
            });
        }
        slot.as_ref().map(|s| s.records.clone()).unwrap_or_default()
    }

    /// Plans a trim of `current` and, if it is worth making, saves it and returns the
    /// trimmed view.
    async fn trim(
        &self,
        history: &History,
        mut records: Vec<Record>,
        current: &[Message],
        budget: &Budget,
        ccx: &ContextCx,
        trigger: Trigger,
    ) -> Option<Trimmed> {
        let at_event = history.last()?.id;
        let session_dir = ccx.cx.session_dir.as_deref().map(PathBuf::as_path);
        let files = Files {
            base: session_dir.and_then(|dir| {
                fs::create_dir_all(dir)
                    .and_then(|()| anchored(dir))
                    .inspect_err(|e| warn(ccx, format!("can't use {}: {e}", dir.display())))
                    .ok()
            }),
            hint: Hint::for_tools(&ccx.cx),
        };
        let taken: HashSet<Pos> = records
            .iter()
            .flat_map(|r| r.edits.iter().map(Edit::pos))
            .collect();
        let reach = match trigger {
            Trigger::Soft => Reach::Soft,
            Trigger::Manual => Reach::Manual,
            Trigger::Overflow => Reach::Overflow,
        };
        let plan = plan::plan(
            history,
            current,
            &taken,
            budget,
            &self.config,
            reach,
            &files,
        );
        if plan.edits.is_empty() {
            return None;
        }
        let before = budget.measure(current);
        records.push(Record {
            kind: CompactionKind::Elided,
            trigger,
            at_event,
            edits: plan.edits,
        });
        let mut view = render(history, &records);
        let freed = before.saturating_sub(budget.measure(&view));
        if trigger == Trigger::Soft
            && (freed as f64) < self.config.min_free * budget.usable() as f64
        {
            return None;
        }

        // The copies first: one left by a crash before its record is reused.
        let failed = match session_dir {
            Some(dir) => save_originals(dir.to_path_buf(), plan.originals).await,
            None => Vec::new(),
        };
        let record = records.last_mut().expect("just pushed");
        for (original, why) in &failed {
            warn(
                ccx,
                format!("couldn't save {}: {why}", original.path.display()),
            );
            if let Some(edit) = record.edits.iter_mut().find(|e| e.pos() == original.pos) {
                edit.replacement =
                    Replacement::Text(stub::result(&original.content, Saved::Nowhere, files.hint));
            }
        }
        let record = record.clone();
        if !failed.is_empty() {
            view = render(history, &records);
        }
        if let Some(dir) = session_dir {
            let (dir, line) = (dir.to_path_buf(), record.clone());
            let appended = tokio::task::spawn_blocking(move || log::append(&dir, &line)).await;
            if let Err(e) = appended.map_err(std::io::Error::other).and_then(|r| r) {
                warn(
                    ccx,
                    format!(
                        "couldn't save the compaction log; a resumed session will send the \
                         untrimmed history: {e}"
                    ),
                );
            }
        }
        if let Some(s) = self.lock().as_mut().filter(|s| s.id == *ccx.cx.session_id) {
            s.records.push(record.clone());
        }

        let after = budget.measure(&view);
        report(ccx, &record, current, &view, before, after);
        Some(Trimmed {
            view,
            record,
            before,
            after,
        })
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

/// The session's records that still match `history`, in order. The others are ignored,
/// with a warning: their events are missing (the session was saved before the log) or
/// their blocks changed.
fn load(history: &History, ccx: &ContextCx) -> Vec<Record> {
    let Some(dir) = ccx.cx.session_dir.as_deref() else {
        return Vec::new();
    };
    let mut taken = HashSet::new();
    let mut records = Vec::new();
    for (i, loaded) in log::load(dir).into_iter().enumerate() {
        match loaded.and_then(|r| r.check(history, &taken).map(|()| r)) {
            Ok(record) => {
                taken.extend(record.edits.iter().map(Edit::pos));
                records.push(record);
            }
            Err(why) => warn(
                ccx,
                format!(
                    "ignoring compaction record {} of this session: {why}",
                    i + 1
                ),
            ),
        }
    }
    records
}

/// Saves the originals of trimmed results off the async runtime: they can be large.
/// Returns those that couldn't be saved, and why.
async fn save_originals(session_dir: PathBuf, originals: Vec<Original>) -> Vec<(Original, String)> {
    if originals.is_empty() {
        return Vec::new();
    }
    let task = tokio::task::spawn_blocking(move || {
        let dir = match log::private_dir(&session_dir, "context/elided") {
            Ok(dir) => dir,
            Err(e) => {
                let why = e.to_string();
                return originals.into_iter().map(|o| (o, why.clone())).collect();
            }
        };
        originals
            .into_iter()
            .filter_map(|o| {
                let name = o.path.file_name().map(|n| dir.join(n))?;
                save_original(&name, &o.content)
                    .err()
                    .map(|e| (o, e.to_string()))
            })
            .collect()
    });
    task.await.unwrap_or_default()
}

/// Writes a new file readable only by the user. One already there (from a trim that
/// crashed before its record) is kept if it holds the same text.
fn save_original(path: &Path, content: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    match ch_kernel::util::create_private(path) {
        Ok(mut file) => file.write_all(content.as_bytes()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let same =
                fs::symlink_metadata(path)?.is_file() && fs::read(path)? == content.as_bytes();
            if same {
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
        stub::thousands(t.before as usize),
        stub::thousands(t.after as usize)
    )
}

fn warn(ccx: &ContextCx, message: String) {
    tracing::warn!("{message}");
    ccx.cx.emit(AgentEvent::Warning { message });
}
