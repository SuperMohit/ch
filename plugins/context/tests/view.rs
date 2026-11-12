//! The strategy's views, called directly over hand-built histories: the invariants of
//! ADR 0004 that apply to trimming.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ch_context::{Context, ContextConfig, Strategy};
use ch_kernel::util::system_reminder;
use ch_kernel::{
    AgentEvent, Budget, CompactOutcome, CompactReason, CompactRequest, ContentBlock, ContextCx,
    ContextStrategy, Cx, History, Message, ModelRequest, Role, TokenCalibrator, ToolSpec, UsageLog,
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
                        thinking: format!("thinking before call {}", self.calls),
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

/// Thinking blocks after a record's first edit, through its `at_event`, are gone; the
/// others are kept.
fn assert_thinking_rule(history: &History, view: &[Message], records: &[Value]) {
    let ranges: Vec<(u64, u64)> = records
        .iter()
        .map(|r| {
            (
                r["edits"][0]["event"].as_u64().unwrap(),
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
            .any(|&(first, at)| event.id > first && event.id <= at);
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
    for _ in 0..3 {
        c.bash(100);
    }
    let next = view(&ctx, &c.history, window, &ccx).await;
    assert_eq!(next[..first.len()], first[..]);
    assert_eq!(records(&tmp.path().join("s1")).len(), 1);

    // A new process renders the same bytes from the history and the log.
    let reloaded = view(&elide(), &c.history, window, &ccx).await;
    assert_eq!(reloaded, next);
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
    let spill = std::fs::canonicalize(tmp.path())
        .unwrap()
        .join("s1/spill/call_1-bash-ab12cd34.txt");
    std::fs::write(&spill, "the whole output").unwrap();
    let mut c = Conv::new("Fix the parser");
    c.call(
        "bash",
        json!({"command": "make"}),
        format!(
            "{}\n[… The full output is saved at {}.\nRead the rest]",
            "x".repeat(900),
            spill.display()
        ),
    );
    for _ in 0..3 {
        c.bash(100);
    }
    let ccx = ccx(Some(tmp.path()), &["bash"]);
    compact(&elide(), &c.history, 100_000, &ccx, CompactReason::Manual).await;
    let v = view(&elide(), &c.history, 100_000, &ccx).await;
    let ContentBlock::ToolResult { content, .. } = &v[2].content[0] else {
        panic!("{:?}", v[2]);
    };
    assert!(
        content.contains(&format!("The full output is saved at {}", spill.display())),
        "{content}"
    );
    assert!(
        content.contains("`tail -n +N <path> | head -n 200`"),
        "{content}"
    );
    assert!(!dir.join("context/elided").exists());
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

#[tokio::test]
async fn bad_log_records_are_ignored_with_a_warning() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("s1");
    let c = long_session();
    let ccx = ccx(Some(tmp.path()), FILE_TOOLS);
    let trimmed = view(&elide(), &c.history, 25_000, &ccx).await;
    let log = dir.join("context/log.jsonl");
    let good = std::fs::read_to_string(&log).unwrap();

    // A line that doesn't parse, and a record whose block no longer matches.
    let mut tampered: Value = serde_json::from_str(good.lines().next().unwrap()).unwrap();
    tampered["edits"][0]["hash"] = json!("0000");
    std::fs::write(&log, format!("not json\n{tampered}\n")).unwrap();
    let mut rx = ccx.cx.events.subscribe();
    let v = view(&elide(), &c.history, 1_000_000, &ccx).await;
    assert_eq!(v, c.history.messages(), "neither record applies");
    let warnings: Vec<String> = received(&mut rx)
        .into_iter()
        .filter_map(|e| match e {
            AgentEvent::Warning { message } => Some(message),
            _ => None,
        })
        .collect();
    assert_eq!(warnings.len(), 2, "{warnings:#?}");
    assert!(warnings[0].contains("record 1"), "{}", warnings[0]);
    assert!(warnings[1].contains("has changed"), "{}", warnings[1]);

    // A record from a log ahead of the saved history: its events are missing.
    std::fs::write(&log, &good).unwrap();
    let mut shorter = Conv::new("Fix the parser");
    for _ in 0..5 {
        shorter.bash(2000);
    }
    let mut rx = ccx.cx.events.subscribe();
    let v = view(&elide(), &shorter.history, 1_000_000, &ccx).await;
    assert_eq!(v, shorter.history.messages());
    let missing = received(&mut rx)
        .into_iter()
        .any(|e| matches!(e, AgentEvent::Warning { message } if message.contains("is missing")));
    assert!(missing);

    // The intact log still renders the trimmed view.
    assert_eq!(view(&elide(), &c.history, 25_000, &ccx).await, trimmed);
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
}
