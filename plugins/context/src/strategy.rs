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
    /// After a summary that wasn't made: the turn, and the size of the view then.
    backoff: Option<(u32, u64)>,
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
    /// Nothing to do: why.
    Nothing(String),
    /// A summary that wasn't written, or wasn't worth keeping: why.
    Failed(String),
    /// Decided, but it couldn't be saved, so it wasn't made.
    NotSaved(String),
}

impl Step {
    /// What `/compact` says about it.
    fn said(self, what: &str) -> String {
        match self {
            Step::Made(made) => made.message,
            Step::Nothing(why) => why,
            Step::Failed(why) => format!("no summary was written: {why}"),
            Step::NotSaved(why) => {
                format!("the {what} couldn't be saved, so it wasn't made: {why}")
            }
        }
    }
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
                backoff: None,
            };
            for record in fresh.records {
                session.add(record);
            }
            *slot = Some(session);
        }
        loaded(&slot).expect("just loaded")
    }

    /// Runs `f` on `ccx`'s session log, if it is the one loaded.
    fn with_session<T>(&self, ccx: &ContextCx, f: impl FnOnce(&mut SessionLog) -> T) -> Option<T> {
        (self.shared.lock().as_mut())
            .filter(|s| s.id == *ccx.cx.session_id)
            .map(f)
    }

    fn freeze(&self, ccx: &ContextCx, why: String) {
        warn(ccx, format!("no more compactions in this session: {why}"));
        self.with_session(ccx, |s| s.frozen = Some(why));
    }

    /// After a summary that wasn't made (`Some` with the size of the view then), no
    /// summary is tried above `hard` until the view grows by `min_free` or the next turn
    /// starts: each try is a paid call. `None` after one that was.
    pub(crate) fn back_off(&self, ccx: &ContextCx, tokens: Option<u64>) {
        let turn = ccx.cx.turn;
        self.with_session(ccx, |s| s.backoff = tokens.map(|t| (turn, t)));
    }

    fn backed_off(&self, ccx: &ContextCx, tokens: u64, budget: &Budget) -> bool {
        let grown = self.config.min_free * budget.usable() as f64;
        self.with_session(ccx, |s| s.backoff)
            .flatten()
            .is_some_and(|(turn, then)| {
                turn == ccx.cx.turn && (tokens as f64) < then as f64 + grown
            })
    }

    /// Saves `record`: with a session directory, appended to the log and made durable
    /// before it takes effect; then kept for the session. Refused once the session is
    /// frozen.
    pub(crate) async fn persist(&self, ccx: &ContextCx, record: &Record) -> Result<(), String> {
        if let Some(why) = self.with_session(ccx, |s| s.frozen.clone()).flatten() {
            return Err(format!("compactions are off for this session: {why}"));
        }
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
        self.with_session(ccx, |s| s.add(record.clone()));
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
        let Loaded { records, frozen } = self.records(history, cx).await;
        let current = render(history, &records).view();
        let strategy = self.config.strategy;
        if frozen.is_some() || strategy == Strategy::None {
            return Ok(View { messages: current });
        }
        let usable = budget.usable() as f64;
        let size = budget.measure(&current);
        let trim = (matches!(strategy, Strategy::Elide | Strategy::Staged)
            && size as f64 > self.config.soft * usable)
            .then(|| self.decide(history, &records, &budget, cx, Trigger::Soft))
            .flatten();
        // A trim that leaves the view above `hard` isn't made: the summary replays the
        // view as it was sent, which the cache holds, and makes the trim pointless. It is
        // made if the summary isn't.
        let projected = trim.as_ref().map_or(size, |t| t.after);
        if matches!(strategy, Strategy::Summarize | Strategy::Staged)
            && projected as f64 > self.config.hard * usable
            && !self.backed_off(cx, size, &budget)
        {
            let step = self
                .summarize(history, &records, &budget, cx, Trigger::Hard, None)
                .await;
            if let Step::Made(made) = step {
                return Ok(View {
                    messages: made.view,
                });
            }
            if cx.cx.cancel.is_cancelled() {
                return Ok(View { messages: current });
            }
        }
        if let Some(trim) = trim
            && let Step::Made(made) = self.commit(history, &records, trim, &budget, cx).await
        {
            return Ok(View {
                messages: made.view,
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
        let mut steps = Vec::new();
        if trims {
            let step = self.trim(history, &records, &budget, cx, trigger).await;
            if let Step::Made(made) = &step {
                records.extend(made.records.iter().cloned());
            }
            steps.push((step, "trim"));
        }
        if summarizes && !cx.cx.cancel.is_cancelled() {
            let step = self
                .summarize(history, &records, &budget, cx, trigger, instructions)
                .await;
            steps.push((step, "summary"));
        }
        let changed = steps.iter().any(|(s, _)| matches!(s, Step::Made(_)));
        // After a change, "nothing to do" for the other step goes without saying.
        let said: Vec<String> = (steps.into_iter())
            .filter(|(s, _)| !(changed && matches!(s, Step::Nothing(_))))
            .map(|(s, what)| s.said(what))
            .collect();
        let message = match changed {
            true => said
                .iter()
                .map(|s| sentence(s))
                .collect::<Vec<_>>()
                .join(" "),
            false => said.join("; "),
        };
        Ok(CompactOutcome { changed, message })
    }
}

/// `s` as a sentence: capitalized, with a full stop.
fn sentence(s: &str) -> String {
    let mut chars = s.chars();
    let mut out: String = chars
        .next()
        .into_iter()
        .flat_map(char::to_uppercase)
        .collect();
    out.push_str(chars.as_str());
    if !out.ends_with('.') {
        out.push('.');
    }
    out
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

#[cfg(test)]
mod tests {
    use ch_kernel::{Cx, ModelRequest, UsageLog};
    use ch_provider_scripted::ScriptedProvider;

    use super::*;

    #[tokio::test]
    async fn once_frozen_nothing_more_is_saved() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cx = Cx::detached("/ws");
        cx.session_dir = Some(Arc::new(tmp.path().join("s")));
        let ccx = ContextCx {
            cx,
            provider: Arc::new(ScriptedProvider::new(Vec::new())),
            agent_request: ModelRequest {
                model: "m".into(),
                system: Vec::new(),
                messages: Vec::new(),
                tools: Vec::new(),
                max_tokens: 100,
                temperature: None,
                thinking_budget: None,
                purpose: "agent".into(),
            },
            summarizer: None,
            usage: UsageLog::default(),
        };
        let mut history = History::new();
        history.push(1, Message::user("task"));
        history.push(1, Message::assistant("done"));
        let context = Context::new(ContextConfig::default());
        context.records(&history, &ccx).await;
        let record = Record::reset(&history).unwrap();
        context.persist(&ccx, &record).await.unwrap();
        // A trim whose record couldn't be taken back out freezes the session; a summary
        // decided in the same view must not be saved after it.
        context.freeze(&ccx, "the log may keep part of a record".into());
        let refused = context.persist(&ccx, &record).await.unwrap_err();
        assert!(
            refused.starts_with("compactions are off for this session"),
            "{refused}"
        );
        let log = std::fs::read_to_string(tmp.path().join("s/context/log.jsonl")).unwrap();
        assert_eq!(log.lines().count(), 1);
    }
}
