//! Writing a summary: the call, the record, and merging the two oldest summaries when
//! they grow too large.

use std::sync::Arc;

use ch_kernel::{
    AgentEvent, Budget, CompactionKind, ContextCx, History, Message, ModelError, ModelProvider,
    ModelRequest, estimate_tokens,
};
use serde_json::json;

use crate::log::{Applied, Record, Trigger};
use crate::plan;
use crate::render::render;
use crate::strategy::{Context, ID, Made, Step, thinking_count, warn};
use crate::summary::{self, Summary};

impl Context {
    /// Replaces the middle after the earlier summaries, up to the recent window, with a
    /// summary appended to the first message. Above `hard` the window is the usual one;
    /// on `/compact` and an overflow it holds only `min_recent_steps` steps.
    pub(crate) async fn summarize(
        &self,
        history: &History,
        records: &[Record],
        budget: &Budget,
        ccx: &ContextCx,
        trigger: Trigger,
        instructions: Option<&str>,
    ) -> Step {
        let rendered = render(history, records);
        let short = trigger != Trigger::Hard;
        let Some(span) = plan::summary_span(&rendered, budget, &self.config, short) else {
            return Step::Nothing;
        };
        let view = rendered.view();
        let before = budget.measure(&view);
        let prompt = summary::prompt(self.config.summary_max_words, instructions);
        let (provider, model, request) = match &ccx.summarizer {
            Some((provider, model)) => {
                let request = summary::transcript(
                    model,
                    &ccx.agent_request,
                    &rendered.messages[0],
                    &rendered.messages[span.clone()],
                    prompt,
                );
                (provider.clone(), model.clone(), request)
            }
            None => {
                let request = summary::replay(&ccx.agent_request, &view, prompt);
                let model = ccx.agent_request.model.clone();
                (ccx.provider.clone(), model, request)
            }
        };
        let text = match ask(ccx, &provider, request, budget).await {
            Ok(text) => text,
            Err(step) => return step,
        };
        let events = history.events();
        let facts = summary::facts(history, span.clone(), &self.config.superseded_tools);
        let summary = Summary {
            n: Applied::of(records).last_n + 1,
            from: events[span.start].id,
            to: events[span.end - 1].id,
            covers: facts.covers,
            text,
            users: facts.users,
            calls: facts.calls,
            model,
            merges: 0,
        };
        let Some(record) = Record::summarized(trigger, history, summary) else {
            return Step::Nothing;
        };
        if let Err(why) = self.persist(ccx, &record).await {
            return Step::NotSaved(why);
        }
        let mut all = records.to_vec();
        all.push(record.clone());
        let mut made = vec![record];
        if let Some(merge) = self.merge(history, &all, budget, ccx, trigger).await {
            all.push(merge.clone());
            made.push(merge);
        }
        let after_view = render(history, &all).view();
        let after = budget.measure(&after_view);
        let merged = made.len() > 1;
        report(ccx, &made[0], &view, &after_view, before, after, merged);
        let covers = &made[0].summary.as_ref().expect("a summary").covers;
        let mut message = format!(
            "Summarized {covers}: {} → {} tokens.",
            short_tokens(before),
            short_tokens(after)
        );
        if merged {
            message.push_str(" The two oldest summaries were merged into one.");
        }
        Step::Made(Made {
            message,
            view: after_view,
            records: made,
        })
    }

    /// When the summaries pass `max_summary_share`, merges the two oldest into one with
    /// the same prompt: the only rewrite. `None` when it isn't needed or fails.
    async fn merge(
        &self,
        history: &History,
        records: &[Record],
        budget: &Budget,
        ccx: &ContextCx,
        trigger: Trigger,
    ) -> Option<Record> {
        let applied = Applied::of(records);
        let [older, newer, ..] = applied.summaries.as_slice() else {
            return None;
        };
        let chars: usize = applied.summaries.iter().map(|s| s.block().len()).sum();
        let share = budget.calibrator.calibrate(estimate_tokens(chars)) as f64;
        if share <= self.config.max_summary_share * budget.usable() as f64 {
            return None;
        }
        let (provider, model) = match &ccx.summarizer {
            Some((provider, model)) => (provider.clone(), model.clone()),
            None => (ccx.provider.clone(), ccx.agent_request.model.clone()),
        };
        let request = summary::merge(
            &model,
            &ccx.agent_request,
            older,
            newer,
            self.config.summary_max_words,
        );
        let text = ask(ccx, &provider, request, budget).await.ok()?;
        let merged = Summary::merged(older, newer, text, model);
        let record = Record::summarized(trigger, history, merged)?;
        self.persist(ccx, &record).await.ok()?;
        Some(record)
    }
}

/// The summary a model writes for `request`; a failure is warned about and kept out of
/// the log.
async fn ask(
    ccx: &ContextCx,
    provider: &Arc<dyn ModelProvider>,
    request: ModelRequest,
    budget: &Budget,
) -> Result<String, Step> {
    match ccx.complete(&**provider, request, budget).await {
        Ok(response) => summary::reply_text(&response).ok_or_else(|| {
            let why = "the model wrote no summary".to_string();
            warn(ccx, format!("{why}; the conversation is kept as it was"));
            Step::Failed(why)
        }),
        Err(ModelError::Cancelled) => Err(Step::Failed("cancelled".into())),
        Err(e) => {
            let why = format!("writing a summary failed: {e}");
            warn(ccx, format!("{why}; the conversation is kept as it was"));
            Err(Step::Failed(why))
        }
    }
}

/// `412k`.
fn short_tokens(n: u64) -> String {
    if n < 1000 {
        n.to_string()
    } else {
        format!("{}k", (n + 500) / 1000)
    }
}

/// Emits the summary's events.
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
            "covers": s.covers,
            "from_event": s.from,
            "to_event": s.to,
            "summary_chars": s.text.chars().count(),
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
