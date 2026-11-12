//! The plugin in real sessions with the scripted provider, whose window is small enough
//! to overflow: trimming keeps a long run going, stubs are read back with `read_file`
//! after a resume, and a copied marker is refused. Summaries in sessions are in
//! `signed.rs`.

mod sessions;

use std::path::Path;
use std::sync::Arc;

use ch_kernel::{
    AgentEvent, ContentBlock, ModelRequest, Role, Session, SessionStore, TerminationReason,
};
use ch_provider_scripted::{ScriptStep, ScriptedPlugin, ScriptedProvider};
use serde_json::{Value, json};
use sessions::{build, dirs, received};

/// Trims only, for the tests of trimming in a session.
const ELIDE: &str = "[plugins.context]\nstrategy = \"elide\"";

/// The scripted provider rejects requests over this many tokens.
const WINDOW: u64 = 20_000;

/// `count` calls to `big`, then a reply.
fn calls(count: u64) -> Vec<ScriptStep> {
    let mut steps: Vec<ScriptStep> = (1..=count)
        .map(|n| ScriptStep::tool("big", json!({ "n": n })))
        .collect();
    steps.push(ScriptStep::text("done"));
    steps
}

/// A session over `provider` in `ws`, saved under `sessions` as `s1`, like one `ch`
/// process.
fn session(provider: ScriptedProvider, ws: &Path, sessions: &Path, toml: &str) -> Session {
    session_as("s1", provider, ws, sessions, toml)
}

fn session_as(
    id: &str,
    provider: ScriptedProvider,
    ws: &Path,
    sessions: &Path,
    toml: &str,
) -> Session {
    build(
        id,
        Arc::new(ScriptedPlugin::from_provider(provider)),
        ws,
        sessions,
        &format!(
            "plugins.enable = [\"provider-scripted\"]\n[model]\nmax_output_tokens = 2000\n{toml}"
        ),
    )
}

fn compactions(events: &[AgentEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, AgentEvent::ContextCompacted { .. }))
        .count()
}

/// Requests that don't start with the whole request before them.
fn rewrites(requests: &[ModelRequest]) -> usize {
    requests
        .windows(2)
        .filter(|p| {
            let (a, b) = (&p[0].messages, &p[1].messages);
            b.len() < a.len() || b[..a.len()] != a[..]
        })
        .count()
}

#[tokio::test]
async fn a_long_run_fits_under_elide_and_overflows_under_none() {
    let d = dirs();
    let provider = ScriptedProvider::new(calls(60)).with_context_window(WINDOW);
    let mut s = session(provider.clone(), &d.ws, &d.sessions, ELIDE);
    let mut rx = s.events().subscribe();
    let r = s.run_turn("read the logs").await;
    assert_eq!(r.reason, TerminationReason::Done, "{r:?}");
    assert_eq!(r.steps, 61);
    // Each request repeats the one before it, except right after a trim, which comes
    // every few steps here because each result is a big share of the window.
    let trims = compactions(&received(&mut rx));
    assert!((1..=20).contains(&trims), "{trims}");
    assert_eq!(rewrites(&provider.requests()), trims);

    let d = dirs();
    let provider = ScriptedProvider::new(calls(60)).with_context_window(WINDOW);
    let mut s = session(
        provider,
        &d.ws,
        &d.sessions,
        "[plugins.context]\nstrategy = \"none\"",
    );
    let r = s.run_turn("read the logs").await;
    assert!(
        matches!(r.reason, TerminationReason::ContextOverflow { .. }),
        "{r:?}"
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
async fn plan_reminders_and_trims_keep_each_request_extending_the_last() {
    let d = dirs();
    // Two turns of plan updates and big results: the planning reminder goes on the
    // first call of each turn and stays in the history, and trims come in between.
    let mut steps = Vec::new();
    for turn in 0..2 {
        for part in 0..3 {
            steps.push(plan(part));
            steps.extend((1..=8).map(|n| ScriptStep::tool("big", json!({ "n": turn * 10 + n }))));
        }
        steps.push(ScriptStep::text("done"));
    }
    let provider = ScriptedProvider::new(steps).with_context_window(WINDOW);
    let mut s = session(provider.clone(), &d.ws, &d.sessions, ELIDE);
    let mut rx = s.events().subscribe();
    for prompt in ["read the logs", "and the rest"] {
        assert_eq!(s.run_turn(prompt).await.reason, TerminationReason::Done);
    }
    let trims = compactions(&received(&mut rx));
    assert!(trims >= 2, "{trims}");
    let requests = provider.requests();
    assert_eq!(rewrites(&requests), trims);
    // The second turn's first call carried the plan reminder.
    let second = requests
        .iter()
        .map(|r| format!("{:?}", r.messages.last().unwrap()))
        .find(|last| last.contains("and the rest"))
        .unwrap();
    assert!(
        second.contains("<system-reminder>") && second.contains("Read part"),
        "{second}"
    );
    // Superseded plans are trimmed, the latest kept.
    let last = format!("{:?}", requests.last().unwrap().messages);
    assert!(
        last.contains("a later update_plan call replaced this"),
        "{last}"
    );
}

#[tokio::test]
async fn an_overflow_is_trimmed_and_the_step_sent_again() {
    let d = dirs();
    let provider = ScriptedProvider::new(calls(25)).with_context_window(WINDOW);
    // The session thinks the window is far bigger, so only the provider's rejection
    // shows that it's full.
    let mut s = session(
        provider.clone(),
        &d.ws,
        &d.sessions,
        "context_window = 1000000\n[plugins.context]\nstrategy = \"elide\"",
    );
    let r = s.run_turn("read the logs").await;
    assert_eq!(r.reason, TerminationReason::Done, "{r:?}");
    let log = std::fs::read_to_string(d.sessions.join("s1/context/log.jsonl")).unwrap();
    assert!(log.contains("\"trigger\":\"overflow\""), "{log}");
    assert!(provider.requests().len() > 26, "some steps were sent twice");
}

/// The stubbed result in `request` whose stub names a saved copy: its call id and path.
fn a_stub(request: &ModelRequest) -> (String, String) {
    request
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .find_map(|b| match b {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } if content.starts_with("[Output elided") => {
                let path = content.split("saved at ").nth(1)?.split(';').next()?;
                Some((tool_use_id.clone(), path.to_string()))
            }
            _ => None,
        })
        .unwrap()
}

#[tokio::test]
async fn a_resumed_session_renders_the_same_view_and_reads_a_stub_back() {
    let d = dirs();
    let provider = ScriptedProvider::new(calls(25)).with_context_window(WINDOW);
    let mut first = session(provider.clone(), &d.ws, &d.sessions, ELIDE);
    assert_eq!(
        first.run_turn("read the logs").await.reason,
        TerminationReason::Done
    );
    let last = provider.requests().pop().unwrap();
    let (call, path) = a_stub(&last);
    first.end().await;
    drop(first);

    // Another process, with settings under which it wouldn't have trimmed: the earlier
    // trims still render as they were sent.
    let steps = vec![
        ScriptStep::tool("read_file", json!({"path": path, "limit": 3})),
        ScriptStep::text("found it"),
    ];
    let provider = ScriptedProvider::new(steps).with_context_window(WINDOW);
    let mut resumed = session_as(
        "new",
        provider.clone(),
        &d.ws,
        &d.sessions,
        "[plugins.context]\nstrategy = \"elide\"\nsoft = 0.9\nelide_min_chars = 20000",
    );
    resumed
        .restore(SessionStore::new(&d.sessions).load("s1").unwrap())
        .unwrap();
    assert_eq!(
        resumed.run_turn("show one").await.reason,
        TerminationReason::Done
    );
    let again = &provider.requests()[0];
    assert_eq!(again.messages[..last.messages.len()], last.messages[..]);

    // The stub's file holds the original result, and read_file can open it.
    let original = resumed
        .history()
        .messages()
        .iter()
        .flat_map(|m| m.content.clone())
        .find_map(|b| match b {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } if tool_use_id == call => Some(content),
            _ => None,
        })
        .unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    let read = resumed
        .history()
        .messages()
        .iter()
        .rev()
        .filter(|m| m.role == Role::User)
        .flat_map(|m| m.content.clone())
        .find_map(|b| match b {
            ContentBlock::ToolResult { content, .. } => Some(content),
            _ => None,
        })
        .unwrap();
    let first_line = original.lines().next().unwrap();
    assert!(read.contains(first_line), "{read}");
}

#[tokio::test]
async fn a_call_that_copies_a_marker_is_refused() {
    let d = dirs();
    let marker = "[elided from an earlier call to save context: 5,000 characters]";
    let steps = vec![
        ScriptStep::tool("write_file", json!({"path": "a.txt", "content": marker})),
        ScriptStep::text("ok"),
    ];
    let mut s = session(ScriptedProvider::new(steps), &d.ws, &d.sessions, "");
    s.run_turn("write it").await;
    let result = &s.history().events()[2].message.content[0];
    let ContentBlock::ToolResult {
        content, is_error, ..
    } = result
    else {
        panic!("{result:?}");
    };
    assert!(is_error);
    assert!(content.contains(ch_context::REFUSED), "{content}");
    assert!(!d.ws.join("a.txt").exists());
}
