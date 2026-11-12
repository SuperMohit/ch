//! Writing a summary: who writes it, the span fitted to their window, the checks on the
//! reply and on the view it gives, its record, and merging the two oldest summaries
//! when they grow too large.

use std::ops::Range;
use std::sync::Arc;

use ch_kernel::util::ReadTools;
use ch_kernel::{
    AgentEvent, Budget, CompactionKind, ContextCx, History, Message, ModelError, ModelProvider,
    ModelRequest, Role, estimate_tokens,
};
use serde_json::json;

use crate::log::{Applied, Record, Trigger};
use crate::plan::{self, Original};
use crate::render::{Rendered, render};
use crate::strategy::{Context, ID, Made, Step, thinking_count, warn};
use crate::stub;
use crate::summary::{self, Summary, USER_TEXT_MAX, UserText};
use crate::trim::{anchor, save_originals};

/// Requests per summary the provider may reject as too long; each halves the span.
const TOO_LONG_TRIES: usize = 3;

/// Summaries one `/compact` or overflow may write: each covers what the one before
/// left out because it didn't fit the summarizer.
const PIECES: usize = 4;

/// Who writes the summaries.
struct Writer {
    provider: Arc<dyn ModelProvider>,
    model: String,
    /// `[model.summarizer]`: it can't read the agent's cache, so it gets a transcript.
    separate: bool,
}

impl Writer {
    fn of(ccx: &ContextCx) -> Self {
        match &ccx.summarizer {
            Some((provider, model)) => Self {
                provider: provider.clone(),
                model: model.clone(),
                separate: true,
            },
            None => Self {
                provider: ccx.provider.clone(),
                model: ccx.agent_request.model.clone(),
                separate: false,
            },
        }
    }

    /// The request for a summary of `span`: the agent's request replayed up to it, or a
    /// transcript for a separate summarizer.
    fn request(
        &self,
        ccx: &ContextCx,
        rendered: &Rendered,
        span: Range<usize>,
        prompt: &str,
    ) -> ModelRequest {
        let (first, span) = (&rendered.messages[0], &rendered.messages[span]);
        if self.separate {
            summary::transcript(&self.model, &ccx.agent_request, first, span, prompt.into())
        } else {
            summary::replay(&ccx.agent_request, first, span, prompt.into())
        }
    }

    /// Whether `request` fits the window it goes to, as far as the estimate tells: the
    /// agent's usable window, or the separate summarizer's window when its provider
    /// knows it.
    fn fits(&self, request: &ModelRequest, budget: &Budget) -> bool {
        if !self.separate {
            return budget.measure(&request.messages) <= budget.usable();
        }
        let Some(window) = self.provider.context_window(&self.model) else {
            return true;
        };
        let chars = request.system.iter().map(String::len).sum::<usize>()
            + request
                .messages
                .iter()
                .map(Message::char_len)
                .sum::<usize>();
        budget.calibrator.calibrate(estimate_tokens(chars)) + u64::from(request.max_tokens)
            <= window
    }
}

/// Why a summary call failed.
enum Fail {
    /// The provider rejected the request as too long.
    TooLong(String),
    Cancelled,
    Other(String),
}

impl Context {
    /// Replaces the middle after the earlier summaries, up to the recent window, with a
    /// summary appended to the first message. Above `hard` the window is the usual one;
    /// on `/compact` and an overflow it holds only `min_recent_steps` steps, and when
    /// the span doesn't fit the summarizer, its parts are summarized one after another.
    /// A failure stops automatic summaries for a while.
    pub(crate) async fn summarize(
        &self,
        history: &History,
        records: &[Record],
        budget: &Budget,
        ccx: &ContextCx,
        trigger: Trigger,
        instructions: Option<&str>,
    ) -> Step {
        let mut all = records.to_vec();
        let mut made = Vec::new();
        let mut said = Vec::new();
        for _ in 0..PIECES {
            let before_view = render(history, &all).view();
            let (record, whole) = match self
                .piece(history, &all, budget, ccx, trigger, instructions)
                .await
            {
                Ok(piece) => piece,
                Err(step) if made.is_empty() => {
                    if matches!(step, Step::Failed(_) | Step::NotSaved(_))
                        && !ccx.cx.cancel.is_cancelled()
                    {
                        self.back_off(ccx, Some(budget.measure(&before_view)));
                    }
                    return step;
                }
                // The earlier parts stand.
                Err(_) => break,
            };
            all.push(record.clone());
            made.push(record.clone());
            let merged = match self.merge(history, &all, budget, ccx, trigger).await {
                Ok(Some(merge)) => {
                    all.push(merge.clone());
                    made.push(merge);
                    true
                }
                Ok(None) => false,
                Err(why) => {
                    warn(
                        ccx,
                        format!(
                            "couldn't merge the two oldest summaries: {why}; the new summary was kept"
                        ),
                    );
                    false
                }
            };
            let view = render(history, &all).view();
            let (before, after) = (budget.measure(&before_view), budget.measure(&view));
            report(ccx, &record, &before_view, &view, before, after, merged);
            let s = record.summary.as_ref().expect("a summary record");
            said.push(format!(
                "Summarized {}: {} → {} tokens.",
                s.covers(),
                short_tokens(before),
                short_tokens(after)
            ));
            if merged {
                said.push("The two oldest summaries were merged into one.".into());
            }
            if whole || trigger == Trigger::Hard || ccx.cx.cancel.is_cancelled() {
                break;
            }
        }
        self.back_off(ccx, None);
        Step::Made(Made {
            message: said.join(" "),
            view: render(history, &all).view(),
            records: made,
        })
    }

    /// One summary, of the span or, when that doesn't fit the summarizer, of its first
    /// part; saved only if it makes the view smaller, and above `hard` only if by
    /// `min_free`. Also says whether it covers the whole span.
    async fn piece(
        &self,
        history: &History,
        records: &[Record],
        budget: &Budget,
        ccx: &ContextCx,
        trigger: Trigger,
        instructions: Option<&str>,
    ) -> Result<(Record, bool), Step> {
        let rendered = render(history, records);
        let short = trigger != Trigger::Hard;
        let Some(whole) = plan::summary_span(&rendered, budget, &self.config, short) else {
            return Err(Step::Nothing(
                "nothing to summarize before the last steps".into(),
            ));
        };
        let writer = Writer::of(ccx);
        let prompt = summary::prompt(self.config.summary_max_words, instructions);
        let (span, text) = write(ccx, &writer, &rendered, whole.clone(), &prompt, budget).await?;
        let events = history.events();
        let facts = summary::facts(
            history,
            span.clone(),
            &self.config.superseded_tools,
            events.len() - 1,
        );
        let summary = Summary {
            n: Applied::of(records).last_n + 1,
            from: events[span.start].id,
            to: events[span.end - 1].id,
            first: facts.first,
            last: facts.last,
            text: summary::cut(&text, self.config.summary_max_words * 3 / 2),
            users: keep_users(ccx, facts.users).await,
            calls: facts.calls,
            model: writer.model,
            merged: false,
        };
        let record = Record::summarized(trigger, history, summary);
        self.saves(history, records, &record, budget, ccx)?;
        self.persist(ccx, &record).await.map_err(Step::NotSaved)?;
        Ok((record, span.end == whole.end))
    }

    /// Whether `record` makes the view smaller: always required, and above `hard` by
    /// at least `min_free`, since it rewrites the cache from the first message on.
    fn saves(
        &self,
        history: &History,
        records: &[Record],
        record: &Record,
        budget: &Budget,
        ccx: &ContextCx,
    ) -> Result<(), Step> {
        let before = budget.measure(&render(history, records).view());
        let mut all = records.to_vec();
        all.push(record.clone());
        let after = budget.measure(&render(history, &all).view());
        let enough = match record.trigger {
            Trigger::Hard => self.config.min_free * budget.usable() as f64,
            _ => 0.0,
        };
        if after < before && (before - after) as f64 >= enough {
            return Ok(());
        }
        Err(failed(
            ccx,
            format!(
                "it would take the conversation from {} to {} tokens, too little to be worth it",
                short_tokens(before),
                short_tokens(after)
            ),
        ))
    }

    /// When the summaries' own text passes `max_summary_share`, merges the two oldest
    /// into one: the only rewrite. `Ok(None)` when it isn't needed; `Err` says why it
    /// failed.
    async fn merge(
        &self,
        history: &History,
        records: &[Record],
        budget: &Budget,
        ccx: &ContextCx,
        trigger: Trigger,
    ) -> Result<Option<Record>, String> {
        let applied = Applied::of(records);
        let [older, newer, ..] = applied.summaries.as_slice() else {
            return Ok(None);
        };
        let chars: usize = applied.summaries.iter().map(|s| s.text.len()).sum();
        let share = budget.calibrator.calibrate(estimate_tokens(chars)) as f64;
        if share <= self.config.max_summary_share * budget.usable() as f64 {
            return Ok(None);
        }
        let writer = Writer::of(ccx);
        let max_words = self.config.summary_max_words;
        let task = &history.events()[0].message;
        let request = summary::merge(
            &writer.model,
            &ccx.agent_request,
            task,
            older,
            newer,
            max_words,
        );
        if !writer.fits(&request, budget) {
            return Err("the two summaries don't fit the summarizer's window".into());
        }
        let text = match ask(ccx, &writer, request, budget).await {
            Ok(text) => text,
            Err(Fail::Cancelled) => return Err("cancelled".into()),
            Err(Fail::TooLong(why) | Fail::Other(why)) => return Err(why),
        };
        let text = summary::cut(&text, max_words * 3 / 2);
        let record = Record::summarized(
            trigger,
            history,
            Summary::merge(older, newer, text, writer.model),
        );
        // It goes out with the summary that set it off, which rewrote the cache already,
        // so any saving will do.
        let before = budget.measure(&render(history, records).view());
        let mut all = records.to_vec();
        all.push(record.clone());
        if budget.measure(&render(history, &all).view()) >= before {
            return Err("the merged summary isn't shorter than the two".into());
        }
        self.persist(ccx, &record).await?;
        Ok(Some(record))
    }
}

/// The summary of `span` or of its first part: the span is halved while the request
/// doesn't fit the summarizer's window, and when the provider rejects it as too long.
async fn write(
    ccx: &ContextCx,
    writer: &Writer,
    rendered: &Rendered,
    mut span: Range<usize>,
    prompt: &str,
    budget: &Budget,
) -> Result<(Range<usize>, String), Step> {
    let mut rejected = 0;
    loop {
        let request = writer.request(ccx, rendered, span.clone(), prompt);
        let shorter = halve(rendered, &span);
        if !writer.fits(&request, budget) {
            match shorter {
                Some(end) => span.end = end,
                None => {
                    let why = "a single step doesn't fit the summarizer's window";
                    return Err(failed(ccx, why.into()));
                }
            }
            continue;
        }
        match (ask(ccx, writer, request, budget).await, shorter) {
            (Ok(text), _) => return Ok((span, text)),
            (Err(Fail::TooLong(_)), Some(end)) if rejected < TOO_LONG_TRIES => {
                rejected += 1;
                span.end = end;
            }
            (Err(Fail::Cancelled), _) => return Err(Step::Failed("cancelled".into())),
            (Err(Fail::TooLong(why) | Fail::Other(why)), _) => return Err(failed(ccx, why)),
        }
    }
}

/// Where the first half of `span`'s steps ends; `None` for a single step. A step starts
/// at a reply, as the span does.
fn halve(rendered: &Rendered, span: &Range<usize>) -> Option<usize> {
    let later: Vec<usize> = (span.start + 1..span.end)
        .filter(|&i| rendered.messages[i].role == Role::Assistant)
        .collect();
    later.get(later.len().checked_sub(1)? / 2).copied()
}

/// The summary in a reply to `request`.
async fn ask(
    ccx: &ContextCx,
    writer: &Writer,
    request: ModelRequest,
    budget: &Budget,
) -> Result<String, Fail> {
    match ccx.complete(&*writer.provider, request, budget).await {
        Ok(response) => summary::reply_text(&response).map_err(Fail::Other),
        Err(ModelError::Cancelled) => Err(Fail::Cancelled),
        Err(ModelError::ContextOverflow(why)) => Err(Fail::TooLong(why)),
        Err(e) => Err(Fail::Other(e.to_string())),
    }
}

/// A summary that wasn't made, warned about.
fn failed(ccx: &ContextCx, why: String) -> Step {
    warn(ccx, format!("no summary was written: {why}"));
    Step::Failed(why)
}

/// The user's texts as a summary keeps them: up to [`USER_TEXT_MAX`] characters each.
/// A longer one is cut there, with a note naming a saved copy of all of it; without a
/// session directory, or when the copy can't be saved, it is kept whole.
async fn keep_users(ccx: &ContextCx, mut users: Vec<UserText>) -> Vec<UserText> {
    let (Some(dir), Some(base)) = (
        ccx.cx.session_dir.as_deref(),
        ccx.cx.session_dir.as_deref().and_then(|d| anchor(d)),
    ) else {
        return users;
    };
    let long: Vec<Original> = (users.iter())
        .filter(|u| u.text.chars().count() > USER_TEXT_MAX)
        .map(|u| Original {
            pos: (u.event, u.block),
            path: (base.join("context/elided")).join(format!("user-{}-{}.txt", u.event, u.block)),
            content: u.text.clone(),
        })
        .collect();
    if long.is_empty() {
        return users;
    }
    let failed = save_originals(dir.clone(), long.clone()).await;
    for (original, why) in &failed {
        warn(
            ccx,
            format!("couldn't save {}: {why}", original.path.display()),
        );
    }
    let tools = ReadTools::of(&ccx.cx);
    for user in &mut users {
        let pos = (user.event, user.block);
        if let Some(o) = long.iter().find(|o| o.pos == pos)
            && !failed.iter().any(|(f, _)| f.pos == pos)
        {
            user.text = stub::user_text(&user.text, USER_TEXT_MAX, &o.path, tools);
        }
    }
    users
}

/// `412k`.
fn short_tokens(n: u64) -> String {
    if n < 1000 {
        n.to_string()
    } else {
        format!("{}k", (n + 500) / 1000)
    }
}

/// Emits the summary's events: the text goes with them, so the user sees what the
/// model will read back.
fn report(
    ccx: &ContextCx,
    record: &Record,
    before_view: &[Message],
    view: &[Message],
    before: u64,
    after: u64,
    merged: bool,
) {
    let s = record.summary.as_ref().expect("a summary record");
    ccx.cx.emit(AgentEvent::ContextCompacted {
        kind: CompactionKind::Summarized,
        tokens_before: before,
        tokens_after: after,
    });
    ccx.cx.emit_custom(
        ID,
        "compaction",
        json!({
            "kind": "summarized",
            "trigger": record.trigger,
            "n": s.n,
            "covers": s.covers(),
            "from_event": s.from,
            "to_event": s.to,
            "text": s.text,
            "user_messages": s.users.len(),
            "latest_calls": s.calls.len(),
            "model": s.model,
            "merged": merged,
            "thinking_dropped": thinking_count(before_view).saturating_sub(thinking_count(view)),
            "at_event": record.at_event,
            "tokens_before": before,
            "tokens_after": after,
        }),
    );
}
