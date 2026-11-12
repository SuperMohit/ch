//! The `ContextStrategy`: one session's compaction log, and what each strategy does on a
//! view, on `/compact` and on an overflow. Trims are in `trim.rs`, summaries in
//! `summarize.rs`.

use std::collections::HashSet;
use std::io;
use std::sync::{Arc, Mutex, MutexGuard};

use ch_kernel::reexports::async_trait;
use ch_kernel::{
    AgentEvent, Budget, CompactOutcome, CompactReason, CompactRequest, ContextCx, ContextStrategy,
    History, Message, View,
};

use crate::config::{ContextConfig, Strategy};
use crate::hook::MarkerHook;
use crate::load::load;
use crate::log::{self, EditKind, Record, Replacement, Trigger};
use crate::render::{is_thinking, render};
use crate::stub;

/// Strategy id, also the plugin id.
pub const ID: &str = "context";

/// The `context` strategy.
pub struct Context {
    pub(crate) config: ContextConfig,
    pub(crate) shared: Arc<Shared>,
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
    /// Why no more compactions are made in this session: its log may hold what wasn't
    /// sent.
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

/// A session's records as loaded, and whether compactions must stop.
pub(crate) struct Loaded {
    pub(crate) records: Vec<Record>,
    pub(crate) frozen: Option<String>,
}

/// What came of a trim or a summary.
pub(crate) enum Step {
    Made(Made),
    /// Nothing to do, or not worth it.
    Nothing,
    /// A summary that couldn't be written; the conversation is kept as it was.
    Failed(String),
    /// Decided, but it couldn't be saved, so it wasn't made.
    NotSaved(String),
}

/// A trim or summary that was made: its records, the new view, and what `/compact`
/// says about it.
pub(crate) struct Made {
    pub(crate) records: Vec<Record>,
    pub(crate) view: Vec<Message>,
    pub(crate) message: String,
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

    /// The records of `ccx`'s session, and why compactions are off if they are. They
    /// are read from its directory, and checked against `history`, when the session
    /// changes (`/clear`, `/resume`, a new process); the lock isn't held while reading.
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
        warn(ccx, format!("no more compactions in this session: {why}"));
        if let Some(s) = (self.shared.lock().as_mut()).filter(|s| s.id == *ccx.cx.session_id) {
            s.frozen = Some(why);
        }
    }

    /// Saves `record`: with a session directory, appended to the log and made durable
    /// before it takes effect; then kept for the session.
    pub(crate) async fn persist(&self, ccx: &ContextCx, record: &Record) -> Result<(), String> {
        if let Some(dir) = ccx.cx.session_dir.as_deref() {
            let (dir, line) = (dir.clone(), record.clone());
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
                warn(ccx, format!("a compaction wasn't made: {why}"));
                if !failed.intact {
                    self.freeze(
                        ccx,
                        "the compaction log may keep part of a record that wasn't sent".into(),
                    );
                }
                return Err(why);
            }
        }
        if let Some(s) = (self.shared.lock().as_mut()).filter(|s| s.id == *ccx.cx.session_id) {
            s.add(record.clone());
        }
        Ok(())
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
        // Under `none` too: a session compacted earlier must keep rendering the same bytes.
        let Loaded {
            mut records,
            frozen,
        } = self.records(history, cx).await;
        let mut current = render(history, &records).view();
        let usable = budget.usable() as f64;
        let over = |share: f64, view: &[Message]| budget.measure(view) as f64 > share * usable;
        let strategy = self.config.strategy;
        if frozen.is_some() || strategy == Strategy::None {
            return Ok(View { messages: current });
        }
        if matches!(strategy, Strategy::Elide | Strategy::Staged)
            && over(self.config.soft, &current)
            && let Step::Made(made) = self
                .trim(history, &records, &budget, cx, Trigger::Soft)
                .await
        {
            records.extend(made.records);
            current = made.view;
        }
        if matches!(strategy, Strategy::Summarize | Strategy::Staged)
            && over(self.config.hard, &current)
            && let Step::Made(made) = self
                .summarize(history, &records, &budget, cx, Trigger::Hard, None)
                .await
        {
            current = made.view;
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
        let strategy = self.config.strategy;
        if strategy == Strategy::None {
            return Ok(CompactOutcome {
                changed: false,
                message: "context management is off (plugins.context.strategy = \"none\")".into(),
            });
        }
        let Loaded {
            mut records,
            frozen,
        } = self.records(history, cx).await;
        if let Some(why) = frozen {
            return Ok(CompactOutcome {
                changed: false,
                message: format!("compactions are off for this session: {why}"),
            });
        }
        let (trigger, instructions) = match request.reason {
            CompactReason::Manual => (Trigger::Manual, request.instructions.as_deref()),
            CompactReason::Overflow => (Trigger::Overflow, None),
        };
        // `/compact` summarizes; an overflow trims first, then summarizes (`staged`).
        let trims = match strategy {
            Strategy::Elide => true,
            Strategy::Staged => trigger == Trigger::Overflow,
            _ => false,
        };
        let summarizes = matches!(strategy, Strategy::Summarize | Strategy::Staged);
        let mut said = Vec::new();
        let mut changed = false;
        if trims {
            match self.trim(history, &records, &budget, cx, trigger).await {
                Step::Made(made) => {
                    records.extend(made.records);
                    said.push(made.message);
                    changed = true;
                }
                other => said.push(why_not(other, "trim")),
            }
        }
        if summarizes {
            match self
                .summarize(history, &records, &budget, cx, trigger, instructions)
                .await
            {
                Step::Made(made) => {
                    said.push(made.message);
                    changed = true;
                }
                other => said.push(why_not(other, "summarize")),
            }
        }
        let message = if changed {
            said.retain(|m| !m.starts_with("nothing to"));
            said.join(" ")
        } else {
            said.join("; ")
        };
        Ok(CompactOutcome { changed, message })
    }
}

/// What `/compact` says when a step made nothing.
fn why_not(step: Step, what: &str) -> String {
    match step {
        Step::Made(made) => made.message,
        Step::Nothing => format!("nothing to {what} before the last steps"),
        Step::Failed(why) => format!("the summary failed: {why}"),
        Step::NotSaved(why) => format!("the {what} couldn't be saved, so it wasn't made: {why}"),
    }
}

/// Thinking blocks in `view`.
pub(crate) fn thinking_count(view: &[Message]) -> usize {
    view.iter()
        .flat_map(|m| &m.content)
        .filter(|b| is_thinking(b))
        .count()
}

pub(crate) fn warn(ccx: &ContextCx, message: String) {
    tracing::warn!("{message}");
    ccx.cx.emit(AgentEvent::Warning { message });
}
