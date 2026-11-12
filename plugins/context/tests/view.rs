//! The strategy's views, called directly over hand-built histories: the invariants of
//! ADR 0004 that apply to trimming.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ch_context::{Context, ContextConfig, REFUSED, Strategy};
use ch_kernel::util::system_reminder;
use ch_kernel::{
    AgentEvent, Budget, CompactOutcome, CompactReason, CompactRequest, ContentBlock, ContextCx,
    ContextStrategy, Cx, History, Hook, HookDecision, HookEvent, Message, ModelRequest, Role,
    TokenCalibrator, ToolCall, ToolSpec, UsageLog,
};
use ch_provider_scripted::ScriptedProvider;
use serde_json::{Value, json};

// ---------- a conversation ----------

/// Builds a history the way the session does: a task, then steps of a tool call and
/// its result, with a reminder on each message the model is sent.
struct Conv {
    history: History,
    turn: u32,
    calls: usize,
    /// Extra characters of thinking in each step.
    think: usize,
}

impl Conv {
    fn new(task: &str) -> Self {
        let mut history = History::new();
        history.push(
            1,
            Message {
                role: Role::User,
                content: vec![ContentBlock::text(task), reminder("the plan is empty")],
            },
        );
        Self {
            history,
            turn: 1,
            calls: 0,
            think: 0,
        }
    }

    /// The model thinks and calls `tool`; its result comes back with a reminder.
    fn call(&mut self, tool: &str, input: Value, result: String) -> &mut Self {
        self.calls += 1;
        let id = format!("c{}", self.calls);
        self.history.push(
            self.turn,
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Thinking {
                        thinking: format!(
                            "thinking before call {}{}",
                            self.calls,
                            "t".repeat(self.think)
                        ),
                        signature: Some("signed".into()),
                    },
                    ContentBlock::ToolUse {
                        id: id.clone(),
                        name: tool.into(),
                        input,
                    },
                ],
            },
        );
        self.history.push(
            self.turn,
            Message {
                role: Role::User,
                content: vec![
                    ContentBlock::ToolResult {
                        tool_use_id: id,
                        content: result,
                        is_error: false,
                    },
                    reminder("check the build"),
                ],
            },
        );
        self
    }

    /// A shell command with a `chars`-character numbered log.
    fn bash(&mut self, chars: usize) -> &mut Self {
        let n = self.calls + 1;
        self.call(
            "bash",
            json!({"command": format!("make step{n}")}),
            log(n, chars),
        )
    }

    fn plan(&mut self) -> &mut Self {
        let n = self.calls + 1;
        let plan = json!({"plan": [{"content": format!("step {n}"), "status": "in_progress"}]});
        self.call("update_plan", plan, "Plan updated: 1 step.".into())
    }

    fn write(&mut self, chars: usize) -> &mut Self {
        let input = json!({"path": "src/lib.rs", "content": "x".repeat(chars)});
        self.call("write_file", input, "Wrote src/lib.rs.".into())
    }

    /// The model answers, and the user sends `prompt`, which starts the next turn.
    fn turn(&mut self, prompt: &str) -> &mut Self {
        self.history.push(self.turn, Message::assistant("Done."));
        self.turn += 1;
        self.history.push(self.turn, Message::user(prompt));
        self
    }
}

fn reminder(text: &str) -> ContentBlock {
    ContentBlock::text(system_reminder(text))
}

fn log(n: usize, chars: usize) -> String {
    let mut out = String::new();
    let mut line = 0;
    while out.len() < chars {
        line += 1;
        out.push_str(&format!("step {n} line {line}\n"));
    }
    out
}

// ---------- the strategy's inputs ----------

fn budget(window: u64) -> Budget {
    Budget {
        window,
        reserved_output: 0,
        fixed_tokens: 0,
        calibrator: TokenCalibrator::default(),
    }
}

fn ccx(session_dir: Option<&Path>, tools: &[&str]) -> ContextCx {
    ccx_for("s1", session_dir, tools)
}

fn ccx_for(id: &str, session_dir: Option<&Path>, tools: &[&str]) -> ContextCx {
    let mut cx = Cx::detached("/ws");
    cx.session_id = Arc::from(id);
    cx.session_dir = session_dir.map(|d| Arc::new(d.join(id)));
    cx.tools = Arc::new(
        tools
            .iter()
            .map(|name| ToolSpec {
                name: name.to_string(),
                description: String::new(),
                input_schema: json!({"type": "object"}),
                read_only: true,
            })
            .collect(),
    );
    ContextCx {
        cx,
        provider: Arc::new(ScriptedProvider::new(Vec::new())),
        agent_request: ModelRequest {
            model: "m".into(),
            system: Vec::new(),
            messages: Vec::new(),
            tools: Vec::new(),
            max_tokens: 1000,
            temperature: None,
            thinking_budget: None,
            purpose: "agent".into(),
        },
        summarizer: None,
        usage: UsageLog::default(),
    }
}

const FILE_TOOLS: &[&str] = &["read_file", "grep_text"];

fn elide() -> Context {
    Context::new(ContextConfig::default())
}

async fn view(ctx: &Context, history: &History, window: u64, ccx: &ContextCx) -> Vec<Message> {
    ctx.view(history, budget(window), ccx)
        .await
        .unwrap()
        .messages
}

async fn compact(
    ctx: &Context,
    history: &History,
    window: u64,
    ccx: &ContextCx,
    reason: CompactReason,
) -> CompactOutcome {
    let request = CompactRequest {
        reason,
        instructions: None,
    };
    ctx.compact(history, budget(window), &request, ccx)
        .await
        .unwrap()
}

fn tokens(messages: &[Message]) -> u64 {
    budget(1).measure(messages)
}

fn received(rx: &mut tokio::sync::broadcast::Receiver<AgentEvent>) -> Vec<AgentEvent> {
    std::iter::from_fn(|| rx.try_recv().ok()).collect()
}

/// The records in a session's log, as JSON.
fn records(session_dir: &Path) -> Vec<Value> {
    match std::fs::read_to_string(session_dir.join("context/log.jsonl")) {
        Ok(text) => text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect(),
        Err(_) => Vec::new(),
    }
}

// ---------- the invariants ----------

fn is_thinking(b: &ContentBlock) -> bool {
    matches!(
        b,
        ContentBlock::Thinking { .. } | ContentBlock::RedactedThinking { .. }
    )
}

fn without_thinking(m: &Message) -> Message {
    Message {
        role: m.role,
        content: m
            .content
            .iter()
            .filter(|b| !is_thinking(b))
            .cloned()
            .collect(),
    }
}

/// What holds for every view.
fn assert_invariants(history: &History, view: &[Message]) {
    let messages = history.messages();
    assert_eq!(view.len(), messages.len(), "no message is removed");
    assert_eq!(view[0], messages[0], "the preamble is byte-identical");
    assert_eq!(
        view.last(),
        messages.last(),
        "the latest message is unchanged"
    );
    for (i, (v, h)) in view.iter().zip(&messages).enumerate() {
        let role = if i % 2 == 0 {
            Role::User
        } else {
            Role::Assistant
        };
        assert_eq!(v.role, role, "roles alternate from a user message");
        let shape = |m: &Message| -> Vec<String> {
            without_thinking(m)
                .content
                .iter()
                .map(|b| match b {
                    ContentBlock::ToolUse { id, name, .. } => format!("use {id} {name}"),
                    ContentBlock::ToolResult { tool_use_id, .. } => format!("result {tool_use_id}"),
                    ContentBlock::Text { .. } => "text".into(),
                    other => panic!("{other:?}"),
                })
                .collect()
        };
        assert_eq!(shape(v), shape(h), "message {i} keeps its blocks");
        for (vb, hb) in without_thinking(v)
            .content
            .iter()
            .zip(&without_thinking(h).content)
        {
            if let ContentBlock::Text { text } = hb
                && !text.starts_with("<system-reminder>")
            {
                assert_eq!(vb, hb, "user text is unchanged");
            }
        }
    }
    for pair in view.windows(2) {
        let uses: Vec<_> = pair[0].tool_calls().into_iter().map(|c| c.id).collect();
        let results: Vec<_> = (pair[1].content.iter())
            .filter_map(|b| match b {
                ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.clone()),
                _ => None,
            })
            .collect();
        if pair[0].role == Role::Assistant {
            assert_eq!(uses, results, "each tool_use has its tool_result");
        }
    }
}

/// The newest messages that are the history's, apart from dropped thinking.
fn untouched_suffix(history: &History, view: &[Message]) -> usize {
    let messages = history.messages();
    view.iter()
        .zip(&messages)
        .rev()
        .take_while(|(v, h)| without_thinking(v) == without_thinking(h))
        .count()
}

/// Every stub's file holds the original result.
fn assert_stubs_read_back(history: &History, view: &[Message]) -> usize {
    let mut read = 0;
    for (v, h) in view.iter().zip(history.messages()) {
        for (vb, hb) in v.content.iter().zip(&h.content) {
            let (
                ContentBlock::ToolResult { content: stub, .. },
                ContentBlock::ToolResult {
                    content: original, ..
                },
            ) = (vb, hb)
            else {
                continue;
            };
            if let Some(rest) = stub.split("saved at ").nth(1) {
                let path = rest.split(';').next().unwrap();
                assert_eq!(&std::fs::read_to_string(path).unwrap(), original, "{path}");
                read += 1;
            }
        }
    }
    read
}

/// Thinking blocks after a record's first edit (all of them, for a reset), through its
/// `at_event`, are gone; the others are kept.
fn assert_thinking_rule(history: &History, view: &[Message], records: &[Value]) {
    let ranges: Vec<(Option<u64>, u64)> = records
        .iter()
        .map(|r| {
            (
                r["edits"][0]["event"].as_u64(),
                r["at_event"].as_u64().unwrap(),
            )
        })
        .collect();
    for (event, v) in history.events().iter().zip(view) {
        if event.message.role != Role::Assistant {
            continue;
        }
        let dropped = ranges
            .iter()
            .any(|&(first, at)| first.is_none_or(|f| event.id > f) && event.id <= at);
        let kept = v.content.iter().filter(|b| is_thinking(b)).count();
        let had = event
            .message
            .content
            .iter()
            .filter(|b| is_thinking(b))
            .count();
        assert_eq!(kept, if dropped { 0 } else { had }, "event {}", event.id);
    }
}

/// 30 steps of 2,000-character logs, about 18k tokens.
fn long_session() -> Conv {
    let mut c = Conv::new("Fix the parser");
    for _ in 0..30 {
        c.bash(2000);
    }
    c
}

// ---------- tests ----------

#[tokio::test]
async fn at_or_below_soft_the_history_is_sent_as_it_is() {
    let tmp = tempfile::tempdir().unwrap();
    let c = long_session();
    let ccx = ccx(Some(tmp.path()), FILE_TOOLS);
    let size = tokens(&c.history.messages());
    // Just at `soft` of the usable window.
    let window = (size as f64 / 0.60).ceil() as u64;
    let v = view(&elide(), &c.history, window, &ccx).await;
    assert_eq!(v, c.history.messages());
    assert!(records(&tmp.path().join("s1")).is_empty());
}

#[tokio::test]
async fn above_soft_the_middle_is_trimmed_down_to_trim_to() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("s1");
    let c = long_session();
    let ccx = ccx(Some(tmp.path()), FILE_TOOLS);
    let mut rx = ccx.cx.events.subscribe();
    let window = 25_000;
    let before = tokens(&c.history.messages());
    assert!(before as f64 > 0.60 * window as f64, "{before}");

    let v = view(&elide(), &c.history, window, &ccx).await;
    let after = tokens(&v);
    assert_invariants(&c.history, &v);
    assert!(after as f64 <= 0.40 * window as f64, "{after}");
    assert!((before - after) as f64 >= 0.10 * window as f64);
    // The recent window is word for word, apart from thinking.
    let suffix = untouched_suffix(&c.history, &v);
    assert!(suffix >= 4, "at least two steps: {suffix}");
    let recent = &v[v.len() - suffix..];
    assert!(
        tokens(recent) as f64 >= 0.30 * window as f64,
        "{}",
        tokens(recent)
    );
    // Stubs name files that hold the originals.
    assert!(assert_stubs_read_back(&c.history, &v) > 5);
    let log = records(&dir);
    assert_eq!(log.len(), 1);
    assert_eq!(log[0]["trigger"], "soft");
    assert_thinking_rule(&c.history, &v, &log);

    let events = received(&mut rx);
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::ContextCompacted { tokens_before, tokens_after, .. }
            if *tokens_before == before && *tokens_after == after
    )));
    let custom = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::Custom { source, name, data }
                if source == "context" && name == "compaction" =>
            {
                Some(data.clone())
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(custom["trigger"], "soft");
    assert!(custom["results_elided"].as_u64().unwrap() > 5);
    assert!(custom["thinking_dropped"].as_u64().unwrap() > 5);
}

#[tokio::test]
async fn between_trims_each_view_extends_the_last_and_a_reload_renders_the_same_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    let mut c = long_session();
    let ccx = ccx(Some(tmp.path()), FILE_TOOLS);
    let ctx = elide();
    let window = 25_000;
    let first = view(&ctx, &c.history, window, &ccx).await;
    assert_eq!(records(&tmp.path().join("s1")).len(), 1);

    // A few small steps don't reach `soft` again: no new trim, and the view only grows.
    // Their thinking was written against the trimmed view, so it stays.
    for _ in 0..3 {
        c.bash(100);
    }
    let next = view(&ctx, &c.history, window, &ccx).await;
    assert_eq!(next[..first.len()], first[..]);
    let log = records(&tmp.path().join("s1"));
    assert_eq!(log.len(), 1);
    assert_thinking_rule(&c.history, &next, &log);
    let newest = next.len() - 2;
    assert!(next[newest].content.iter().any(is_thinking));

    // A new process renders the same bytes from the history and the log, also with
    // settings under which it would decide nothing: the bytes come from the log.
    let other = Context::new(ContextConfig {
        soft: 0.9,
        elide_min_chars: 20_000,
        superseded_tools: Vec::new(),
        ..Default::default()
    });
    let reloaded = view(&other, &c.history, 1_000_000, &ccx).await;
    assert_eq!(reloaded, next);
    // `none` doesn't trim, but a session trimmed before keeps its bytes.
    let none = Context::new(ContextConfig {
        strategy: Strategy::None,
        ..Default::default()
    });
    assert_eq!(view(&none, &c.history, 1_000_000, &ccx).await, next);
}

#[tokio::test]
async fn compact_between_turns_drops_the_final_answer_s_thinking() {
    let tmp = tempfile::tempdir().unwrap();
    let mut c = Conv::new("Fix the parser");
    for _ in 0..6 {
        c.bash(2000);
    }
    // The model's final answer, with its reasoning; `/compact` comes after it.
    c.history.push(
        1,
        Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    thinking: "all done".into(),
                    signature: Some("signed".into()),
                },
                ContentBlock::text("Done."),
            ],
        },
    );
    let ccx = ccx(Some(tmp.path()), FILE_TOOLS);
    let out = compact(&elide(), &c.history, 1_000_000, &ccx, CompactReason::Manual).await;
    assert!(out.changed, "{}", out.message);
    // It was signed against the untrimmed history, so the next turn can't send it.
    c.history.push(2, Message::user("next"));
    let v = view(&elide(), &c.history, 1_000_000, &ccx).await;
    let answer = &v[v.len() - 2];
    assert_eq!(answer.content, [ContentBlock::text("Done.")]);
}

#[tokio::test]
async fn a_trim_that_frees_too_little_is_not_made() {
    let tmp = tempfile::tempdir().unwrap();
    // Many short steps: almost nothing in the middle is long enough to trim.
    let mut c = Conv::new("Fix the parser");
    for _ in 0..60 {
        c.bash(300);
    }
    c.bash(700);
    for _ in 0..10 {
        c.bash(300);
    }
    let ccx = ccx(Some(tmp.path()), FILE_TOOLS);
    let size = tokens(&c.history.messages());
    let window = (size as f64 / 0.70) as u64;
    let v = view(&elide(), &c.history, window, &ccx).await;
    assert_eq!(v, c.history.messages());
    assert!(records(&tmp.path().join("s1")).is_empty());
    // There was something to trim, only not enough.
    let out = compact(&elide(), &c.history, window, &ccx, CompactReason::Manual).await;
    assert!(out.changed, "{}", out.message);
}

#[tokio::test]
async fn superseded_plans_go_first_and_only_the_latest_stays() {
    let tmp = tempfile::tempdir().unwrap();
    let mut c = Conv::new("Fix the parser");
    for _ in 0..6 {
        c.plan();
        c.bash(400);
    }
    for _ in 0..4 {
        c.bash(400);
    }
    let ccx = ccx(Some(tmp.path()), FILE_TOOLS);
    let out = compact(&elide(), &c.history, 100_000, &ccx, CompactReason::Manual).await;
    assert!(out.changed, "{}", out.message);
    assert!(
        out.message.contains("5 superseded calls"),
        "{}",
        out.message
    );
    let v = view(&elide(), &c.history, 100_000, &ccx).await;
    assert_invariants(&c.history, &v);
    let plans: Vec<&Value> = v
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|b| match b {
            ContentBlock::ToolUse { name, input, .. } if name == "update_plan" => Some(input),
            _ => None,
        })
        .collect();
    assert_eq!(plans.len(), 6);
    for old in &plans[..5] {
        assert_eq!(
            **old,
            json!({"superseded": "a later update_plan call replaced this"})
        );
    }
    assert_eq!(plans[5]["plan"][0]["content"], "step 11");
}

#[tokio::test]
async fn long_inputs_become_markers_and_old_reminders_are_elided_but_user_text_stays() {
    let tmp = tempfile::tempdir().unwrap();
    let mut c = Conv::new("Fix the parser");
    c.write(5000);
    c.turn("<system-reminder> is how I start every prompt; keep going");
    c.write(5000);
    // A message the user sent during the turn.
    c.history.push(c.turn, Message::assistant("Working on it."));
    c.history
        .push(c.turn, Message::user("also update the docs"));
    for _ in 0..3 {
        c.bash(100);
    }
    let ccx = ccx(Some(tmp.path()), FILE_TOOLS);
    let out = compact(&elide(), &c.history, 100_000, &ccx, CompactReason::Manual).await;
    assert!(out.changed, "{}", out.message);
    let v = view(&elide(), &c.history, 100_000, &ccx).await;
    assert_invariants(&c.history, &v);
    let flat = format!("{v:?}");
    assert_eq!(
        flat.matches("[elided from an earlier call to save context: 5,000 characters]")
            .count(),
        2
    );
    assert!(flat.contains("<system-reminder> is how I start every prompt"));
    assert!(flat.contains("also update the docs"));
    assert!(flat.contains("[reminder elided]"));
    // The task's own reminder is part of the preamble.
    assert!(flat.contains("the plan is empty"));
}

#[tokio::test]
async fn a_spilled_result_points_at_its_spill_file_and_needs_no_copy() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("s1");
    std::fs::create_dir_all(dir.join("spill")).unwrap();
    let spill_dir = std::fs::canonicalize(tmp.path()).unwrap().join("s1/spill");
    // Spill names a file after the call it was made for: this result's call is `c1`.
    let own = spill_dir.join("c1-bash-ab12cd34.txt");
    let other = spill_dir.join("c2-bash-ef56ab78.txt");
    let head = "x".repeat(900);
    std::fs::write(&own, format!("{head}{}", "y".repeat(5000))).unwrap();
    std::fs::write(&other, format!("{head}{}", "y".repeat(5000))).unwrap();
    let notice = |path: &Path| {
        format!(
            "{head}\n\n[… The full output is saved at {}.\nRead the rest]",
            path.display()
        )
    };
    let mut c = Conv::new("Fix the parser");
    c.call("bash", json!({"command": "make"}), notice(&own));
    // Results that name another call's spill file get a copy instead.
    c.call("bash", json!({"command": "make"}), notice(&own));
    c.call("bash", json!({"command": "make"}), notice(&other));
    for _ in 0..3 {
        c.bash(100);
    }
    let ccx = ccx(Some(tmp.path()), &["bash"]);
    compact(&elide(), &c.history, 100_000, &ccx, CompactReason::Manual).await;
    let v = view(&elide(), &c.history, 100_000, &ccx).await;
    let result = |i: usize| match &v[i].content[0] {
        ContentBlock::ToolResult { content, .. } => content.clone(),
        other => panic!("{other:?}"),
    };
    let first = result(2);
    assert!(
        first.contains(&format!(
            "The whole output, 5,900 bytes, is saved at {}",
            own.display()
        )),
        "{first}"
    );
    assert!(first.contains("a preview of 4 lines"), "{first}");
    assert!(
        first.contains("from line N on with `tail -n +N <path> | head -n 200`"),
        "{first}"
    );
    // The second names `c1`'s file but is `c2`'s result; the third is `c3`'s.
    for i in [4, 6] {
        let stub = result(i);
        assert!(stub.contains("context/elided/"), "{stub}");
    }
    assert!(dir.join("context/elided").exists());
}

#[tokio::test]
async fn without_a_session_directory_stubs_say_to_run_the_command_again() {
    let c = long_session();
    let ctx = elide();
    let ccx = ccx(None, FILE_TOOLS);
    let v = view(&ctx, &c.history, 25_000, &ccx).await;
    assert_invariants(&c.history, &v);
    let flat = format!("{v:?}");
    assert!(flat.contains("Run the command again if you need it."));
    assert!(!flat.contains("saved at"));
    // The trim is kept in memory for the rest of the session.
    assert_eq!(view(&ctx, &c.history, 25_000, &ccx).await, v);
}

#[tokio::test]
async fn an_overflow_trims_every_candidate_and_results_in_the_window_but_the_newest_step() {
    let tmp = tempfile::tempdir().unwrap();
    let mut c = Conv::new("Fix the parser");
    for _ in 0..6 {
        c.bash(2000);
    }
    let ccx = ccx(Some(tmp.path()), FILE_TOOLS);
    // A big window: nothing is over `soft`, and the whole session is the recent window.
    let window = 1_000_000;
    let out = compact(&elide(), &c.history, window, &ccx, CompactReason::Overflow).await;
    assert!(out.changed);
    let v = view(&elide(), &c.history, window, &ccx).await;
    assert_invariants(&c.history, &v);
    let stubbed = |m: &Message| matches!(&m.content[0], ContentBlock::ToolResult { content, .. } if content.starts_with("[Output elided"));
    let results: Vec<bool> = v.iter().skip(2).step_by(2).map(stubbed).collect();
    assert_eq!(results, [true, true, true, true, true, false]);
    assert_eq!(records(&tmp.path().join("s1"))[0]["trigger"], "overflow");

    // Nothing left to trim: the session isn't sent again.
    let again = compact(&elide(), &c.history, window, &ccx, CompactReason::Overflow).await;
    assert!(!again.changed, "{}", again.message);
}

fn warnings(rx: &mut tokio::sync::broadcast::Receiver<AgentEvent>) -> Vec<String> {
    received(rx)
        .into_iter()
        .filter_map(|e| match e {
            AgentEvent::Warning { message } => Some(message),
            _ => None,
        })
        .collect()
}

fn thinking_left(view: &[Message]) -> usize {
    view.iter()
        .flat_map(|m| &m.content)
        .filter(|b| is_thinking(b))
        .count()
}

/// A history trimmed twice, with the model answering after each trim: its view and the
/// two records.
async fn trimmed_twice(tmp: &Path) -> (Conv, Vec<Message>, String) {
    let mut c = long_session();
    let ccx = ccx(Some(tmp), FILE_TOOLS);
    let ctx = elide();
    view(&ctx, &c.history, 25_000, &ccx).await;
    for _ in 0..12 {
        c.bash(2000);
    }
    let v = view(&ctx, &c.history, 25_000, &ccx).await;
    c.bash(100);
    let log = std::fs::read_to_string(tmp.join("s1/context/log.jsonl")).unwrap();
    assert_eq!(log.lines().count(), 2);
    (c, v, log)
}

#[tokio::test]
async fn a_record_that_no_longer_matches_stops_the_log_and_resets_the_reasoning() {
    let tmp = tempfile::tempdir().unwrap();
    let (c, _, log) = trimmed_twice(tmp.path()).await;
    let path = tmp.path().join("s1/context/log.jsonl");
    let ccx = ccx(Some(tmp.path()), FILE_TOOLS);

    // The first record's block changed (the history was edited), though the model
    // answered after it: what was sent can't be rendered again.
    let mut lines: Vec<Value> = log
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    lines[0]["edits"][0]["hash"] = json!("0000");
    let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
    std::fs::write(&path, text).unwrap();
    let mut rx = ccx.cx.events.subscribe();
    let v = view(&elide(), &c.history, 1_000_000, &ccx).await;
    let w = warnings(&mut rx);
    assert_eq!(w.len(), 1, "{w:#?}");
    assert!(
        w[0].contains("record 1") && w[0].contains("has changed"),
        "{}",
        w[0]
    );
    // Neither record applies, the later one included; no thinking is sent.
    assert_eq!(thinking_left(&v), 0);
    let stripped: Vec<Message> = c.history.messages().iter().map(without_thinking).collect();
    assert_eq!(v, stripped);
    // The reset is in the log, so the next process sends the same bytes, quietly.
    let after = records(&tmp.path().join("s1"));
    assert_eq!(after.len(), 1);
    assert_eq!(after[0]["kind"], "reset");
    let mut rx = ccx.cx.events.subscribe();
    assert_eq!(view(&elide(), &c.history, 1_000_000, &ccx).await, v);
    assert!(warnings(&mut rx).is_empty());
}

#[tokio::test]
async fn a_line_that_doesn_t_parse_resets_unless_it_is_the_torn_end_of_the_log() {
    let tmp = tempfile::tempdir().unwrap();
    let (c, trimmed, log) = trimmed_twice(tmp.path()).await;
    let path = tmp.path().join("s1/context/log.jsonl");
    let ccx = ccx(Some(tmp.path()), FILE_TOOLS);

    // A crash while appending: the last line is cut off. Nothing was sent with it.
    std::fs::write(&path, format!("{log}{{\"kind\":\"eli")).unwrap();
    let mut rx = ccx.cx.events.subscribe();
    assert_eq!(
        view(&elide(), &c.history, 1_000_000, &ccx).await[..trimmed.len()],
        trimmed[..]
    );
    assert!(warnings(&mut rx)[0].contains("newer than the saved session"));
    assert_eq!(
        records(&tmp.path().join("s1")).len(),
        2,
        "the torn line is gone"
    );

    // Garbage in the middle of the log.
    let first = log.lines().next().unwrap();
    std::fs::write(&path, format!("not json\n{first}\n")).unwrap();
    let v = view(&elide(), &c.history, 1_000_000, &ccx).await;
    assert_eq!(thinking_left(&v), 0);
    assert_eq!(records(&tmp.path().join("s1"))[0]["kind"], "reset");
}

#[tokio::test]
async fn an_edit_to_a_block_already_edited_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let (c, _, log) = trimmed_twice(tmp.path()).await;
    let path = tmp.path().join("s1/context/log.jsonl");
    let ccx = ccx(Some(tmp.path()), FILE_TOOLS);
    let mut lines: Vec<Value> = log
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    // The second record also edits the first record's first block.
    let twice = lines[0]["edits"][0].clone();
    lines[1]["edits"].as_array_mut().unwrap().insert(0, twice);
    let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
    std::fs::write(&path, text).unwrap();
    let mut rx = ccx.cx.events.subscribe();
    view(&elide(), &c.history, 1_000_000, &ccx).await;
    let w = warnings(&mut rx);
    assert!(
        w[0].contains("record 2") && w[0].contains("edited twice"),
        "{w:#?}"
    );
}

#[tokio::test]
async fn records_from_before_a_crash_are_dropped_and_don_t_come_back_when_ids_are_reused() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("s1");
    let ccx = ccx(Some(tmp.path()), FILE_TOOLS);
    // The process trims at the start of a turn, then stops before the session is saved:
    // the history on disk ends before that turn, but the log has the trim.
    let mut lost = long_session();
    lost.turn("now the docs");
    view(&elide(), &lost.history, 25_000, &ccx).await;
    assert_eq!(records(&dir).len(), 1);

    // The next process reuses the turn's event id for another prompt.
    let mut resumed = long_session();
    resumed.turn("something else entirely");
    let mut rx = ccx.cx.events.subscribe();
    let v = view(&elide(), &resumed.history, 1_000_000, &ccx).await;
    assert_eq!(v, resumed.history.messages(), "the lost trim isn't applied");
    assert!(warnings(&mut rx)[0].contains("newer than the saved session"));
    assert!(records(&dir).is_empty(), "it is gone from the log");

    // A process after that, with the model having answered since: still nothing comes
    // back, and nothing is reset.
    for _ in 0..3 {
        resumed.bash(100);
    }
    let mut rx = ccx.cx.events.subscribe();
    let v = view(&elide(), &resumed.history, 1_000_000, &ccx).await;
    assert_eq!(v, resumed.history.messages());
    assert!(warnings(&mut rx).is_empty());
}

#[tokio::test]
async fn each_session_has_its_own_log() {
    let tmp = tempfile::tempdir().unwrap();
    let c = long_session();
    let ctx = elide();
    let a = ccx_for("a", Some(tmp.path()), FILE_TOOLS);
    let b = ccx_for("b", Some(tmp.path()), FILE_TOOLS);
    let trimmed = view(&ctx, &c.history, 25_000, &a).await;
    assert_ne!(trimmed, c.history.messages());
    // Session `b` (after `/clear`, say) has no trim, even with a bigger window.
    assert_eq!(
        view(&ctx, &c.history, 1_000_000, &b).await,
        c.history.messages()
    );
    // Back in `a` (`/resume`), its log is read again.
    assert_eq!(view(&ctx, &c.history, 1_000_000, &a).await, trimmed);
}

#[tokio::test]
async fn under_none_nothing_is_trimmed_and_compact_says_so() {
    let tmp = tempfile::tempdir().unwrap();
    let c = long_session();
    let none = Context::new(ContextConfig {
        strategy: Strategy::None,
        ..Default::default()
    });
    let ccx = ccx(Some(tmp.path()), FILE_TOOLS);
    assert_eq!(
        view(&none, &c.history, 25_000, &ccx).await,
        c.history.messages()
    );
    let out = compact(&none, &c.history, 25_000, &ccx, CompactReason::Overflow).await;
    assert!(!out.changed);
    assert!(out.message.contains("off"), "{}", out.message);
    assert!(records(&tmp.path().join("s1")).is_empty());
}

#[tokio::test]
async fn a_copy_left_by_a_crash_is_reused_and_a_foreign_file_is_not_trusted() {
    let tmp = tempfile::tempdir().unwrap();
    let c = long_session();
    let ccx = ccx(Some(tmp.path()), FILE_TOOLS);
    let expected = view(&elide(), &c.history, 25_000, &ccx).await;
    // As if the process stopped after saving the copies but before the record.
    let dir = tmp.path().join("s1");
    std::fs::remove_file(dir.join("context/log.jsonl")).unwrap();
    let elided = dir.join("context/elided");
    let first: PathBuf = std::fs::read_dir(&elided)
        .unwrap()
        .map(|e| e.unwrap().path())
        .min()
        .unwrap();
    std::fs::write(&first, "something else").unwrap();
    let mut rx = ccx.cx.events.subscribe();
    let v = view(&elide(), &c.history, 25_000, &ccx).await;
    assert_ne!(v, expected, "one stub can't point at the changed file");
    assert!(format!("{v:?}").contains("Run the command again"));
    let warned = received(&mut rx).into_iter().any(
        |e| matches!(e, AgentEvent::Warning { message } if message.contains("a different file")),
    );
    assert!(warned);
    assert_eq!(std::fs::read_to_string(&first).unwrap(), "something else");
    // The record holds the stub that was sent, so a reload renders the same view.
    assert_eq!(view(&elide(), &c.history, 25_000, &ccx).await, v);
}

#[cfg(unix)]
#[tokio::test]
async fn a_trim_whose_record_can_t_be_saved_isn_t_made() {
    use std::os::unix::fs::PermissionsExt as _;
    let tmp = tempfile::tempdir().unwrap();
    let c = long_session();
    let ccx = ccx(Some(tmp.path()), FILE_TOOLS);
    let log = tmp.path().join("s1/context/log.jsonl");
    std::fs::create_dir_all(log.parent().unwrap()).unwrap();
    std::fs::write(&log, "").unwrap();
    std::fs::set_permissions(&log, std::fs::Permissions::from_mode(0o400)).unwrap();
    let mut rx = ccx.cx.events.subscribe();
    let ctx = elide();
    let v = view(&ctx, &c.history, 25_000, &ccx).await;
    assert_eq!(v, c.history.messages(), "the view goes out unchanged");
    let w = warnings(&mut rx);
    assert!(w[0].contains("so it wasn't made"), "{w:#?}");
    // Nothing is half-applied: the next view decides again.
    std::fs::set_permissions(&log, std::fs::Permissions::from_mode(0o600)).unwrap();
    let v = view(&ctx, &c.history, 25_000, &ccx).await;
    assert_ne!(v, c.history.messages());
    assert_eq!(view(&elide(), &c.history, 25_000, &ccx).await, v);
}

#[tokio::test]
async fn a_result_shorter_than_its_stub_is_left_alone() {
    let tmp = tempfile::tempdir().unwrap();
    // A long session path makes the stub of a 205-character result longer than it.
    let sessions = tmp
        .path()
        .join("sessions-in-a-deeply-nested-place".repeat(4));
    std::fs::create_dir(&sessions).unwrap();
    let mut c = Conv::new("Fix the parser");
    c.call("bash", json!({"command": "ls"}), "x".repeat(205));
    c.bash(5000);
    for _ in 0..3 {
        c.bash(100);
    }
    let ccx = ccx(Some(&sessions), FILE_TOOLS);
    let ctx = Context::new(ContextConfig {
        elide_min_chars: 200,
        ..Default::default()
    });
    let out = compact(&ctx, &c.history, 1_000_000, &ccx, CompactReason::Manual).await;
    assert!(out.message.contains("1 tool result"), "{}", out.message);
    let v = view(&ctx, &c.history, 1_000_000, &ccx).await;
    assert_eq!(v[2].content[0], c.history.messages()[2].content[0]);
    assert_ne!(v[4].content[0], c.history.messages()[4].content[0]);
}

#[tokio::test]
async fn the_projection_counts_the_thinking_a_trim_drops() {
    let tmp = tempfile::tempdir().unwrap();
    let mut c = Conv::new("Fix the parser");
    c.think = 1500;
    for _ in 0..30 {
        c.bash(2000);
    }
    let ccx = ccx(Some(tmp.path()), FILE_TOOLS);
    let window = 45_000;
    let before = tokens(&c.history.messages());
    assert!(before as f64 > 0.60 * window as f64, "{before}");
    let v = view(&elide(), &c.history, window, &ccx).await;
    let after = tokens(&v);
    // Dropping the thinking frees most of what's needed: few results go, and the view
    // ends near `trim_to`, not far below it.
    assert!(after as f64 <= 0.40 * window as f64, "{after}");
    assert!(after as f64 >= 0.35 * window as f64, "{after}");
}

#[tokio::test]
async fn a_call_that_copies_a_stub_written_in_this_session_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let c = long_session();
    let ctx = elide();
    let hook = ctx.marker_hook();
    let ccx = ccx(Some(tmp.path()), FILE_TOOLS);
    let v = view(&ctx, &c.history, 25_000, &ccx).await;
    let stub = v
        .iter()
        .flat_map(|m| &m.content)
        .find_map(|b| match b {
            ContentBlock::ToolResult { content, .. } if content.starts_with("[Output elided") => {
                Some(content.clone())
            }
            _ => None,
        })
        .unwrap();
    let call = |content: String| HookEvent::PreTool {
        call: ToolCall {
            id: "w1".into(),
            name: "write_file".into(),
            input: json!({"path": "notes.md", "content": content}),
        },
        read_only: false,
    };
    let copied = call(format!("The log said:\n{stub}\n"));
    assert_eq!(hook.on(&copied, &ccx.cx).await, HookDecision::deny(REFUSED));
    // Another session's stubs are not this one's; prose about stubs is fine.
    let other = ccx_for("s2", Some(tmp.path()), FILE_TOOLS);
    assert_eq!(hook.on(&copied, &other.cx).await, HookDecision::Continue);
    let prose = call("A stub looks like `[Output elided to save context: 3 lines, …]`.".into());
    assert_eq!(hook.on(&prose, &ccx.cx).await, HookDecision::Continue);
}
