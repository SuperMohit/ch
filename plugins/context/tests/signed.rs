//! Summaries in real sessions, through `Signed`: every request, summary calls included,
//! must keep the model's signed reasoning valid, and summary calls are held to the
//! provider's window.

mod sessions;

use ch_kernel::reexports::CancellationToken;
use ch_kernel::reexports::futures::StreamExt;
use ch_kernel::{
    AgentEvent, CompactionKind, ContentBlock, Message, ModelEvent, ModelProvider, ModelRequest,
    Role, SessionStore, TerminationReason,
};
use ch_provider_scripted::{ScriptStep, ScriptedProvider};
use serde_json::{Value, json};
use sessions::{Dirs, Signed, dirs, received, signed_session};

/// `count` calls to `big` with `lines` lines each, then a reply. Replies differ, so
/// `Signed` tells them apart without their thinking.
fn calls(from: u64, count: u64, lines: u64) -> Vec<ScriptStep> {
    let mut steps: Vec<ScriptStep> = (from..from + count)
        .map(|n| ScriptStep::tool("big", json!({ "n": n, "lines": lines })))
        .collect();
    steps.push(ScriptStep::text(format!(
        "done with logs {from}–{}",
        from + count - 1
    )));
    steps
}

fn log(d: &Dirs, id: &str) -> Vec<Value> {
    std::fs::read_to_string(d.sessions.join(id).join("context/log.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn kinds(log: &[Value]) -> Vec<String> {
    log.iter()
        .map(|r| format!("{} {}", r["kind"], r["trigger"]).replace('"', ""))
        .collect()
}

/// Thinking blocks in `request`.
fn thinking(request: &ModelRequest) -> usize {
    (request.messages.iter())
        .flat_map(|m| &m.content)
        .filter(|b| matches!(b, ContentBlock::Thinking { .. }))
        .count()
}

fn assert_signed(p: &Signed) {
    assert_eq!(p.violations(), Vec::<String>::new());
}

/// `messages` sent to `p` as an agent request: the reply, or why it was rejected.
async fn send(p: &Signed, messages: Vec<Message>) -> Result<Message, String> {
    let request = ModelRequest {
        model: "scripted".into(),
        system: vec!["You are a test agent.".into()],
        messages,
        tools: Vec::new(),
        max_tokens: 100,
        temperature: None,
        thinking_budget: Some(1000),
        purpose: "agent".into(),
    };
    let stream = (p.stream(request, CancellationToken::new()).await).map_err(|e| e.to_string())?;
    let events: Vec<_> = stream.collect().await;
    Ok(events
        .into_iter()
        .find_map(|e| match e {
            Ok(ModelEvent::Completed(r)) => Some(r.message),
            _ => None,
        })
        .expect("a reply"))
}

/// The result of `reply`'s call.
fn result(reply: &Message) -> Message {
    let id = reply.tool_calls().remove(0).id;
    Message {
        role: Role::User,
        content: vec![ContentBlock::ToolResult {
            tool_use_id: id,
            content: "ok".into(),
            is_error: false,
        }],
    }
}

fn without_thinking(m: &Message) -> Message {
    Message {
        role: m.role,
        content: (m.content.iter())
            .filter(|b| !matches!(b, ContentBlock::Thinking { .. }))
            .cloned()
            .collect(),
    }
}

/// The checker holds a request to the rule exactly: the oldest block can't be left out
/// alone, and a block left out once can't come back, even when a trim changed its reply.
#[tokio::test]
async fn the_checker_refuses_a_leading_gap_and_thinking_brought_back() {
    let script = || {
        ScriptedProvider::new(vec![
            ScriptStep::tool("big", json!({ "n": 1, "note": "x".repeat(2000) })),
            ScriptStep::tool("big", json!({ "n": 2 })),
            ScriptStep::text("done"),
        ])
    };
    let task = Message::user("read the logs");

    // The second reply was written after the first one's thinking.
    let p = Signed::new(script());
    let first = send(&p, vec![task.clone()]).await.unwrap();
    let messages = vec![task.clone(), first.clone(), result(&first)];
    let second = send(&p, messages).await.unwrap();
    let gap = vec![
        task.clone(),
        without_thinking(&first),
        result(&first),
        second.clone(),
        result(&second),
    ];
    let refused = send(&p, gap).await.unwrap_err();
    assert!(
        refused.contains("the thinking before it isn't what it was written after"),
        "{refused}"
    );

    // Left out once, with its input trimmed, then sent again.
    let p = Signed::new(script());
    let first = send(&p, vec![task.clone()]).await.unwrap();
    let mut trimmed = without_thinking(&first);
    if let ContentBlock::ToolUse { input, .. } = &mut trimmed.content[0] {
        input["note"] = json!("[elided]");
    }
    send(&p, vec![task.clone(), trimmed, result(&first)])
        .await
        .unwrap();
    let again = vec![task, first.clone(), result(&first)];
    let refused = send(&p, again).await.unwrap_err();
    assert!(
        refused.contains("thinking left out before and sent again"),
        "{refused}"
    );
}

#[tokio::test]
async fn a_long_run_fits_under_staged_and_each_request_extends_the_last_between_compactions() {
    let d = dirs();
    // Big results to trim, and small ones that only a summary can take away.
    let mut steps: Vec<ScriptStep> = (1..=120)
        .map(|n| {
            let lines = if n % 2 == 0 { 100 } else { 12 };
            ScriptStep::tool("big", json!({ "n": n, "lines": lines }))
        })
        .collect();
    steps.push(ScriptStep::text("done"));
    let p = Signed::new(ScriptedProvider::new(steps).with_context_window(12_000));
    let mut s = signed_session(&p, &d, "s1", "");
    let mut rx = s.events().subscribe();
    let r = s.run_turn("read the logs").await;
    assert_eq!(r.reason, TerminationReason::Done, "{r:?}");
    assert_eq!(r.steps, 121);
    assert_signed(&p);
    // The estimate held: nothing was rejected as too long, so no overflow was needed.
    assert_eq!(p.rejected(), 0);
    assert!(
        kinds(&log(&d, "s1"))
            .iter()
            .all(|k| !k.ends_with("overflow"))
    );

    // Which agent requests came right after a compaction.
    let mut after_compaction = Vec::new();
    let mut compacted = false;
    let (mut trims, mut summaries) = (0, 0);
    for event in received(&mut rx) {
        match event {
            AgentEvent::ContextCompacted { kind, .. } => {
                compacted = true;
                match kind {
                    CompactionKind::Elided => trims += 1,
                    CompactionKind::Summarized => summaries += 1,
                }
            }
            AgentEvent::Custom { data, .. } => assert_ne!(data["trigger"], "overflow"),
            AgentEvent::ModelRequest { purpose, .. } if purpose == "agent" => {
                after_compaction.push(compacted);
                compacted = false;
            }
            _ => {}
        }
    }
    assert!(
        trims > 0 && summaries > 0,
        "{trims} trims, {summaries} summaries"
    );
    let requests = p.inner.requests();
    assert_eq!(requests.len(), after_compaction.len());
    for (i, pair) in requests.windows(2).enumerate() {
        if after_compaction[i + 1] {
            continue;
        }
        let (a, b) = (&pair[0].messages, &pair[1].messages);
        assert_eq!(
            b[..a.len()],
            a[..],
            "request {} doesn't extend the last",
            i + 1
        );
    }
    // The summaries are in the last request's first message, and the reasoning written
    // since the last compaction is sent: most requests carry some.
    let first = format!("{:?}", requests.last().unwrap().messages[0]);
    assert!(first.contains("<context-summary n=\\\"1\\\""), "{first}");
    let reasoned = requests.iter().filter(|r| thinking(r) > 0).count();
    assert!(
        reasoned > requests.len() / 2,
        "{reasoned} of {}",
        requests.len()
    );
}

#[tokio::test]
async fn merged_summaries_keep_the_reasoning_valid() {
    let d = dirs();
    let mut p = Signed::new(ScriptedProvider::new(calls(1, 150, 40)).with_context_window(12_000));
    p.pad = 1500;
    let mut s = signed_session(
        &p,
        &d,
        "s1",
        "[plugins.context]\nstrategy = \"summarize\"\nmax_summary_share = 0.05\nmin_free = 0.02",
    );
    let r = s.run_turn("read the logs").await;
    assert_eq!(r.reason, TerminationReason::Done, "{r:?}");
    assert_signed(&p);
    let log = log(&d, "s1");
    assert!(
        log.iter().any(|r| r["summary"]["merged"] == true),
        "{:?}",
        kinds(&log)
    );
}

#[tokio::test]
async fn compact_then_another_turn_keeps_the_reasoning_valid() {
    let d = dirs();
    let mut script = calls(1, 8, 40);
    script.extend(calls(9, 4, 40));
    let p = Signed::new(ScriptedProvider::new(script).with_context_window(50_000));
    let mut s = signed_session(&p, &d, "s1", "");
    let mut rx = s.events().subscribe();
    assert_eq!(
        s.run_turn("read the logs").await.reason,
        TerminationReason::Done
    );
    let out = s
        .compact(Some("keep the log numbers".into()))
        .await
        .unwrap();
    assert!(out.changed, "{}", out.message);
    // `/compact` shows the summary's text with its event.
    let shown = received(&mut rx).into_iter().any(|e| {
        matches!(e, AgentEvent::Custom { data, .. }
            if data["text"].as_str().is_some_and(|t| t.starts_with("## Goal")))
    });
    assert!(shown);
    let prompt = format!("{:?}", p.summaries()[0].messages.last().unwrap());
    assert!(
        prompt.contains("The user asked: keep the log numbers"),
        "{prompt}"
    );
    assert_eq!(
        s.run_turn("and the rest").await.reason,
        TerminationReason::Done
    );
    assert_signed(&p);
    assert_eq!(kinds(&log(&d, "s1")), ["summarized manual"]);
    // The second turn's reasoning is sent within it.
    assert!(thinking(&p.inner.requests().pop().unwrap()) > 0);
}

#[tokio::test]
async fn a_resumed_session_with_other_settings_keeps_the_reasoning_valid() {
    let d = dirs();
    let p = Signed::new(ScriptedProvider::new(calls(1, 80, 12)).with_context_window(12_000));
    let mut first = signed_session(&p, &d, "s1", "");
    assert_eq!(
        first.run_turn("read the logs").await.reason,
        TerminationReason::Done
    );
    assert!(
        kinds(&log(&d, "s1"))
            .iter()
            .any(|k| k.starts_with("summarized"))
    );
    let last = p.inner.requests().pop().unwrap();
    first.end().await;
    drop(first);

    // Another process, which wouldn't have compacted when the first one did.
    let q = p.then(ScriptedProvider::new(calls(81, 3, 12)).with_context_window(12_000));
    let mut resumed = signed_session(
        &q,
        &d,
        "new",
        "[plugins.context]\nstrategy = \"summarize\"\nhard = 0.95\nsummary_max_words = 300",
    );
    resumed
        .restore(SessionStore::new(&d.sessions).load("s1").unwrap())
        .unwrap();
    assert_eq!(
        resumed.run_turn("go on").await.reason,
        TerminationReason::Done
    );
    assert_signed(&q);
    let again = &q.inner.requests()[0];
    assert_eq!(again.messages[..last.messages.len()], last.messages[..]);
}

/// The session thinks the window is far bigger than the provider's, so only the
/// provider's rejection shows that it's full; the summary call is held to the same
/// window.
#[tokio::test]
async fn an_overflow_is_summarized_and_the_step_sent_again() {
    for strategy in ["summarize", "staged"] {
        let d = dirs();
        let p = Signed::new(ScriptedProvider::new(calls(1, 40, 100)).with_context_window(20_000));
        let toml =
            format!("context_window = 1000000\n[plugins.context]\nstrategy = \"{strategy}\"");
        let mut s = signed_session(&p, &d, "s1", &toml);
        let r = s.run_turn("read the logs").await;
        assert_eq!(r.reason, TerminationReason::Done, "{strategy}: {r:?}");
        assert_signed(&p);
        let kinds = kinds(&log(&d, "s1"));
        assert!(
            kinds.contains(&"summarized overflow".to_string()),
            "{strategy}: {kinds:?}"
        );
        assert_eq!(
            kinds.contains(&"elided overflow".to_string()),
            strategy == "staged",
            "{kinds:?}"
        );
        assert!(p.rejected() > 0);
    }
}

/// The summary replays the request that overflowed up to its last two steps, which are
/// small: the provider rejects the replay too, so the span is halved and the rest is
/// summarized in a further part.
#[tokio::test]
async fn an_overflow_whose_summary_is_rejected_too_is_summarized_in_parts() {
    let d = dirs();
    // Big steps up to about 18k tokens, then small ones until the request overflows.
    let mut script = calls(1, 20, 100);
    script.pop();
    script.extend(calls(21, 40, 10));
    let p = Signed::new(ScriptedProvider::new(script).with_context_window(20_000));
    let toml = "context_window = 1000000\n[plugins.context]\nstrategy = \"summarize\"";
    let mut s = signed_session(&p, &d, "s1", toml);
    let r = s.run_turn("read the logs").await;
    assert_eq!(r.reason, TerminationReason::Done, "{r:?}");
    assert_signed(&p);
    assert!(p.rejected_summaries() > 0);
    let log = log(&d, "s1");
    let parts: Vec<&Value> = (log.iter())
        .filter(|r| r["kind"] == "summarized" && r["trigger"] == "overflow")
        .collect();
    assert!(parts.len() >= 2, "{:?}", kinds(&log));
    // Parts of one compaction are bound to the same event.
    assert!(
        parts
            .windows(2)
            .any(|w| w[0]["at_event"] == w[1]["at_event"]),
        "{log:#?}"
    );
}

fn plan(done: usize) -> ScriptStep {
    let items: Vec<Value> = (0..4)
        .map(|i| {
            let status = match i.cmp(&done) {
                std::cmp::Ordering::Less => "completed",
                std::cmp::Ordering::Equal => "in_progress",
                std::cmp::Ordering::Greater => "pending",
            };
            json!({"content": format!("Read part {i}"), "status": status,
                   "activeForm": format!("Reading part {i}")})
        })
        .collect();
    ScriptStep::tool("update_plan", json!({ "plan": items }))
}

#[tokio::test]
async fn plan_reminders_and_summaries_keep_the_reasoning_valid() {
    let d = dirs();
    let mut steps = Vec::new();
    for turn in 0..2 {
        for part in 0..4 {
            steps.push(plan(part));
            steps.extend((1..=10).map(|n| {
                ScriptStep::tool(
                    "big",
                    json!({ "n": turn * 100 + part * 10 + n, "lines": 30 }),
                )
            }));
        }
        steps.push(ScriptStep::text(format!("done with turn {turn}")));
    }
    let p = Signed::new(ScriptedProvider::new(steps).with_context_window(12_000));
    let mut s = signed_session(&p, &d, "s1", "");
    for prompt in ["read the logs", "and the rest"] {
        assert_eq!(s.run_turn(prompt).await.reason, TerminationReason::Done);
    }
    assert_signed(&p);
    assert!(
        kinds(&log(&d, "s1"))
            .iter()
            .any(|k| k.starts_with("summarized"))
    );
    // The second turn's first call carried the plan reminder.
    let second = (p.inner.requests().iter())
        .map(|r| format!("{:?}", r.messages.last().unwrap()))
        .find(|last| last.contains("and the rest"))
        .unwrap();
    assert!(
        second.contains("<system-reminder>") && second.contains("Read part"),
        "{second}"
    );
}
