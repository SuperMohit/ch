//! Kernel tests with fake plugins only.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ch_kernel::reexports::CancellationToken;
use ch_kernel::*;
use futures::stream;
use serde_json::{Value, json};

// ---------- fakes ----------

#[derive(Default)]
struct Script {
    responses: Mutex<VecDeque<Result<ModelResponse, String>>>,
    requests: Mutex<Vec<ModelRequest>>,
}

struct ScriptedProvider(Arc<Script>);

#[async_trait::async_trait]
impl ModelProvider for ScriptedProvider {
    fn id(&self) -> &str {
        "scripted"
    }
    async fn stream(
        &self,
        request: ModelRequest,
        _cancel: CancellationToken,
    ) -> Result<ModelStream, ModelError> {
        self.0.requests.lock().unwrap().push(request);
        let next = self.0.responses.lock().unwrap().pop_front();
        match next {
            Some(Ok(resp)) => {
                let mut evs = Vec::new();
                let text = resp.message.text();
                if !text.is_empty() {
                    evs.push(Ok(ModelEvent::TextDelta(text)));
                }
                evs.push(Ok(ModelEvent::Completed(resp)));
                Ok(Box::pin(stream::iter(evs)))
            }
            Some(Err(e)) if e == "overflow" => Err(ModelError::ContextOverflow("too long".into())),
            Some(Err(e)) => Err(ModelError::Unavailable(e)),
            None => Ok(Box::pin(stream::iter(vec![Ok(ModelEvent::Completed(
                text_reply("(script exhausted)"),
            ))]))),
        }
    }
    fn context_window(&self, _model: &str) -> Option<u64> {
        Some(100_000)
    }
}

fn usage() -> Usage {
    Usage {
        input_tokens: 100,
        output_tokens: 10,
        ..Default::default()
    }
}

fn text_reply(text: &str) -> ModelResponse {
    ModelResponse {
        message: Message::assistant(text),
        stop_reason: StopReason::EndTurn,
        usage: usage(),
    }
}

fn tool_reply(calls: &[(&str, &str, Value)]) -> ModelResponse {
    ModelResponse {
        message: Message {
            role: Role::Assistant,
            content: calls
                .iter()
                .map(|(id, name, input)| ContentBlock::ToolUse {
                    id: id.to_string(),
                    name: name.to_string(),
                    input: input.clone(),
                })
                .collect(),
        },
        stop_reason: StopReason::ToolUse,
        usage: usage(),
    }
}

struct EchoTool;

#[async_trait::async_trait]
impl Tool for EchoTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "echo".into(),
            description: "Echo the input".into(),
            input_schema: json!({"type":"object","properties":{"text":{"type":"string"}}}),
            read_only: true,
        }
    }
    async fn call(&self, input: Value, _cx: &Cx) -> ToolOutput {
        ToolOutput::ok(input["text"].as_str().unwrap_or("").to_string())
    }
}

struct PanicTool;

#[async_trait::async_trait]
impl Tool for PanicTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "boom".into(),
            description: "Panics".into(),
            input_schema: json!({"type":"object"}),
            read_only: false,
        }
    }
    async fn call(&self, _input: Value, _cx: &Cx) -> ToolOutput {
        panic!("kaboom")
    }
}

/// Sleeps `ms` or until cancelled.
struct SlowTool {
    name: &'static str,
    read_only: bool,
}

#[async_trait::async_trait]
impl Tool for SlowTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.into(),
            description: "Sleeps".into(),
            input_schema: json!({"type":"object","properties":{"ms":{"type":"integer"}}}),
            read_only: self.read_only,
        }
    }
    async fn call(&self, input: Value, cx: &Cx) -> ToolOutput {
        let ms = input["ms"].as_u64().unwrap_or(10);
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(ms)) => ToolOutput::ok("slept"),
            _ = cx.cancel.cancelled() => ToolOutput::error("interrupted"),
        }
    }
}

type Log = Arc<Mutex<Vec<String>>>;

struct ScriptedHook {
    id: &'static str,
    priority: i32,
    points: Vec<HookPoint>,
    log: Log,
    decide: Box<dyn Fn(&HookEvent) -> HookDecision + Send + Sync>,
}

#[async_trait::async_trait]
impl Hook for ScriptedHook {
    fn id(&self) -> &str {
        self.id
    }
    fn points(&self) -> &[HookPoint] {
        &self.points
    }
    fn priority(&self) -> i32 {
        self.priority
    }
    async fn on(&self, event: &HookEvent, _cx: &Cx) -> HookDecision {
        self.log
            .lock()
            .unwrap()
            .push(format!("{}:{}", self.id, event.point().as_str()));
        (self.decide)(event)
    }
}

struct FixedApprover(Approval, Log);

#[async_trait::async_trait]
impl Approver for FixedApprover {
    fn id(&self) -> &str {
        "fixed"
    }
    async fn ask(&self, request: &ApprovalRequest) -> Approval {
        self.1.lock().unwrap().push(format!("ask:{}", request.tool));
        self.0.clone()
    }
}

struct RecordingSink {
    events: Arc<Mutex<Vec<String>>>,
    flushed: Arc<Mutex<bool>>,
}

#[async_trait::async_trait]
impl EventSink for RecordingSink {
    fn id(&self) -> &str {
        "recorder"
    }
    async fn handle(&self, event: &AgentEvent) {
        let v = serde_json::to_value(event).unwrap();
        self.events
            .lock()
            .unwrap()
            .push(v["type"].as_str().unwrap().to_string());
    }
    async fn flush(&self) {
        *self.flushed.lock().unwrap() = true;
    }
}

struct Reminder;

#[async_trait::async_trait]
impl PromptContributor for Reminder {
    fn id(&self) -> &str {
        "reminder"
    }
    async fn system_blocks(&self, _cx: &Cx) -> Vec<String> {
        vec!["You are a test agent.".into()]
    }
    async fn turn_reminders(&self, _cx: &Cx) -> Vec<String> {
        vec!["remember the plan".into()]
    }
}

/// A plugin assembled from closures, for tests.
struct TestPlugin {
    manifest: PluginManifest,
    #[allow(clippy::type_complexity)]
    register: Box<dyn Fn(&mut RegisterCx<'_>) -> anyhow::Result<()> + Send + Sync>,
}

impl Plugin for TestPlugin {
    fn manifest(&self) -> PluginManifest {
        self.manifest.clone()
    }
    fn register(&self, cx: &mut RegisterCx<'_>) -> anyhow::Result<()> {
        (self.register)(cx)
    }
}

fn plugin(
    id: &str,
    f: impl Fn(&mut RegisterCx<'_>) -> anyhow::Result<()> + Send + Sync + 'static,
) -> Arc<dyn Plugin> {
    Arc::new(TestPlugin {
        manifest: PluginManifest::new(id, "test"),
        register: Box::new(f),
    })
}

struct Harness {
    script: Arc<Script>,
    session: Session,
}

fn config() -> Config {
    Config::parse(
        r#"
        [model]
        provider = "scripted"
        id = "test-model"
        "#,
    )
    .unwrap()
}

fn harness(
    responses: Vec<Result<ModelResponse, String>>,
    extra: Vec<Arc<dyn Plugin>>,
    cfg: Config,
) -> Harness {
    let script = Arc::new(Script {
        responses: Mutex::new(responses.into()),
        ..Default::default()
    });
    let s2 = script.clone();
    let mut plugins = vec![plugin("scripted", move |cx| {
        cx.add_provider(ScriptedProvider(s2.clone()));
        Ok(())
    })];
    plugins.push(plugin("tools", |cx| {
        cx.add_tool(EchoTool);
        cx.add_tool(PanicTool);
        cx.add_tool(SlowTool {
            name: "slow",
            read_only: true,
        });
        cx.add_tool(SlowTool {
            name: "slow_write",
            read_only: false,
        });
        Ok(())
    }));
    plugins.extend(extra);
    let registry =
        Arc::new(RegistryBuilder::build(plugins, &cfg, std::path::Path::new("/tmp")).unwrap());
    let session = Session::builder(registry, Arc::new(cfg))
        .workspace("/tmp")
        .build()
        .unwrap();
    Harness { script, session }
}

fn tool_results(msg: &Message) -> Vec<(String, String, bool)> {
    msg.content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => Some((tool_use_id.clone(), content.clone(), *is_error)),
            _ => None,
        })
        .collect()
}

// ---------- tests ----------

#[tokio::test]
async fn plain_reply_ends_turn() {
    let mut h = harness(vec![Ok(text_reply("hi there"))], vec![], config());
    let r = h.session.run_turn("hello").await;
    assert_eq!(r.reason, TerminationReason::Done);
    assert_eq!(r.final_text, "hi there");
    assert_eq!(r.steps, 1);
    assert_eq!(h.session.history().len(), 2);
    assert_eq!(h.session.usage().input_tokens, 100);
}

#[tokio::test]
async fn tool_call_round_trip_pairs_results() {
    let mut h = harness(
        vec![
            Ok(tool_reply(&[
                ("t1", "echo", json!({"text":"one"})),
                ("t2", "echo", json!({"text":"two"})),
            ])),
            Ok(text_reply("done")),
        ],
        vec![],
        config(),
    );
    let r = h.session.run_turn("go").await;
    assert_eq!(r.reason, TerminationReason::Done);
    let ev = h.session.history().events();
    assert_eq!(ev.len(), 4);
    let results = tool_results(&ev[2].message);
    assert_eq!(
        results,
        vec![
            ("t1".into(), "one".into(), false),
            ("t2".into(), "two".into(), false)
        ]
    );
    // The second request carries the tool results.
    let reqs = h.script.requests.lock().unwrap();
    assert_eq!(reqs.len(), 2);
    assert_eq!(reqs[1].messages.len(), 3);
}

#[tokio::test]
async fn unknown_tool_lists_available_tools() {
    let mut h = harness(
        vec![
            Ok(tool_reply(&[("t1", "nope", json!({}))])),
            Ok(text_reply("ok")),
        ],
        vec![],
        config(),
    );
    h.session.run_turn("go").await;
    let results = tool_results(&h.session.history().events()[2].message);
    assert!(results[0].2);
    assert!(results[0].1.contains("Unknown tool `nope`"));
    assert!(results[0].1.contains("echo"));
}

#[tokio::test]
async fn panicking_tool_becomes_error_observation() {
    let mut h = harness(
        vec![
            Ok(tool_reply(&[("t1", "boom", json!({}))])),
            Ok(text_reply("ok")),
        ],
        vec![],
        config(),
    );
    let r = h.session.run_turn("go").await;
    assert_eq!(r.reason, TerminationReason::Done);
    let results = tool_results(&h.session.history().events()[2].message);
    assert!(results[0].2);
    assert!(results[0].1.contains("kaboom"));
}

#[tokio::test]
async fn hooks_run_in_priority_order_and_first_deny_wins() {
    let log: Log = Default::default();
    let (l1, l2, l3) = (log.clone(), log.clone(), log.clone());
    let hooks = plugin("hooks", move |cx| {
        cx.add_hook(ScriptedHook {
            id: "late",
            priority: 10,
            points: vec![HookPoint::PreTool],
            log: l1.clone(),
            decide: Box::new(|_| HookDecision::note("late note")),
        });
        cx.add_hook(ScriptedHook {
            id: "early",
            priority: -10,
            points: vec![HookPoint::PreTool],
            log: l2.clone(),
            decide: Box::new(|_| HookDecision::note("early note")),
        });
        cx.add_hook(ScriptedHook {
            id: "denier",
            priority: 5,
            points: vec![HookPoint::PreTool],
            log: l3.clone(),
            decide: Box::new(|ev| match ev.tool_call() {
                Some(c) if c.input["text"] == "secret" => HookDecision::deny("no secrets"),
                _ => HookDecision::Continue,
            }),
        });
        Ok(())
    });
    let mut h = harness(
        vec![
            Ok(tool_reply(&[
                ("t1", "echo", json!({"text":"fine"})),
                ("t2", "echo", json!({"text":"secret"})),
            ])),
            Ok(text_reply("ok")),
        ],
        vec![hooks],
        config(),
    );
    h.session.run_turn("go").await;
    let log = log.lock().unwrap().clone();
    assert_eq!(
        log,
        vec![
            "early:pre_tool",
            "denier:pre_tool",
            "late:pre_tool",
            "early:pre_tool",
            "denier:pre_tool"
        ]
    );
    let results = tool_results(&h.session.history().events()[2].message);
    assert_eq!(results[0].1, "fine\n\nearly note\n\nlate note");
    assert!(!results[0].2);
    assert!(results[1].2);
    assert!(
        results[1]
            .1
            .contains("Permission denied (denier): no secrets")
    );
    // Notes collected before the deny are still attached.
    assert!(results[1].1.contains("early note"));
}

#[tokio::test]
async fn rewrite_changes_tool_input() {
    let hooks = plugin("hooks", |cx| {
        cx.add_hook(ScriptedHook {
            id: "rw",
            priority: 0,
            points: vec![HookPoint::PreTool],
            log: Default::default(),
            decide: Box::new(|_| HookDecision::Rewrite {
                input: json!({"text":"rewritten"}),
            }),
        });
        Ok(())
    });
    let mut h = harness(
        vec![
            Ok(tool_reply(&[("t1", "echo", json!({"text":"orig"}))])),
            Ok(text_reply("ok")),
        ],
        vec![hooks],
        config(),
    );
    h.session.run_turn("go").await;
    let results = tool_results(&h.session.history().events()[2].message);
    assert_eq!(results[0].1, "rewritten");
}

#[tokio::test]
async fn hooks_before_a_rewrite_judge_the_rewritten_input() {
    let log: Log = Default::default();
    let (l1, l2) = (log.clone(), log.clone());
    let hooks = plugin("hooks", move |cx| {
        // Runs first, like the permission check.
        cx.add_hook(ScriptedHook {
            id: "guard",
            priority: -100,
            points: vec![HookPoint::PreTool],
            log: l1.clone(),
            decide: Box::new(|ev| match ev.tool_call() {
                Some(c) if c.input["text"] == "danger" => HookDecision::deny("not that"),
                _ => HookDecision::Continue,
            }),
        });
        cx.add_hook(ScriptedHook {
            id: "rw",
            priority: 0,
            points: vec![HookPoint::PreTool],
            log: l2.clone(),
            decide: Box::new(|ev| match ev.tool_call() {
                Some(c) if c.input["text"] == "harmless" => HookDecision::Rewrite {
                    input: json!({"text":"danger"}),
                },
                _ => HookDecision::Continue,
            }),
        });
        Ok(())
    });
    let mut h = harness(
        vec![
            Ok(tool_reply(&[
                ("t1", "echo", json!({"text":"harmless"})),
                ("t2", "echo", json!({"text":"fine"})),
            ])),
            Ok(text_reply("ok")),
        ],
        vec![hooks],
        config(),
    );
    h.session.run_turn("go").await;
    let results = tool_results(&h.session.history().events()[2].message);
    assert!(results[0].2, "{results:?}");
    assert!(results[0].1.contains("Permission denied (guard): not that"));
    assert_eq!(results[1].1, "fine");
    // Only the rewritten call is checked twice.
    assert_eq!(
        log.lock().unwrap().clone(),
        vec![
            "guard:pre_tool",
            "rw:pre_tool",
            "guard:pre_tool",
            "guard:pre_tool",
            "rw:pre_tool",
        ]
    );
}

#[tokio::test]
async fn ask_goes_to_approver_and_always_is_remembered() {
    let log: Log = Default::default();
    let l = log.clone();
    let l2 = log.clone();
    let plugins = vec![plugin("perm", move |cx| {
        cx.add_hook(ScriptedHook {
            id: "asker",
            priority: 0,
            points: vec![HookPoint::PreTool],
            log: l.clone(),
            decide: Box::new(|_| HookDecision::ask("writes", Some("slow_write".into()))),
        });
        cx.add_approver(FixedApprover(Approval::AllowAlways, l2.clone()));
        Ok(())
    })];
    let mut h = harness(
        vec![
            Ok(tool_reply(&[("t1", "slow_write", json!({"ms":1}))])),
            Ok(tool_reply(&[("t2", "slow_write", json!({"ms":1}))])),
            Ok(text_reply("ok")),
        ],
        plugins,
        config(),
    );
    h.session.run_turn("go").await;
    let asks = log
        .lock()
        .unwrap()
        .iter()
        .filter(|l| l.starts_with("ask:"))
        .count();
    assert_eq!(asks, 1, "AllowAlways should suppress the second prompt");
    let ev = h.session.history().events();
    assert_eq!(tool_results(&ev[2].message)[0].1, "slept");
    assert_eq!(tool_results(&ev[4].message)[0].1, "slept");
}

#[tokio::test]
async fn ask_without_approver_is_denied() {
    let plugins = vec![plugin("perm", |cx| {
        cx.add_hook(ScriptedHook {
            id: "asker",
            priority: 0,
            points: vec![HookPoint::PreTool],
            log: Default::default(),
            decide: Box::new(|_| HookDecision::ask("risky", None)),
        });
        Ok(())
    })];
    let mut h = harness(
        vec![
            Ok(tool_reply(&[("t1", "echo", json!({"text":"x"}))])),
            Ok(text_reply("ok")),
        ],
        plugins,
        config(),
    );
    h.session.run_turn("go").await;
    let r = &tool_results(&h.session.history().events()[2].message)[0];
    assert!(r.2);
    assert!(r.1.contains("needs approval"));
}

/// Cancels the turn while its approval prompt is open, as Ctrl-C at the REPL does.
struct CancellingApprover(Arc<Mutex<Option<CancelHandle>>>);

#[async_trait::async_trait]
impl Approver for CancellingApprover {
    fn id(&self) -> &str {
        "cancelling"
    }
    async fn ask(&self, _request: &ApprovalRequest) -> Approval {
        self.0.lock().unwrap().as_ref().unwrap().cancel();
        std::future::pending().await
    }
}

#[tokio::test]
async fn a_call_whose_approval_is_cancelled_was_not_denied() {
    let handle: Arc<Mutex<Option<CancelHandle>>> = Default::default();
    let h2 = handle.clone();
    let plugins = vec![plugin("perm", move |cx| {
        cx.add_hook(ScriptedHook {
            id: "asker",
            priority: 0,
            points: vec![HookPoint::PreTool],
            log: Default::default(),
            decide: Box::new(|_| HookDecision::ask("risky", None)),
        });
        cx.add_approver(CancellingApprover(h2.clone()));
        Ok(())
    })];
    let mut h = harness(
        vec![
            Ok(tool_reply(&[("t1", "echo", json!({"text":"x"}))])),
            Ok(text_reply("never sent")),
        ],
        plugins,
        config(),
    );
    *handle.lock().unwrap() = Some(h.session.cancel_handle());
    let r = h.session.run_turn("go").await;
    assert_eq!(r.reason, TerminationReason::Cancelled);
    let result = &tool_results(&h.session.history().events()[2].message)[0];
    assert_eq!(
        result.1, "Not run: the user cancelled the turn.",
        "the model must not be told the user denied the call"
    );
}

#[tokio::test]
async fn cancellation_mid_tool_keeps_history_valid() {
    let mut h = harness(
        vec![Ok(tool_reply(&[
            ("t1", "slow_write", json!({"ms": 10_000})),
            ("t2", "echo", json!({"text":"never"})),
        ]))],
        vec![],
        config(),
    );
    let cancel = h.session.cancel_handle();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel.cancel();
    });
    let started = Instant::now();
    let r = h.session.run_turn("go").await;
    assert_eq!(r.reason, TerminationReason::Cancelled);
    assert!(started.elapsed() < Duration::from_secs(5));
    let ev = h.session.history().events();
    assert_eq!(ev.len(), 3);
    let results = tool_results(&ev[2].message);
    assert_eq!(results.len(), 2, "every tool_use gets a tool_result");
    assert!(results.iter().all(|r| r.2));
    // The next turn starts with a fresh token and works.
    let r = h.session.run_turn("again").await;
    assert_eq!(r.reason, TerminationReason::Done);
}

#[tokio::test]
async fn read_only_calls_run_concurrently() {
    let mut h = harness(
        vec![
            Ok(tool_reply(&[
                ("a", "slow", json!({"ms": 300})),
                ("b", "slow", json!({"ms": 300})),
                ("c", "slow", json!({"ms": 300})),
            ])),
            Ok(text_reply("ok")),
        ],
        vec![],
        config(),
    );
    let started = Instant::now();
    h.session.run_turn("go").await;
    assert!(
        started.elapsed() < Duration::from_millis(800),
        "took {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn step_limit_ends_turn() {
    let mut cfg = config();
    cfg.set_override("agent.max_steps=2").unwrap();
    let mut h = harness(
        vec![
            Ok(tool_reply(&[("a", "echo", json!({"text":"1"}))])),
            Ok(tool_reply(&[("b", "echo", json!({"text":"2"}))])),
            Ok(tool_reply(&[("c", "echo", json!({"text":"3"}))])),
        ],
        vec![],
        cfg,
    );
    let r = h.session.run_turn("go").await;
    assert_eq!(r.reason, TerminationReason::StepLimit);
    assert_eq!(r.steps, 2);
}

#[tokio::test]
async fn context_overflow_and_provider_errors_end_turn() {
    let mut h = harness(vec![Err("overflow".into())], vec![], config());
    let r = h.session.run_turn("go").await;
    assert!(matches!(
        r.reason,
        TerminationReason::ContextOverflow { .. }
    ));
    let mut h = harness(vec![Err("down".into())], vec![], config());
    let r = h.session.run_turn("go").await;
    assert!(matches!(r.reason, TerminationReason::Error { .. }));
}

#[tokio::test]
async fn reminders_go_to_request_not_history() {
    let plugins = vec![plugin("prompt", |cx| {
        cx.add_prompt(Reminder);
        Ok(())
    })];
    let mut h = harness(vec![Ok(text_reply("ok"))], plugins, config());
    h.session.run_turn("task").await;
    let reqs = h.script.requests.lock().unwrap();
    assert_eq!(reqs[0].system, vec!["You are a test agent."]);
    let last = reqs[0].messages.last().unwrap();
    assert!(last.content.iter().any(|b| matches!(b,
        ContentBlock::Text { text } if text.contains("<system-reminder>") && text.contains("remember the plan"))));
    let stored = &h.session.history().events()[0].message;
    assert_eq!(stored.content.len(), 1, "reminder must not be stored");
    // Providers are told which trailing blocks are per call, for cache breakpoints.
    assert_eq!(reqs[0].volatile_tail, 1);
}

#[tokio::test]
async fn tool_calls_cut_off_by_the_output_limit_are_not_run() {
    let mut cut = tool_reply(&[("t1", "echo", json!({"text":"partial"}))]);
    cut.stop_reason = StopReason::MaxTokens;
    let mut h = harness(vec![Ok(cut), Ok(text_reply("done"))], vec![], config());
    let r = h.session.run_turn("go").await;
    assert_eq!(r.reason, TerminationReason::Done);
    assert_eq!(r.steps, 2);
    let results = tool_results(&h.session.history().events()[2].message);
    assert_eq!(results.len(), 1);
    assert!(results[0].2);
    assert!(results[0].1.contains("output limit"), "{}", results[0].1);
    assert!(!results[0].1.contains("partial"), "the tool must not run");
}

#[tokio::test]
async fn turn_end_note_continues_the_turn_and_stop_ends_it() {
    let calls = Arc::new(Mutex::new(0));
    let c = calls.clone();
    let plugins = vec![plugin("verifier", move |cx| {
        let c = c.clone();
        cx.add_hook(ScriptedHook {
            id: "verify",
            priority: 0,
            points: vec![HookPoint::TurnEnd],
            log: Default::default(),
            decide: Box::new(move |_| {
                let mut n = c.lock().unwrap();
                *n += 1;
                if *n == 1 {
                    HookDecision::note("you forgot the tests")
                } else {
                    HookDecision::Continue
                }
            }),
        });
        Ok(())
    })];
    let mut h = harness(
        vec![Ok(text_reply("first")), Ok(text_reply("second"))],
        plugins,
        config(),
    );
    let r = h.session.run_turn("go").await;
    assert_eq!(r.reason, TerminationReason::Done);
    assert_eq!(r.steps, 2);
    assert_eq!(r.final_text, "second");
    assert!(
        h.session.history().events()[2]
            .message
            .text()
            .contains("you forgot the tests")
    );
}

#[tokio::test]
async fn post_tool_stop_ends_turn_after_step() {
    let plugins = vec![plugin("stuck", |cx| {
        cx.add_hook(ScriptedHook {
            id: "stuck",
            priority: 0,
            points: vec![HookPoint::PostTool],
            log: Default::default(),
            decide: Box::new(|_| HookDecision::stop("looping")),
        });
        Ok(())
    })];
    let mut h = harness(
        vec![Ok(tool_reply(&[("a", "echo", json!({"text":"1"}))]))],
        plugins,
        config(),
    );
    let r = h.session.run_turn("go").await;
    assert_eq!(
        r.reason,
        TerminationReason::HookStop {
            source: "stuck".into(),
            reason: "looping".into()
        }
    );
    // History is still well-formed.
    assert_eq!(
        tool_results(&h.session.history().events()[2].message).len(),
        1
    );
}

#[tokio::test]
async fn user_prompt_deny_and_rewrite() {
    let plugins = vec![plugin("guard", |cx| {
        cx.add_hook(ScriptedHook {
            id: "guard",
            priority: 0,
            points: vec![HookPoint::UserPrompt],
            log: Default::default(),
            decide: Box::new(|ev| match ev {
                HookEvent::UserPrompt { text } if text.contains("forbidden") => {
                    HookDecision::deny("nope")
                }
                _ => HookDecision::Rewrite {
                    input: json!("rewritten prompt"),
                },
            }),
        });
        Ok(())
    })];
    let mut h = harness(vec![Ok(text_reply("ok"))], plugins, config());
    let r = h.session.run_turn("forbidden thing").await;
    assert!(matches!(r.reason, TerminationReason::PromptDenied { .. }));
    assert!(h.session.history().is_empty());
    h.session.run_turn("fine").await;
    assert_eq!(
        h.session.history().events()[0].message.text(),
        "rewritten prompt"
    );
}

#[tokio::test]
async fn sinks_receive_events_and_flush_on_end() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let flushed = Arc::new(Mutex::new(false));
    let (e2, f2) = (events.clone(), flushed.clone());
    let plugins = vec![plugin("sink", move |cx| {
        cx.add_sink(RecordingSink {
            events: e2.clone(),
            flushed: f2.clone(),
        });
        Ok(())
    })];
    let mut h = harness(vec![Ok(text_reply("ok"))], plugins, config());
    h.session.run_turn("go").await;
    h.session.end().await;
    let ev = events.lock().unwrap().clone();
    assert_eq!(ev.first().map(String::as_str), Some("session_start"));
    for t in [
        "turn_start",
        "model_request",
        "text_delta",
        "assistant_message",
        "usage",
        "turn_end",
    ] {
        assert!(ev.iter().any(|e| e == t), "missing {t} in {ev:?}");
    }
    assert_eq!(ev.last().map(String::as_str), Some("session_end"));
    assert!(*flushed.lock().unwrap());
}

#[tokio::test]
async fn tool_output_is_truncated() {
    let mut cfg = config();
    cfg.set_override("agent.tool_result_max_chars=200").unwrap();
    let big = "x".repeat(5000);
    let mut h = harness(
        vec![
            Ok(tool_reply(&[("a", "echo", json!({ "text": big }))])),
            Ok(text_reply("ok")),
        ],
        vec![],
        cfg,
    );
    h.session.run_turn("go").await;
    let r = &tool_results(&h.session.history().events()[2].message)[0];
    assert!(r.1.len() < 400);
    assert!(r.1.contains("truncated"));
}

#[tokio::test]
async fn sessions_persist_and_restore() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path());
    let script = Arc::new(Script {
        responses: Mutex::new(vec![Ok(text_reply("ok"))].into()),
        ..Default::default()
    });
    let s2 = script.clone();
    let plugins = vec![plugin("scripted", move |cx| {
        cx.add_provider(ScriptedProvider(s2.clone()));
        Ok(())
    })];
    let cfg = config();
    let registry = Arc::new(RegistryBuilder::build(plugins, &cfg, dir.path()).unwrap());
    let cfg = Arc::new(cfg);
    let mut s = Session::builder(registry.clone(), cfg.clone())
        .store(store.clone())
        .build()
        .unwrap();
    s.run_turn("remember me").await;
    let id = s.id().to_string();
    let snap = store.load(&id).unwrap();
    assert_eq!(snap.meta.title, "remember me");
    let s2 = Session::builder(registry, cfg)
        .restore(snap)
        .build()
        .unwrap();
    assert_eq!(s2.history().len(), 2);
    assert_eq!(s2.turn(), 1);
}

fn prompt_guard() -> Arc<dyn Plugin> {
    plugin("guard", |cx| {
        cx.add_hook(ScriptedHook {
            id: "guard",
            priority: 0,
            points: vec![HookPoint::UserPrompt],
            log: Default::default(),
            decide: Box::new(|ev| match ev {
                HookEvent::UserPrompt { text } if text == "blocked" => HookDecision::deny("no"),
                _ => HookDecision::Continue,
            }),
        });
        Ok(())
    })
}

#[tokio::test]
async fn saved_sessions_keep_event_ids_and_turn_numbers() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path());
    let h = harness(
        vec![Ok(text_reply("one")), Ok(text_reply("two"))],
        vec![prompt_guard()],
        config(),
    );
    let (registry, cfg) = (h.session.registry().clone(), h.session.config().clone());
    let mut s = Session::builder(registry.clone(), cfg.clone())
        .store(store.clone())
        .build()
        .unwrap();
    s.run_turn("first").await;
    s.clear().unwrap();
    s.run_turn("blocked").await;
    s.run_turn("second").await;
    let snap = store.load(s.id()).unwrap();
    let ids: Vec<u64> = snap.history.events().iter().map(|e| e.id).collect();
    assert_eq!(ids, [2, 3]);
    let mut s2 = Session::builder(registry, cfg)
        .restore(snap)
        .build()
        .unwrap();
    assert_eq!(s2.turn(), 2);
    s2.run_turn("third").await;
    let last = s2.history().last().unwrap();
    assert_eq!((last.id, last.turn), (5, 3));
    for entry in std::fs::read_dir(dir.path()).unwrap() {
        let name = entry.unwrap().file_name();
        assert!(!name.to_string_lossy().ends_with(".tmp"), "{name:?}");
    }
}

#[tokio::test]
async fn model_settings_apply_only_to_the_configured_model() {
    let cfg = Config::parse(
        r#"
        [model]
        provider = "scripted"
        id = "test-model"
        context_window = 5000
        "#,
    )
    .unwrap();
    let mut h = harness(vec![], vec![], cfg);
    assert_eq!(h.session.context_window(), 5000);
    h.session.set_model("picked-model");
    let provider_window = ScriptedProvider(Default::default())
        .context_window("picked-model")
        .unwrap();
    assert_eq!(h.session.context_window(), provider_window);
}

#[tokio::test]
async fn resuming_keeps_the_model_unless_the_provider_changed_or_one_is_given() {
    let mut h = harness(vec![], vec![], config());
    h.session.set_model("picked-model");
    let snap = h.session.snapshot();
    assert_eq!(snap.meta.provider.as_deref(), Some("scripted"));

    let (registry, cfg) = (h.session.registry().clone(), h.session.config().clone());
    let resume =
        |snap: SessionSnapshot| Session::builder(registry.clone(), cfg.clone()).restore(snap);
    assert_eq!(
        resume(snap.clone()).build().unwrap().model(),
        "picked-model"
    );
    let s = resume(snap.clone()).model("flag-model").build().unwrap();
    assert_eq!(s.model(), "flag-model");
    let mut elsewhere = snap;
    elsewhere.meta.provider = Some("another-provider".into());
    assert_eq!(resume(elsewhere).build().unwrap().model(), "test-model");
}

#[tokio::test]
async fn turn_end_hooks_run_once_however_the_turn_ends() {
    let log: Log = Default::default();
    let l = log.clone();
    let plugins = vec![
        prompt_guard(),
        plugin("verifier", move |cx| {
            cx.add_hook(ScriptedHook {
                id: "verify",
                priority: 0,
                points: vec![HookPoint::TurnEnd],
                log: l.clone(),
                decide: Box::new(|_| HookDecision::stop("verified")),
            });
            Ok(())
        }),
    ];
    let mut refusal = text_reply("I can't help with that.");
    refusal.stop_reason = StopReason::Refusal;
    let mut h = harness(vec![Ok(text_reply("done")), Ok(refusal)], plugins, config());
    let turn_ends = || log.lock().unwrap().len();

    let r = h.session.run_turn("go").await;
    assert!(matches!(r.reason, TerminationReason::HookStop { .. }));
    assert_eq!(turn_ends(), 1);
    h.session.run_turn("go").await;
    assert_eq!(turn_ends(), 2);
    let r = h.session.run_turn("blocked").await;
    assert!(matches!(r.reason, TerminationReason::PromptDenied { .. }));
    assert_eq!(turn_ends(), 3);
}

#[tokio::test]
async fn a_pre_tool_stop_skips_the_rest_of_a_read_only_batch() {
    let plugins = vec![plugin("guard", |cx| {
        cx.add_hook(ScriptedHook {
            id: "guard",
            priority: 0,
            points: vec![HookPoint::PreTool],
            log: Default::default(),
            decide: Box::new(|ev| match ev.tool_call() {
                Some(c) if c.id == "b" => HookDecision::stop("enough"),
                _ => HookDecision::Continue,
            }),
        });
        Ok(())
    })];
    let mut h = harness(
        vec![Ok(tool_reply(&[
            ("a", "echo", json!({"text":"1"})),
            ("b", "echo", json!({"text":"2"})),
            ("c", "echo", json!({"text":"3"})),
        ]))],
        plugins,
        config(),
    );
    let r = h.session.run_turn("go").await;
    assert!(matches!(r.reason, TerminationReason::HookStop { .. }));
    let results = tool_results(&h.session.history().events()[2].message);
    assert_eq!(results[0], ("a".into(), "1".into(), false));
    assert!(results[1].2);
    assert!(
        results[2].2 && results[2].1.contains("stopped by a hook"),
        "{results:?}"
    );
}

#[tokio::test]
async fn hook_notes_reach_the_bus_and_after_model_notes_stay_in_their_turn() {
    let plugins = vec![plugin("noter", |cx| {
        cx.add_hook(ScriptedHook {
            id: "noter",
            priority: 0,
            points: vec![HookPoint::PreTool, HookPoint::AfterModel],
            log: Default::default(),
            decide: Box::new(|ev| match ev {
                HookEvent::PreTool { .. } => HookDecision::note("pre-tool note"),
                _ => HookDecision::note("after-model note"),
            }),
        });
        Ok(())
    })];
    let mut h = harness(
        vec![
            Ok(tool_reply(&[("a", "echo", json!({"text":"1"}))])),
            Ok(text_reply("done")),
            Ok(text_reply("second turn")),
        ],
        plugins,
        config(),
    );
    let mut rx = h.session.events().subscribe();
    h.session.run_turn("go").await;
    let mut notes = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if let AgentEvent::Note { text, .. } = ev {
            notes.push(text);
        }
    }
    assert!(notes.contains(&"pre-tool note".to_string()), "{notes:?}");
    h.session.run_turn("next").await;
    let reqs = h.script.requests.lock().unwrap();
    assert!(format!("{:?}", reqs[1].messages).contains("after-model note"));
    assert!(!format!("{:?}", reqs[2].messages).contains("after-model note"));
}

// ---------- registry validation ----------

fn build(plugins: Vec<Arc<dyn Plugin>>, cfg: &Config) -> anyhow::Result<Registry> {
    RegistryBuilder::build(plugins, cfg, std::path::Path::new("/tmp"))
}

#[test]
fn duplicate_tool_names_are_rejected() {
    let p = vec![
        plugin("a", |cx| {
            cx.add_tool(EchoTool);
            Ok(())
        }),
        plugin("b", |cx| {
            cx.add_tool(EchoTool);
            Ok(())
        }),
    ];
    let err = build(p, &Config::default()).err().unwrap().to_string();
    assert!(
        err.contains("tool `echo` is registered by both `a` and `b`"),
        "{err}"
    );
}

#[test]
fn missing_requirement_is_reported() {
    let needy: Arc<dyn Plugin> = Arc::new(TestPlugin {
        manifest: PluginManifest::new("needy", "x").requires(&["base"]),
        register: Box::new(|_| Ok(())),
    });
    let base = plugin("base", |_| Ok(()));
    let mut cfg = Config::default();
    cfg.set_override("plugins.disable=[\"base\"]").unwrap();
    let err = build(vec![needy.clone(), base.clone()], &cfg)
        .err()
        .unwrap()
        .to_string();
    assert!(err.contains("requires `base`, which is disabled"), "{err}");
    // Requirements are registered first even if listed later.
    let order = Arc::new(Mutex::new(Vec::new()));
    let (o1, o2) = (order.clone(), order.clone());
    let needy: Arc<dyn Plugin> = Arc::new(TestPlugin {
        manifest: PluginManifest::new("needy", "x").requires(&["base"]),
        register: Box::new(move |_| {
            o1.lock().unwrap().push("needy");
            Ok(())
        }),
    });
    let base = plugin("base", move |_| {
        o2.lock().unwrap().push("base");
        Ok(())
    });
    build(vec![needy, base], &Config::default()).unwrap();
    assert_eq!(*order.lock().unwrap(), vec!["base", "needy"]);
}

#[test]
fn api_version_mismatch_is_rejected() {
    let mut m = PluginManifest::new("old", "x");
    m.api_version = 0;
    let p: Arc<dyn Plugin> = Arc::new(TestPlugin {
        manifest: m,
        register: Box::new(|_| Ok(())),
    });
    let err = build(vec![p], &Config::default())
        .err()
        .unwrap()
        .to_string();
    assert!(err.contains("plugin API"), "{err}");
}

#[test]
fn unknown_plugin_in_config_and_default_off_plugins() {
    let mut cfg = Config::default();
    cfg.set_override("plugins.enable=[\"ghost\"]").unwrap();
    assert!(build(vec![plugin("a", |_| Ok(()))], &cfg).is_err());

    let off: Arc<dyn Plugin> = Arc::new(TestPlugin {
        manifest: PluginManifest::new("telemetry", "x").default_enabled(false),
        register: Box::new(|cx| {
            cx.add_tool(EchoTool);
            Ok(())
        }),
    });
    let r = build(vec![off.clone()], &Config::default()).unwrap();
    assert!(r.tools.is_empty());
    assert!(!r.is_enabled("telemetry"));
    let mut cfg = Config::default();
    cfg.set_override("plugins.enable=[\"telemetry\"]").unwrap();
    let r = build(vec![off], &cfg).unwrap();
    assert_eq!(r.tools.len(), 1);
}

#[test]
fn services_are_shared_between_plugins() {
    struct Store(u32);
    let provider = plugin("provider", |cx| cx.provide_service(Arc::new(Store(7))));
    let seen = Arc::new(Mutex::new(0));
    let s2 = seen.clone();
    let consumer: Arc<dyn Plugin> = Arc::new(TestPlugin {
        manifest: PluginManifest::new("consumer", "x").requires(&["provider"]),
        register: Box::new(move |cx| {
            *s2.lock().unwrap() = cx.service::<Store>().map(|s| s.0).unwrap_or(0);
            Ok(())
        }),
    });
    let r = build(vec![consumer, provider], &Config::default()).unwrap();
    assert_eq!(*seen.lock().unwrap(), 7);
    assert!(r.services.get::<Store>().is_some());
}

#[test]
fn single_slot_selection() {
    struct Ctx(&'static str);
    #[async_trait::async_trait]
    impl ContextStrategy for Ctx {
        fn id(&self) -> &str {
            self.0
        }
        async fn view(&self, h: &History, _b: Budget, _cx: &ContextCx) -> anyhow::Result<View> {
            Ok(View {
                messages: h.messages(),
            })
        }
    }
    let r = build(vec![], &Config::default()).unwrap();
    assert_eq!(r.context(None).unwrap().id(), "passthrough");
    let p = vec![plugin("ctx", |cx| {
        cx.add_context(Ctx("a"));
        cx.add_context(Ctx("b"));
        Ok(())
    })];
    let r = build(p, &Config::default()).unwrap();
    let err = r.context(None).err().unwrap().to_string();
    assert!(err.contains("`select.context`"), "{err}");
    assert_eq!(r.context(Some("b")).unwrap().id(), "b");
    assert!(r.context(Some("c")).is_err());
}

#[tokio::test]
async fn cancel_before_turn_applies_to_next_turn() {
    let mut h = harness(vec![Ok(text_reply("never"))], vec![], config());
    h.session.cancel_handle().cancel();
    assert!(h.session.cancel_handle().is_cancelled());
    let r = h.session.run_turn("go").await;
    assert_eq!(r.reason, TerminationReason::Cancelled);
    // The pending cancel is used up.
    let r = h.session.run_turn("again").await;
    assert_eq!(r.reason, TerminationReason::Done);
    // clear_pending drops a stray cancel.
    h.session.cancel_handle().cancel();
    h.session.cancel_handle().clear_pending();
    let r = h.session.run_turn("third").await;
    assert_eq!(r.reason, TerminationReason::Done);
}

#[tokio::test]
async fn refused_or_cut_off_responses_never_run_tools() {
    let mut refusal = tool_reply(&[("t1", "echo", json!({"text":"x"}))]);
    refusal.stop_reason = StopReason::Refusal;
    refusal
        .message
        .content
        .insert(0, ContentBlock::text("I can't help with that."));
    let mut h = harness(vec![Ok(refusal)], vec![], config());
    let r = h.session.run_turn("go").await;
    assert_eq!(r.reason, TerminationReason::Done);
    let ev = h.session.history().events();
    assert_eq!(ev.len(), 2);
    assert!(ev[1].message.tool_calls().is_empty());

    let mut cut = tool_reply(&[("t1", "echo", json!({"text":"x"}))]);
    cut.stop_reason = StopReason::ContextWindowExceeded;
    let mut h = harness(vec![Ok(cut)], vec![], config());
    let r = h.session.run_turn("go").await;
    assert!(matches!(
        r.reason,
        TerminationReason::ContextOverflow { .. }
    ));
    assert!(
        h.session.history().events()[1]
            .message
            .tool_calls()
            .is_empty()
    );
}

#[tokio::test]
async fn after_model_stop_answers_every_tool_call() {
    let plugins = vec![plugin("guard", |cx| {
        cx.add_hook(ScriptedHook {
            id: "guard",
            priority: 0,
            points: vec![HookPoint::AfterModel],
            log: Default::default(),
            decide: Box::new(|_| HookDecision::stop("enough")),
        });
        Ok(())
    })];
    let mut h = harness(
        vec![Ok(tool_reply(&[("t1", "echo", json!({"text":"x"}))]))],
        plugins,
        config(),
    );
    let r = h.session.run_turn("go").await;
    assert!(matches!(r.reason, TerminationReason::HookStop { .. }));
    let results = tool_results(&h.session.history().events()[2].message);
    assert_eq!(results.len(), 1);
    assert!(results[0].2);
}

struct NamedCommand(&'static str, bool);

#[async_trait::async_trait]
impl Command for NamedCommand {
    fn name(&self) -> &str {
        self.0
    }
    fn description(&self) -> &str {
        if self.1 { "user" } else { "core" }
    }
    fn yields_on_conflict(&self) -> bool {
        self.1
    }
    async fn run(&self, _args: &str, _cx: &mut CmdCx<'_>) -> anyhow::Result<Outcome> {
        Ok(Outcome::Message(self.description().into()))
    }
}

#[test]
fn yielding_commands_give_way() {
    let p = vec![
        plugin("user", |cx| {
            cx.add_command(NamedCommand("help", true));
            cx.add_command(NamedCommand("deploy", true));
            Ok(())
        }),
        plugin("core", |cx| {
            cx.add_command(NamedCommand("help", false));
            Ok(())
        }),
    ];
    let r = build(p, &Config::default()).unwrap();
    assert_eq!(r.commands.len(), 2);
    assert_eq!(r.command("help").unwrap().description(), "core");
    assert!(r.command("deploy").is_some());
    // Two non-yielding commands still clash.
    let p = vec![
        plugin("a", |cx| {
            cx.add_command(NamedCommand("x", false));
            Ok(())
        }),
        plugin("b", |cx| {
            cx.add_command(NamedCommand("x", false));
            Ok(())
        }),
    ];
    assert!(build(p, &Config::default()).is_err());
}

#[test]
fn host_services_are_visible_to_plugins() {
    struct Opts(&'static str);
    let seen = Arc::new(Mutex::new(String::new()));
    let s2 = seen.clone();
    let p = vec![plugin("reader", move |cx| {
        *s2.lock().unwrap() = cx
            .service::<Opts>()
            .map(|o| o.0.to_string())
            .unwrap_or_default();
        Ok(())
    })];
    let mut services = Services::default();
    services.provide(Arc::new(Opts("from host")));
    let r = RegistryBuilder::build_with_services(
        p,
        &Config::default(),
        std::path::Path::new("/tmp"),
        services,
    )
    .unwrap();
    assert_eq!(*seen.lock().unwrap(), "from host");
    assert!(r.services.get::<Opts>().is_some());
}

/// Streams one tool call slowly (input in many pieces), then replies with text.
struct SlowToolInputProvider(std::sync::atomic::AtomicUsize);

#[async_trait::async_trait]
impl ModelProvider for SlowToolInputProvider {
    fn id(&self) -> &str {
        "scripted"
    }
    async fn stream(
        &self,
        _request: ModelRequest,
        _cancel: CancellationToken,
    ) -> Result<ModelStream, ModelError> {
        use futures::StreamExt;
        let call = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mut evs = Vec::new();
        if call == 0 {
            evs.push(ModelEvent::ToolUseStart {
                id: "t1".into(),
                name: "echo".into(),
            });
            for _ in 0..8 {
                evs.push(ModelEvent::ToolInputDelta {
                    id: "t1".into(),
                    len: 1000,
                });
            }
            evs.push(ModelEvent::Completed(tool_reply(&[(
                "t1",
                "echo",
                json!({"text": "x"}),
            )])));
        } else {
            evs.push(ModelEvent::Completed(text_reply("done")));
        }
        Ok(Box::pin(futures::stream::iter(evs).then(|e| async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            Ok(e)
        })))
    }
}

#[tokio::test]
async fn tool_input_streaming_emits_throttled_progress() {
    let p = vec![
        plugin("scripted", |cx| {
            cx.add_provider(SlowToolInputProvider(Default::default()));
            Ok(())
        }),
        plugin("tools", |cx| {
            cx.add_tool(EchoTool);
            Ok(())
        }),
    ];
    let cfg = config();
    let registry = Arc::new(build(p, &cfg).unwrap());
    let mut session = Session::builder(registry, Arc::new(cfg)).build().unwrap();
    let mut rx = session.events().subscribe();
    let r = session.run_turn("go").await;
    assert_eq!(r.reason, TerminationReason::Done);

    let mut started = 0;
    let mut progress = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        match ev {
            AgentEvent::ToolInputStarted { name, .. } => {
                assert_eq!(name, "echo");
                started += 1;
            }
            AgentEvent::ToolInputProgress { bytes, .. } => progress.push(bytes),
            _ => {}
        }
    }
    assert_eq!(started, 1);
    // 8 deltas 100ms apart, at most one progress event per 250ms.
    assert!(!progress.is_empty() && progress.len() < 8, "{progress:?}");
    assert!(progress.windows(2).all(|w| w[0] < w[1]), "{progress:?}");
    assert!(*progress.last().unwrap() <= 8000);
}

#[tokio::test]
async fn clear_starts_a_new_session_and_keeps_the_old_one() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path());
    let h = harness(
        vec![Ok(text_reply("one")), Ok(text_reply("two"))],
        vec![],
        config(),
    );
    let (registry, cfg) = (h.session.registry().clone(), h.session.config().clone());
    let mut s = Session::builder(registry, cfg)
        .store(store.clone())
        .build()
        .unwrap();
    s.run_turn("first").await;
    let old_id = s.id().to_string();
    s.clear().unwrap();
    assert_ne!(s.id(), old_id);
    assert!(s.history().is_empty());
    assert_eq!(s.turn(), 0);
    s.run_turn("second").await;

    let old = store.load(&old_id).unwrap();
    assert_eq!(old.meta.title, "first");
    assert_eq!(old.history.len(), 2);
    let new = store.load(s.id()).unwrap();
    assert_eq!((new.meta.title.as_str(), new.meta.turns), ("second", 1));
    assert_eq!(store.latest().unwrap().unwrap().id, s.id());
}

#[cfg(unix)]
#[tokio::test]
async fn a_session_open_in_one_process_cant_be_opened_in_another() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path());
    let h = harness(vec![Ok(text_reply("one"))], vec![], config());
    let (registry, cfg) = (h.session.registry().clone(), h.session.config().clone());
    let open = || Session::builder(registry.clone(), cfg.clone()).store(store.clone());

    let mut first = open().build().unwrap();
    first.run_turn("first").await;
    let snap = store.load(first.id()).unwrap();
    match open().restore(snap.clone()).build() {
        Ok(_) => panic!("opened a session that is in use"),
        Err(e) => assert!(
            format!("{e:#}").contains("open in another ch process"),
            "{e:#}"
        ),
    }

    let mut other = open().build().unwrap();
    let other_id = other.id().to_string();
    assert!(other.restore(snap.clone()).is_err());
    assert_eq!(
        other.id(),
        other_id,
        "a failed /resume keeps the current session"
    );

    let first_id = first.id().to_string();
    drop(first);
    other.restore(snap).unwrap();
    assert_eq!(other.id(), first_id);
}
