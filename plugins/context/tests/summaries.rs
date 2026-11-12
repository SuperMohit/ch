//! Summaries, called directly over hand-built histories: the invariants of ADR 0004
//! for summaries.

mod common;

use common::*;

fn with(strategy: Strategy) -> Context {
    Context::new(ContextConfig {
        strategy,
        ..Default::default()
    })
}

/// The text of every text block in `message`.
fn texts(message: &Message) -> Vec<String> {
    message
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

/// The summary blocks of a view's first message.
fn summary_blocks(view: &[Message]) -> Vec<String> {
    texts(&view[0])
        .into_iter()
        .filter(|t| t.starts_with("<context-summary"))
        .collect()
}

/// What holds for a view with summaries: the first message starts with the history's,
/// then the summaries; roles alternate; every call has its result; the last message is
/// the history's.
fn assert_summarized(history: &History, view: &[Message]) {
    let first = &history.messages()[0];
    assert_eq!(view[0].content[..first.content.len()], first.content[..]);
    assert!(
        view[0].content[first.content.len()..].iter().all(
            |b| matches!(b, ContentBlock::Text { text } if text.starts_with("<context-summary"))
        )
    );
    for (i, m) in view.iter().enumerate() {
        let role = if i % 2 == 0 {
            Role::User
        } else {
            Role::Assistant
        };
        assert_eq!(m.role, role, "message {i}");
    }
    for pair in view.windows(2) {
        if pair[0].role != Role::Assistant {
            continue;
        }
        let uses: Vec<_> = pair[0].tool_calls().into_iter().map(|c| c.id).collect();
        let results: Vec<_> = (pair[1].content.iter())
            .filter_map(|b| match b {
                ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(uses, results);
    }
    assert_eq!(view.last(), history.messages().last());
}

/// A window at which `history` is just above `share` of it.
fn window_above(history: &History, share: f64) -> u64 {
    (tokens(&history.messages()) as f64 / (share + 0.02)) as u64
}

#[tokio::test]
async fn above_hard_the_middle_becomes_a_summary_in_the_first_message() {
    let tmp = tempfile::tempdir().unwrap();
    let c = long_session();
    let p = Summarizer::new();
    let ccx = ccx_with(Some(tmp.path()), &p, None);
    let mut rx = ccx.cx.events.subscribe();
    let window = window_above(&c.history, 0.85);
    let v = view(&with(Strategy::Summarize), &c.history, window, &ccx).await;

    // One call, replaying the agent's request with the prompt last.
    let requests = p.requests();
    assert_eq!(requests.len(), 1);
    let r = &requests[0];
    let agent = &ccx.agent_request;
    assert_eq!(r.purpose, "context.summarize");
    assert_eq!(
        (&r.model, &r.system, &r.tools, r.thinking_budget),
        (
            &agent.model,
            &agent.system,
            &agent.tools,
            agent.thinking_budget
        )
    );
    let sent = c.history.messages();
    let n = sent.len();
    assert_eq!(r.messages[..n - 1], sent[..n - 1]);
    let last = texts(r.messages.last().unwrap());
    assert!(
        last.last()
            .unwrap()
            .starts_with("This conversation is close to the limit")
    );

    // The view: the task, its summary, then the recent window.
    assert_summarized(&c.history, &v);
    let blocks = summary_blocks(&v);
    assert_eq!(blocks.len(), 1);
    assert!(
        blocks[0].starts_with("<context-summary n=\"1\" covers=\"turn 1, steps 1–"),
        "{}",
        blocks[0]
    );
    assert!(blocks[0].contains("Summary 1 by m."));
    assert!(
        !blocks[0].contains("ignored"),
        "the reply's tool call is ignored"
    );
    assert_eq!(thinking_left(&v), 0, "a summary drops all earlier thinking");
    assert!(v.len() < n / 2);
    assert!(tokens(&v) as f64 <= 0.85 * window as f64);

    let events = received(&mut rx);
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::ContextCompacted {
            kind: ch_kernel::CompactionKind::Summarized,
            ..
        }
    )));
    let custom = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::Custom { data, .. } if data["kind"] == "summarized" => Some(data.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(custom["trigger"], "hard");
    assert_eq!(custom["n"], 1);
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::ModelRequest { purpose, .. } if purpose == "context.summarize"
    )));
}

#[tokio::test]
async fn no_summary_at_or_below_hard() {
    let tmp = tempfile::tempdir().unwrap();
    let c = long_session();
    let p = Summarizer::new();
    let ccx = ccx_with(Some(tmp.path()), &p, None);
    // `summarize`: just at `hard`.
    let window = (tokens(&c.history.messages()) as f64 / 0.85).ceil() as u64;
    let v = view(&with(Strategy::Summarize), &c.history, window, &ccx).await;
    assert_eq!(v, c.history.messages());
    // `staged`: above `soft`, and the trim brings it below `hard`.
    let v = view(&with(Strategy::Staged), &c.history, 25_000, &ccx).await;
    assert!(summary_blocks(&v).is_empty());
    assert_eq!(records(&tmp.path().join("s1"))[0]["kind"], "elided");
    assert!(p.requests().is_empty());
}

/// Rounds of work: a new prompt, a plan, a message the user sends during the turn, and
/// fifteen steps.
fn round(c: &mut Conv, n: usize) {
    c.turn(&format!("user prompt #{n}#"));
    c.plan();
    c.history.push(c.turn, Message::assistant("Working on it."));
    c.history.push(
        c.turn,
        Message::user(format!("user note #{n}#, keep it short")),
    );
    for _ in 0..15 {
        c.bash(2000);
    }
}

#[tokio::test]
async fn summaries_are_appended_in_order_and_what_the_user_wrote_and_the_plan_survive() {
    let tmp = tempfile::tempdir().unwrap();
    let mut c = Conv::new("Fix the parser");
    for _ in 0..30 {
        c.bash(2000);
    }
    let p = Summarizer::new();
    let ccx = ccx_with(Some(tmp.path()), &p, None);
    let ctx = with(Strategy::Summarize);
    let window = 30_000;
    let mut blocks: Vec<String> = Vec::new();
    let mut v = Vec::new();
    for n in 1..=7 {
        round(&mut c, n);
        v = view(&ctx, &c.history, window, &ccx).await;
        assert_summarized(&c.history, &v);
        // Earlier summaries stay byte for byte, and new ones come after them.
        let now = summary_blocks(&v);
        assert_eq!(now[..blocks.len()], blocks[..], "round {n}");
        blocks = now;
    }
    assert!(blocks.len() >= 3, "{}", blocks.len());
    for (i, block) in blocks.iter().enumerate() {
        assert!(block.starts_with(&format!("<context-summary n=\"{}\"", i + 1)));
    }
    // Every message the user wrote is sent exactly once, word for word.
    let all: String = v.iter().flat_map(texts).collect::<Vec<_>>().join("\n");
    for n in 1..=7 {
        for text in [
            format!("user prompt #{n}#"),
            format!("user note #{n}#, keep it short"),
        ] {
            assert_eq!(all.matches(&text).count(), 1, "{text}");
        }
    }
    assert!(blocks[0].contains("<user-messages n=\"1\">\n[turn 2, step 0] user prompt #1#"));
    // The latest plan, word for word, once it is summarized.
    let latest = c
        .history
        .events()
        .iter()
        .rev()
        .find_map(|e| {
            e.message
                .tool_calls()
                .into_iter()
                .find(|t| t.name == "update_plan")
        })
        .unwrap();
    let json = latest.input.to_string();
    let first = texts(&v[0]).join("\n");
    let in_place = v
        .iter()
        .any(|m| m.tool_calls().iter().any(|t| t.id == latest.id));
    if !in_place {
        assert!(
            first.contains(&format!(
                "<latest-call tool=\"update_plan\">{json}</latest-call>"
            )),
            "{first}"
        );
    }
    assert!(!in_place, "this test wants the latest plan summarized");

    // A new process, with settings under which it would decide nothing, renders the
    // same bytes.
    let other = Context::new(ContextConfig {
        strategy: Strategy::None,
        ..Default::default()
    });
    assert_eq!(view(&other, &c.history, 10_000_000, &ccx).await, v);
}

#[tokio::test]
async fn past_max_summary_share_the_two_oldest_summaries_are_merged() {
    let tmp = tempfile::tempdir().unwrap();
    let mut c = Conv::new("Fix the parser");
    for _ in 0..30 {
        c.bash(2000);
    }
    let mut p = Summarizer::new();
    p.pad = 3000;
    let ccx = ccx_with(Some(tmp.path()), &p, None);
    let ctx = Context::new(ContextConfig {
        strategy: Strategy::Summarize,
        max_summary_share: 0.05,
        ..Default::default()
    });
    let mut rx = ccx.cx.events.subscribe();
    let mut v = Vec::new();
    let mut n = 0;
    while records(&tmp.path().join("s1")).len() < 3 && n < 12 {
        n += 1;
        round(&mut c, n);
        v = view(&ctx, &c.history, 30_000, &ccx).await;
    }
    let log = records(&tmp.path().join("s1"));
    assert_eq!(log[2]["summary"]["merges"], 2, "{log:#?}");
    let blocks = summary_blocks(&v);
    assert_eq!(blocks.len(), 1);
    assert!(blocks[0].starts_with("<context-summary n=\"1\""));
    assert!(blocks[0].contains("Summary 3 by m."), "the merge's text");
    // Both summaries' copied parts are kept.
    assert!(blocks[0].contains("user prompt #1#"));
    assert!(blocks[0].contains("user prompt #2#") || blocks[0].contains("user prompt #3#"));
    let merged = received(&mut rx)
        .into_iter()
        .any(|e| matches!(e, AgentEvent::Custom { data, .. } if data["merged"] == true));
    assert!(merged);
    assert_eq!(
        view(&with(Strategy::None), &c.history, 10_000_000, &ccx).await,
        v
    );
}

#[tokio::test]
async fn a_separate_summarizer_gets_a_transcript_of_the_span() {
    let tmp = tempfile::tempdir().unwrap();
    let c = long_session();
    let agent = Summarizer::new();
    let small = Summarizer::new();
    let ccx = ccx_with(Some(tmp.path()), &agent, Some((&small, "small-model")));
    let window = window_above(&c.history, 0.85);
    let v = view(&with(Strategy::Summarize), &c.history, window, &ccx).await;
    assert!(agent.requests().is_empty());
    let requests = small.requests();
    assert_eq!(requests.len(), 1);
    let r = &requests[0];
    assert_eq!(r.model, "small-model");
    assert!(r.tools.is_empty() && r.system.is_empty() && r.thinking_budget.is_none());
    assert_eq!(r.messages.len(), 1);
    let text = texts(&r.messages[0]).join("\n");
    assert!(text.contains("<task>\n[user] Fix the parser"), "{text}");
    assert!(text.contains("[assistant calls bash] {\"command\":\"make step1\"}"));
    assert!(text.contains("[tool result] step 1 line 1"));
    assert!(text.contains("This conversation is close to the limit"));
    assert!(summary_blocks(&v)[0].contains("Summary 1 by small-model."));
    assert_eq!(
        records(&tmp.path().join("s1"))[0]["summary"]["model"],
        "small-model"
    );
}

/// A history the trims can't bring below `hard`: mostly short results.
fn hard_to_trim() -> Conv {
    let mut c = Conv::new("Fix the parser");
    for _ in 0..4 {
        c.bash(2000);
    }
    for _ in 0..150 {
        c.bash(450);
    }
    c
}

#[tokio::test]
async fn a_failed_summary_keeps_the_trimmed_view() {
    for answer in [Answer::Fail, Answer::Refuse] {
        let tmp = tempfile::tempdir().unwrap();
        let c = hard_to_trim();
        let p = Summarizer::new();
        p.answer(answer);
        let ccx = ccx_with(Some(tmp.path()), &p, None);
        let mut rx = ccx.cx.events.subscribe();
        let window = window_above(&c.history, 0.97);
        let staged = Context::new(ContextConfig {
            hard: 0.80,
            ..Default::default()
        });
        let v = view(&staged, &c.history, window, &ccx).await;
        assert_eq!(p.requests().len(), 1, "{answer:?}");
        let log = records(&tmp.path().join("s1"));
        assert_eq!(log.len(), 1, "{answer:?}: only the trim");
        assert_eq!(log[0]["kind"], "elided");
        assert!(summary_blocks(&v).is_empty());
        assert_eq!(
            view(&with(Strategy::None), &c.history, 10_000_000, &ccx).await,
            v
        );
        let w = warnings(&mut rx);
        assert!(w.iter().any(|m| m.contains("kept as it was")), "{w:#?}");
    }
}

#[tokio::test]
async fn cancelling_the_turn_during_a_summary_writes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let c = long_session();
    let p = Summarizer::new();
    p.answer(Answer::Cancel);
    let ccx = ccx_with(Some(tmp.path()), &p, None);
    let mut rx = ccx.cx.events.subscribe();
    let window = window_above(&c.history, 0.85);
    let v = view(&with(Strategy::Summarize), &c.history, window, &ccx).await;
    assert!(ccx.cx.cancel.is_cancelled());
    assert_eq!(v, c.history.messages());
    assert!(records(&tmp.path().join("s1")).is_empty());
    assert!(warnings(&mut rx).is_empty());
}

#[tokio::test]
async fn compact_summarizes_up_to_the_last_steps_with_the_user_s_instructions() {
    let tmp = tempfile::tempdir().unwrap();
    let mut c = long_session();
    c.history.push(1, Message::assistant("All done."));
    let p = Summarizer::new();
    let ccx = ccx_with(Some(tmp.path()), &p, None);
    let request = CompactRequest {
        reason: CompactReason::Manual,
        instructions: Some("focus on the parser work".into()),
    };
    let out = with(Strategy::Staged)
        .compact(&c.history, budget(1_000_000), &request, &ccx)
        .await
        .unwrap();
    assert!(out.changed);
    assert!(
        out.message.starts_with("Summarized turn 1, steps 1–29: "),
        "{}",
        out.message
    );
    assert!(out.message.contains("k → "), "{}", out.message);
    let prompt = texts(p.requests()[0].messages.last().unwrap());
    assert!(prompt.last().unwrap().ends_with("focus on the parser work"));
    // The task, the summary, and the last two steps.
    c.history.push(2, Message::user("next"));
    let v = view(&with(Strategy::Staged), &c.history, 1_000_000, &ccx).await;
    assert_summarized(&c.history, &v);
    assert_eq!(v.len(), 1 + 4);
}

#[tokio::test]
async fn an_overflow_under_staged_trims_then_summarizes() {
    let tmp = tempfile::tempdir().unwrap();
    let c = long_session();
    let p = Summarizer::new();
    let ccx = ccx_with(Some(tmp.path()), &p, None);
    let out = compact(
        &with(Strategy::Staged),
        &c.history,
        1_000_000,
        &ccx,
        CompactReason::Overflow,
    )
    .await;
    assert!(out.changed);
    assert!(
        out.message.contains("Trimmed") && out.message.contains("Summarized"),
        "{}",
        out.message
    );
    let log = records(&tmp.path().join("s1"));
    let kinds: Vec<_> = log.iter().map(|r| r["kind"].clone()).collect();
    assert_eq!(kinds, [json!("elided"), json!("summarized")]);
    assert_eq!(log[1]["trigger"], "overflow");
    // `summarize` doesn't trim.
    let tmp = tempfile::tempdir().unwrap();
    let ccx = ccx_with(Some(tmp.path()), &p, None);
    let out = compact(
        &with(Strategy::Summarize),
        &c.history,
        1_000_000,
        &ccx,
        CompactReason::Overflow,
    )
    .await;
    assert!(
        out.changed && !out.message.contains("Trimmed"),
        "{}",
        out.message
    );
}

#[tokio::test]
async fn a_summary_record_must_follow_the_earlier_ones() {
    let tmp = tempfile::tempdir().unwrap();
    let c = long_session();
    let p = Summarizer::new();
    let ccx = ccx_with(Some(tmp.path()), &p, None);
    let window = window_above(&c.history, 0.85);
    let summarized = view(&with(Strategy::Summarize), &c.history, window, &ccx).await;
    let path = tmp.path().join("s1/context/log.jsonl");
    let good = std::fs::read_to_string(&path).unwrap();
    for (field, value) in [("from", json!(3)), ("to", json!(3)), ("merges", json!(1))] {
        let mut record: Value = serde_json::from_str(good.trim()).unwrap();
        record["summary"][field] = value;
        std::fs::write(&path, format!("{record}\n")).unwrap();
        let mut rx = ccx.cx.events.subscribe();
        let v = view(&with(Strategy::None), &c.history, 10_000_000, &ccx).await;
        assert!(summary_blocks(&v).is_empty(), "{field}");
        assert!(!warnings(&mut rx).is_empty(), "{field}");
    }
    std::fs::write(&path, &good).unwrap();
    assert_eq!(
        view(&with(Strategy::None), &c.history, 10_000_000, &ccx).await,
        summarized
    );
}
