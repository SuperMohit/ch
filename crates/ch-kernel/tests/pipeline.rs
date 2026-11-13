//! Kernel tests with fake plugins only.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
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
        cancel: CancellationToken,
    ) -> Result<ModelStream, ModelError> {
        self.0.requests.lock().unwrap().push(request);
        let next = self.0.responses.lock().unwrap().pop_front();
        let overflow = || ModelError::ContextOverflow("too long".into());
        match next {
            // The turn is cancelled just as the provider rejects the request.
            Some(Err(e)) if e == "overflow after cancel" => {
                cancel.cancel();
                Err(overflow())
            }
            // The rejection comes after some text, thinking or a tool call.
            Some(Err(e)) if e.starts_with("overflow after ") => {
                let first = match e.as_str() {
                    "overflow after text" => ModelEvent::TextDelta("partial".into()),
                    "overflow after thinking" => ModelEvent::ThinkingDelta("hmm".into()),
                    "overflow after a tool call" => ModelEvent::ToolUseStart {
                        id: "t1".into(),
                        name: "echo".into(),
                    },
                    other => panic!("unknown script entry `{other}`"),
                };
                Ok(Box::pin(stream::iter(vec![Ok(first), Err(overflow())])))
            }
            Some(Ok(resp)) => {
                let mut evs = Vec::new();
                let text = resp.message.text();
                if !text.is_empty() {
                    evs.push(Ok(ModelEvent::TextDelta(text)));
                }
                evs.push(Ok(ModelEvent::Completed(resp)));
                Ok(Box::pin(stream::iter(evs)))
            }
            Some(Err(e)) if e == "overflow" => Err(overflow()),
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
async fn post_tool_hooks_see_the_whole_output_and_may_replace_it() {
    let log: Log = Default::default();
    let seen: Arc<Mutex<Vec<usize>>> = Default::default();
    let (l1, l2, l3, s) = (log.clone(), log.clone(), log.clone(), seen.clone());
    let hooks = plugin("hooks", move |cx| {
        let s = s.clone();
        cx.add_hook(ScriptedHook {
            id: "count",
            priority: 0,
            points: vec![HookPoint::PostTool],
            log: l1.clone(),
            decide: Box::new(move |ev| {
                if let HookEvent::PostTool { output, .. } = ev {
                    s.lock().unwrap().push(output.content.len());
                }
                HookDecision::note("counted")
            }),
        });
        cx.add_hook(ScriptedHook {
            id: "replace",
            priority: 100,
            points: vec![HookPoint::PostTool],
            log: l2.clone(),
            decide: Box::new(|ev| match ev {
                HookEvent::PostTool { call, .. } if call.id == "t1" => HookDecision::Rewrite {
                    input: json!("short preview"),
                },
                _ => HookDecision::Continue,
            }),
        });
        cx.add_hook(ScriptedHook {
            id: "after",
            priority: 200,
            points: vec![HookPoint::PostTool],
            log: l3.clone(),
            decide: Box::new(|ev| match ev {
                HookEvent::PostTool { output, .. } => {
                    HookDecision::note(format!("saw {} chars", output.content.len()))
                }
                _ => HookDecision::Continue,
            }),
        });
        Ok(())
    });
    let long = "x".repeat(500);
    let cfg = Config::parse(
        r#"
        [model]
        provider = "scripted"
        id = "test-model"
        [agent]
        tool_result_max_chars = 100
        "#,
    )
    .unwrap();
    let mut h = harness(
        vec![
            Ok(tool_reply(&[
                ("t1", "echo", json!({"text": long})),
                ("t2", "echo", json!({"text": long})),
            ])),
            Ok(text_reply("ok")),
        ],
        vec![hooks],
        cfg,
    );
    h.session.run_turn("go").await;

    // Every hook saw the whole output, each hook ran once per call (a post_tool
    // rewrite doesn't re-run the chain), and the later hook saw the replacement.
    assert_eq!(*seen.lock().unwrap(), [500, 500]);
    assert_eq!(
        *log.lock().unwrap(),
        [
            "count:post_tool",
            "replace:post_tool",
            "after:post_tool",
            "count:post_tool",
            "replace:post_tool",
            "after:post_tool"
        ]
    );
    let results = tool_results(&h.session.history().events()[2].message);
    assert_eq!(results[0].1, "short preview\n\ncounted\n\nsaw 13 chars");
    // Without a replacement the kernel still cuts the output, then adds the notes.
    let (cut, notes) = results[1].1.split_once("\n\ncounted").unwrap();
    assert!(cut.chars().count() <= 100, "{cut}");
    assert_eq!(notes, "\n\nsaw 500 chars");
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

/// Each approval request's reason and key.
type Asked = Arc<Mutex<Vec<(String, Option<String>)>>>;

/// Records each approval request and answers "always".
struct AlwaysApprover(Asked);

#[async_trait::async_trait]
impl Approver for AlwaysApprover {
    fn id(&self) -> &str {
        "always"
    }
    async fn ask(&self, request: &ApprovalRequest) -> Approval {
        let asked = (request.reason.clone(), request.key.clone());
        self.0.lock().unwrap().push(asked);
        Approval::AllowAlways
    }
}

#[tokio::test]
async fn always_allow_covers_only_the_hook_that_asked() {
    let asked: Asked = Default::default();
    let a2 = asked.clone();
    let plugins = vec![plugin("perm", move |cx| {
        cx.add_hook(ScriptedHook {
            id: "perm",
            priority: 0,
            points: vec![HookPoint::PreTool],
            log: Default::default(),
            decide: Box::new(|_| HookDecision::ask("writes", Some("slow_write".into()))),
        });
        cx.add_hook(ScriptedHook {
            id: "extra",
            priority: 1,
            points: vec![HookPoint::PreTool],
            log: Default::default(),
            decide: Box::new(|ev| match ev {
                HookEvent::PreTool { call, .. } if call.input["ms"] == 2 => {
                    HookDecision::ask("extra check", None)
                }
                _ => HookDecision::Continue,
            }),
        });
        cx.add_approver(AlwaysApprover(a2.clone()));
        Ok(())
    })];
    let call = |id, ms| Ok(tool_reply(&[(id, "slow_write", json!({ "ms": ms }))]));
    let mut h = harness(
        vec![
            call("t1", 2),
            call("t2", 1),
            call("t3", 2),
            call("t4", 1),
            Ok(text_reply("ok")),
        ],
        plugins,
        config(),
    );
    h.session.run_turn("go").await;
    assert_eq!(
        *asked.lock().unwrap(),
        [
            // Both hooks ask: one prompt, and no single key covers both.
            ("writes; extra check".to_string(), None),
            ("writes".to_string(), Some("slow_write".to_string())),
            // "slow_write" is always allowed now, but the other hook still asks.
            ("extra check".to_string(), None),
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
    // The passthrough strategy can't compact, so nothing is sent again.
    let mut h = harness(
        vec![Err("overflow".into()), Ok(text_reply("never"))],
        vec![],
        config(),
    );
    let r = h.session.run_turn("go").await;
    assert!(matches!(
        r.reason,
        TerminationReason::ContextOverflow { .. }
    ));
    assert_eq!(h.script.requests.lock().unwrap().len(), 1);
    let mut h = harness(vec![Err("down".into())], vec![], config());
    let r = h.session.run_turn("go").await;
    assert!(matches!(r.reason, TerminationReason::Error { .. }));
}

/// What `TestContext::compact` was asked, and what its `ContextCx` held.
#[derive(Debug, Clone)]
struct SeenCompaction {
    request: CompactRequest,
    provider: String,
    model: String,
    summarizer: Option<(String, String)>,
}

/// The block a compacted view starts with.
const COMPACTED: &str = "[compacted]";

/// Sends the whole history, and once a compaction changed something, starts the first
/// message with `COMPACTED` (when it isn't also the last). A compaction is recorded,
/// and can summarize first (through `ContextCx::complete`), then cancel the turn or
/// fail. A view can summarize or cancel the turn too.
#[derive(Clone, Default)]
struct TestContext {
    changes: bool,
    summarizes: bool,
    cancels: bool,
    fails: bool,
    view_summarizes: bool,
    view_cancels: bool,
    compacted: Arc<AtomicBool>,
    asked: Arc<Mutex<Vec<SeenCompaction>>>,
}

impl TestContext {
    fn plugin(&self) -> Arc<dyn Plugin> {
        let ctx = self.clone();
        plugin("test-context", move |cx| {
            cx.add_context(ctx.clone());
            Ok(())
        })
    }

    fn reasons(&self) -> Vec<CompactReason> {
        let asked = self.asked.lock().unwrap();
        asked.iter().map(|a| a.request.reason).collect()
    }

    /// Sends a summary request the way a real strategy would.
    async fn summarize(&self, h: &History, budget: &Budget, cx: &ContextCx) -> anyhow::Result<()> {
        let (provider, model) = match &cx.summarizer {
            Some((p, m)) => (p.clone(), m.clone()),
            None => (cx.provider.clone(), cx.agent_request.model.clone()),
        };
        let mut messages = h.messages();
        messages.push(Message::user("Summarize the conversation."));
        let request = ModelRequest {
            model,
            messages,
            purpose: "context.summarize".into(),
            ..cx.agent_request.clone()
        };
        cx.complete(&*provider, request, budget).await?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl ContextStrategy for TestContext {
    fn id(&self) -> &str {
        "test-context"
    }
    async fn view(&self, h: &History, b: Budget, cx: &ContextCx) -> anyhow::Result<View> {
        if self.view_summarizes {
            self.summarize(h, &b, cx).await?;
        }
        if self.view_cancels {
            cx.cx.cancel.cancel();
        }
        let mut messages = h.messages();
        if self.compacted.load(Ordering::SeqCst) && messages.len() > 1 {
            messages[0].content.insert(0, ContentBlock::text(COMPACTED));
        }
        Ok(View { messages })
    }
    async fn compact(
        &self,
        h: &History,
        b: Budget,
        request: &CompactRequest,
        cx: &ContextCx,
    ) -> anyhow::Result<CompactOutcome> {
        self.asked.lock().unwrap().push(SeenCompaction {
            request: request.clone(),
            provider: cx.provider.id().to_string(),
            model: cx.agent_request.model.clone(),
            summarizer: cx
                .summarizer
                .as_ref()
                .map(|(p, m)| (p.id().to_string(), m.clone())),
        });
        if self.summarizes {
            self.summarize(h, &b, cx).await?;
        }
        if self.cancels {
            cx.cx.cancel.cancel();
        }
        if self.fails {
            anyhow::bail!("the disk is full");
        }
        self.compacted.store(self.changes, Ordering::SeqCst);
        Ok(CompactOutcome {
            changed: self.changes,
            message: "compacted".into(),
        })
    }
}

fn changes() -> TestContext {
    TestContext {
        changes: true,
        ..Default::default()
    }
}

fn is_overflow(r: &TurnResult) -> bool {
    matches!(r.reason, TerminationReason::ContextOverflow { .. })
}

/// Drains the events received so far.
fn received(rx: &mut tokio::sync::broadcast::Receiver<AgentEvent>) -> Vec<AgentEvent> {
    std::iter::from_fn(|| rx.try_recv().ok()).collect()
}

fn usage_purposes(events: &[AgentEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Usage { purpose, .. } => Some(purpose.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn an_overflow_compacts_and_sends_the_step_again_once() {
    // Each step may compact once.
    let ctx = changes();
    let mut h = harness(
        vec![
            Err("overflow".into()),
            Ok(tool_reply(&[("a", "echo", json!({"text": "1"}))])),
            Err("overflow".into()),
            Ok(text_reply("done")),
        ],
        vec![ctx.plugin()],
        config(),
    );
    let r = h.session.run_turn("go").await;
    assert_eq!((r.reason, r.steps), (TerminationReason::Done, 2));
    assert_eq!(ctx.reasons(), [CompactReason::Overflow; 2]);
    assert_eq!(h.script.requests.lock().unwrap().len(), 4);

    // A second overflow in the same step ends the turn.
    let ctx = changes();
    let mut h = harness(
        vec![
            Err("overflow".into()),
            Err("overflow".into()),
            Ok(text_reply("never")),
        ],
        vec![ctx.plugin()],
        config(),
    );
    let r = h.session.run_turn("go").await;
    assert!(is_overflow(&r), "{r:?}");
    assert_eq!(h.script.requests.lock().unwrap().len(), 2);
    assert_eq!(ctx.reasons(), [CompactReason::Overflow]);
}

#[tokio::test]
async fn an_overflow_ends_the_turn_when_compacting_changes_nothing() {
    let ctx = TestContext::default();
    let mut h = harness(
        vec![Err("overflow".into()), Ok(text_reply("never"))],
        vec![ctx.plugin()],
        config(),
    );
    let r = h.session.run_turn("go").await;
    assert!(is_overflow(&r), "{r:?}");
    assert_eq!(ctx.reasons(), [CompactReason::Overflow]);
    assert_eq!(h.script.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn an_overflow_retry_sends_the_compacted_view_and_stores_nothing_twice() {
    let log: Log = Default::default();
    let l = log.clone();
    let ctx = changes();
    let plugins = vec![
        plugin("prompt", |cx| {
            cx.add_prompt(Reminder);
            Ok(())
        }),
        plugin("noter", move |cx| {
            cx.add_hook(ScriptedHook {
                id: "noter",
                priority: 0,
                points: vec![HookPoint::BeforeModel],
                log: l.clone(),
                decide: Box::new(|_| HookDecision::note("check the build")),
            });
            Ok(())
        }),
        ctx.plugin(),
    ];
    let mut h = harness(
        vec![
            Ok(tool_reply(&[("a", "echo", json!({"text": "1"}))])),
            Err("overflow".into()),
            Ok(text_reply("done")),
        ],
        plugins,
        config(),
    );
    let r = h.session.run_turn("task").await;
    assert_eq!(r.reason, TerminationReason::Done);
    // The hooks ran once per step, not for the retry.
    assert_eq!(*log.lock().unwrap(), ["noter:before_model"; 2]);
    let reqs = h.script.requests.lock().unwrap();
    assert_eq!(reqs.len(), 3);
    let (rejected, retry) = (&reqs[1], &reqs[2]);
    // The retry is the compacted view, with the rest of the request unchanged.
    assert_ne!(retry, rejected);
    assert_eq!(retry.messages[0].content[0], ContentBlock::text(COMPACTED));
    assert_eq!(
        retry.messages[0].content[1..],
        rejected.messages[0].content[..]
    );
    assert_eq!(retry.messages[1..], rejected.messages[1..]);
    assert_eq!(
        (&retry.model, &retry.system, &retry.tools),
        (&rejected.model, &rejected.system, &rejected.tools)
    );
    // It ends with the step's results, reminder and note, stored once.
    let results = &h.session.history().events()[2].message;
    assert_eq!(retry.messages.last(), Some(results));
    let sent = format!("{results:?}");
    assert_eq!(sent.matches("remember the plan").count(), 1);
    assert_eq!(sent.matches("check the build").count(), 1);
    let stored = format!("{:?}", h.session.history().messages());
    assert_eq!(stored.matches("remember the plan").count(), 2);
    assert_eq!(stored.matches("check the build").count(), 2);
}

#[tokio::test]
async fn an_overflow_after_output_streamed_ends_the_turn() {
    // The partial answer was shown already, so it isn't sent again.
    for script in [
        "overflow after text",
        "overflow after thinking",
        "overflow after a tool call",
    ] {
        let ctx = changes();
        let mut h = harness(
            vec![Err(script.into()), Ok(text_reply("never"))],
            vec![ctx.plugin()],
            config(),
        );
        let r = h.session.run_turn("go").await;
        assert!(is_overflow(&r), "{script}: {r:?}");
        assert!(ctx.reasons().is_empty(), "{script}");
        assert_eq!(h.script.requests.lock().unwrap().len(), 1, "{script}");
    }
}

#[tokio::test]
async fn a_cancel_before_or_during_the_overflow_compaction_ends_the_turn() {
    let cancels = |ctx: TestContext| TestContext {
        cancels: true,
        ..ctx
    };
    let failing = TestContext {
        fails: true,
        ..Default::default()
    };
    let overflow = vec![CompactReason::Overflow];
    for (name, ctx, script, reasons) in [
        ("changed", cancels(changes()), "overflow", overflow.clone()),
        (
            "unchanged",
            cancels(Default::default()),
            "overflow",
            overflow.clone(),
        ),
        ("failed", cancels(failing), "overflow", overflow),
        ("before", changes(), "overflow after cancel", vec![]),
    ] {
        let mut h = harness(
            vec![Err(script.into()), Ok(text_reply("never"))],
            vec![ctx.plugin()],
            config(),
        );
        let r = h.session.run_turn("go").await;
        assert_eq!(r.reason, TerminationReason::Cancelled, "{name}");
        assert_eq!(ctx.reasons(), reasons, "{name}");
        assert_eq!(h.script.requests.lock().unwrap().len(), 1, "{name}");
    }
}

#[tokio::test]
async fn a_failed_overflow_compaction_warns_and_ends_the_turn() {
    let ctx = TestContext {
        fails: true,
        ..changes()
    };
    let mut h = harness(
        vec![Err("overflow".into()), Ok(text_reply("never"))],
        vec![ctx.plugin()],
        config(),
    );
    let mut rx = h.session.events().subscribe();
    let r = h.session.run_turn("go").await;
    assert!(is_overflow(&r), "{r:?}");
    assert_eq!(h.script.requests.lock().unwrap().len(), 1);
    let warned = received(&mut rx).into_iter().any(
        |e| matches!(e, AgentEvent::Warning { message } if message.contains("the disk is full")),
    );
    assert!(warned);
}

#[tokio::test]
async fn a_cancel_during_the_view_ends_the_turn_before_anything_is_stored() {
    let ctx = TestContext {
        view_cancels: true,
        ..Default::default()
    };
    let prompt = plugin("prompt", |cx| {
        cx.add_prompt(Reminder);
        Ok(())
    });
    let mut h = harness(
        vec![Ok(text_reply("never"))],
        vec![prompt, ctx.plugin()],
        config(),
    );
    let mut rx = h.session.events().subscribe();
    let r = h.session.run_turn("go").await;
    assert_eq!(r.reason, TerminationReason::Cancelled);
    // The call's reminder wasn't stored, and nothing was sent.
    assert_eq!(h.session.history().events()[0].message.content.len(), 1);
    let requested = received(&mut rx)
        .into_iter()
        .any(|e| matches!(e, AgentEvent::ModelRequest { .. }));
    assert!(!requested);
}

#[tokio::test]
async fn a_strategy_s_model_calls_count_however_the_compaction_ends() {
    // Each call uses 100 input tokens; the rejected call reports none.
    let summarizes = || TestContext {
        summarizes: true,
        ..changes()
    };
    let fails = TestContext {
        fails: true,
        ..summarizes()
    };
    let cancels = TestContext {
        cancels: true,
        ..summarizes()
    };
    for (name, ctx, input) in [
        ("compacts", summarizes(), 200),
        ("fails", fails, 100),
        ("cancels", cancels, 100),
    ] {
        let mut h = harness(
            vec![
                Err("overflow".into()),
                Ok(text_reply("a summary")),
                Ok(text_reply("done")),
            ],
            vec![ctx.plugin()],
            config(),
        );
        let mut rx = h.session.events().subscribe();
        h.session.run_turn("go").await;
        assert_eq!(h.session.usage().input_tokens, input, "{name}");
        let purposes = usage_purposes(&received(&mut rx));
        assert_eq!(purposes[0], "context.summarize", "{name}");
    }

    // A view's own calls count too, before the agent's.
    let ctx = TestContext {
        view_summarizes: true,
        ..Default::default()
    };
    let mut h = harness(
        vec![Ok(text_reply("a summary")), Ok(text_reply("done"))],
        vec![ctx.plugin()],
        config(),
    );
    let mut rx = h.session.events().subscribe();
    h.session.run_turn("go").await;
    assert_eq!(h.session.usage().input_tokens, 200);
    let purposes = usage_purposes(&received(&mut rx));
    assert_eq!(purposes, ["context.summarize", "agent"]);

    // Also when the turn is cancelled while the view is built.
    let ctx = TestContext {
        view_summarizes: true,
        view_cancels: true,
        ..Default::default()
    };
    let mut h = harness(
        vec![Ok(text_reply("a summary")), Ok(text_reply("never"))],
        vec![ctx.plugin()],
        config(),
    );
    let mut rx = h.session.events().subscribe();
    let r = h.session.run_turn("go").await;
    assert_eq!(r.reason, TerminationReason::Cancelled);
    assert_eq!(h.session.usage().input_tokens, 100);
    let purposes = usage_purposes(&received(&mut rx));
    assert_eq!(purposes, ["context.summarize"]);
}

#[tokio::test]
async fn a_strategy_s_model_calls_carry_the_turn_and_step() {
    let ctx = TestContext {
        view_summarizes: true,
        ..Default::default()
    };
    let mut h = harness(
        vec![
            Ok(text_reply("a summary")),
            Ok(text_reply("first")),
            Ok(text_reply("a summary")),
            Ok(tool_reply(&[("a", "echo", json!({"text": "1"}))])),
            Ok(text_reply("a summary")),
            Ok(text_reply("done")),
        ],
        vec![ctx.plugin()],
        config(),
    );
    h.session.run_turn("one").await;
    let mut rx = h.session.events().subscribe();
    h.session.run_turn("two").await;
    let requests: Vec<_> = received(&mut rx)
        .into_iter()
        .filter_map(|e| match e {
            AgentEvent::ModelRequest {
                purpose,
                turn,
                step,
                ..
            } => Some((purpose, turn, step)),
            _ => None,
        })
        .collect();
    let summary = || "context.summarize".to_string();
    let agent = || "agent".to_string();
    assert_eq!(
        requests,
        [
            (summary(), 2, 0),
            (agent(), 2, 0),
            (summary(), 2, 1),
            (agent(), 2, 1)
        ]
    );
}

#[tokio::test]
async fn a_strategy_s_own_model_calls_count_in_the_totals() {
    let cfg = Config::parse(
        r#"
        [model]
        provider = "scripted"
        id = "test-model"
        [model.pricing]
        input = 10000.0
        output = 10000.0
        "#,
    )
    .unwrap();
    let ctx = TestContext {
        summarizes: true,
        ..changes()
    };
    let prompt = plugin("prompt", |cx| {
        cx.add_prompt(Reminder);
        Ok(())
    });
    let mut h = harness(
        vec![Ok(text_reply("hi")), Ok(text_reply("a summary"))],
        vec![prompt, ctx.plugin()],
        cfg,
    );
    let mut rx = h.session.events().subscribe();
    h.session.run_turn("task").await;
    assert!(h.session.compact(None).await.unwrap().changed);
    {
        // The summary replays the agent's request: the same model, system and tools.
        let reqs = h.script.requests.lock().unwrap();
        let (agent, summary) = (&reqs[0], &reqs[1]);
        assert_eq!(summary.purpose, "context.summarize");
        assert_eq!(
            (&summary.model, &summary.system, &summary.tools),
            (&agent.model, &agent.system, &agent.tools)
        );
    }
    // Each call uses 100 input and 10 output tokens, at $0.01 a token.
    assert_eq!(h.session.usage().input_tokens, 200);
    let cost = h.session.cost();
    assert!((cost - 2.2).abs() < 1e-9, "{cost}");
    let saved = h.session.snapshot();
    assert_eq!((saved.usage, saved.cost), (h.session.usage(), cost));

    let events = received(&mut rx);
    let requested: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ModelRequest { purpose, model, .. } => {
                Some((purpose.as_str(), model.as_str()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        requested,
        [("agent", "test-model"), ("context.summarize", "test-model")]
    );
    assert_eq!(usage_purposes(&events), ["agent", "context.summarize"]);
}

#[tokio::test]
async fn pricing_goes_by_each_call_s_model() {
    let priced = |summarizer: &str| {
        Config::parse(&format!(
            "[model]\nprovider = \"scripted\"\nid = \"test-model\"\n\
             [model.pricing]\ninput = 10000.0\noutput = 10000.0\n\
             [model.summarizer]\nid = \"{summarizer}\""
        ))
        .unwrap()
    };
    let summarizes = || TestContext {
        summarizes: true,
        ..Default::default()
    };
    // A summary by another model costs nothing, though the agent's model is priced.
    let mut h = harness(
        vec![Ok(text_reply("hi")), Ok(text_reply("a summary"))],
        vec![summarizes().plugin()],
        priced("small-model"),
    );
    h.session.run_turn("task").await;
    h.session.compact(None).await.unwrap();
    assert_eq!(h.session.usage().input_tokens, 200);
    assert!(
        (h.session.cost() - 1.1).abs() < 1e-9,
        "{}",
        h.session.cost()
    );

    // A summary by the configured model is priced, though the agent switched away.
    let mut h = harness(
        vec![Ok(text_reply("hi")), Ok(text_reply("a summary"))],
        vec![summarizes().plugin()],
        priced("test-model"),
    );
    h.session.set_model("picked-model");
    h.session.run_turn("task").await;
    h.session.compact(None).await.unwrap();
    assert_eq!(h.session.usage().input_tokens, 200);
    assert!(
        (h.session.cost() - 1.1).abs() < 1e-9,
        "{}",
        h.session.cost()
    );
}

#[tokio::test]
async fn a_compaction_s_cost_is_saved_even_without_a_turn_after_it() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path());
    let ctx = TestContext {
        summarizes: true,
        ..changes()
    };
    let purposes: Arc<Mutex<Vec<String>>> = Default::default();
    let p = purposes.clone();
    let sink = plugin("usage-sink", move |cx| {
        cx.add_sink(UsageSink(p.clone()));
        Ok(())
    });
    let h = harness(
        vec![Ok(text_reply("hi")), Ok(text_reply("a summary"))],
        vec![ctx.plugin(), sink],
        config(),
    );
    let (registry, cfg) = (h.session.registry().clone(), h.session.config().clone());
    let open = |snap: Option<SessionSnapshot>| {
        let b = Session::builder(registry.clone(), cfg.clone()).store(store.clone());
        let b = match snap {
            Some(s) => b.restore(s),
            None => b,
        };
        b.build().unwrap()
    };
    let mut s = open(None);
    s.run_turn("task").await;
    s.end().await;
    let id = s.id().to_string();
    drop(s);
    purposes.lock().unwrap().clear();

    // As after `ch --continue`: the first thing in the session is `/compact`.
    let mut s = open(Some(store.load(&id).unwrap()));
    s.compact(None).await.unwrap();
    assert_eq!(store.load(&id).unwrap().usage.input_tokens, 200);
    s.end().await;
    assert_eq!(*purposes.lock().unwrap(), ["context.summarize"]);
}

/// Records the purpose of each `Usage` event.
struct UsageSink(Arc<Mutex<Vec<String>>>);

#[async_trait::async_trait]
impl EventSink for UsageSink {
    fn id(&self) -> &str {
        "usage"
    }
    async fn handle(&self, event: &AgentEvent) {
        if let AgentEvent::Usage { purpose, .. } = event {
            self.0.lock().unwrap().push(purpose.clone());
        }
    }
}

/// Two more providers, one with a default model and one without.
fn other_providers() -> Arc<dyn Plugin> {
    plugin("others", |cx| {
        cx.add_provider(Named("other", Some("other-default")));
        cx.add_provider(Named("plain", None));
        Ok(())
    })
}

/// The summarizer a strategy is given under the config `table`, before and after
/// `/model`.
async fn summarizer_for(table: &str) -> Option<(String, String)> {
    let cfg = Config::parse(&format!(
        "[model]\nprovider = \"scripted\"\nid = \"test-model\"\n{table}"
    ))
    .unwrap();
    let ctx = TestContext::default();
    let mut h = harness(vec![], vec![other_providers(), ctx.plugin()], cfg);
    h.session.compact(None).await.unwrap();
    h.session.set_model("picked-model");
    h.session.compact(None).await.unwrap();
    let asked = ctx.asked.lock().unwrap().clone();
    // The agent's provider and current model are there either way.
    let agent: Vec<_> = asked
        .iter()
        .map(|a| (a.provider.as_str(), a.model.as_str()))
        .collect();
    assert_eq!(
        agent,
        [("scripted", "test-model"), ("scripted", "picked-model")]
    );
    assert_eq!(asked[0].summarizer, asked[1].summarizer, "{table}");
    asked[0].summarizer.clone()
}

#[tokio::test]
async fn the_summarizer_is_set_only_by_its_config() {
    let named = |p: &str, m: &str| Some((p.to_string(), m.to_string()));

    assert_eq!(summarizer_for("").await, None);
    assert_eq!(summarizer_for("[model.summarizer]").await, None);
    assert_eq!(
        summarizer_for("[model.summarizer]\nid = \"small-model\"").await,
        named("scripted", "small-model")
    );
    // A provider without an id means that provider's default model.
    assert_eq!(
        summarizer_for("[model.summarizer]\nprovider = \"other\"").await,
        named("other", "other-default")
    );
    assert_eq!(
        summarizer_for("[model.summarizer]\nprovider = \"other\"\nid = \"m\"").await,
        named("other", "m")
    );

    // One with no default model needs an id.
    let cfg = Config::parse(
        "[model]\nprovider = \"scripted\"\nid = \"test-model\"\n\
         [model.summarizer]\nprovider = \"plain\"",
    )
    .unwrap();
    let scripted = plugin("scripted", |cx| {
        cx.add_provider(ScriptedProvider(Default::default()));
        Ok(())
    });
    let registry = Arc::new(build(vec![scripted, other_providers()], &cfg).unwrap());
    let err = Session::builder(registry, Arc::new(cfg))
        .build()
        .err()
        .unwrap();
    assert!(
        format!("{err:#}").contains("`plain` has no default model; set model.summarizer.id"),
        "{err:#}"
    );
}

/// Signed thinking stays valid only while everything before it is unchanged, so each
/// request must repeat the one before it exactly and only add to it.
fn assert_each_request_extends_the_last(reqs: &[ModelRequest]) {
    for (i, pair) in reqs.windows(2).enumerate() {
        let (before, after) = (&pair[0].messages, &pair[1].messages);
        assert!(after.len() > before.len(), "request {} shrank", i + 1);
        assert_eq!(
            after[..before.len()],
            before[..],
            "request {} changed",
            i + 1
        );
    }
}

#[tokio::test]
async fn reminders_and_notes_are_stored_so_each_request_extends_the_last() {
    let plugins = vec![
        plugin("prompt", |cx| {
            cx.add_prompt(Reminder);
            Ok(())
        }),
        plugin("noter", |cx| {
            cx.add_hook(ScriptedHook {
                id: "noter",
                priority: 0,
                points: vec![HookPoint::BeforeModel],
                log: Default::default(),
                decide: Box::new(|_| HookDecision::note("check the build")),
            });
            Ok(())
        }),
    ];
    let mut h = harness(
        vec![
            Ok(tool_reply(&[("a", "echo", json!({"text": "1"}))])),
            Ok(text_reply("done")),
            Ok(text_reply("again")),
        ],
        plugins,
        config(),
    );
    h.session.run_turn("task").await;
    h.session.run_turn("more").await;
    let reqs = h.script.requests.lock().unwrap();
    assert_eq!(reqs.len(), 3);
    assert_eq!(reqs[0].system, vec!["You are a test agent."]);
    let texts = |m: &Message| -> Vec<String> {
        m.content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect()
    };
    let first = texts(reqs[0].messages.last().unwrap());
    assert_eq!(first.len(), 3, "{first:?}");
    assert!(first[1].contains("<system-reminder>") && first[1].contains("remember the plan"));
    assert!(first[2].contains("check the build"));
    // Within a turn and across turns.
    assert_each_request_extends_the_last(&reqs);
    // The history holds exactly what was sent, so a resumed session sends it again.
    let sent = &reqs[2].messages;
    assert_eq!(h.session.history().messages()[..sent.len()], sent[..]);
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
    assert_each_request_extends_the_last(&h.script.requests.lock().unwrap());
}

#[tokio::test]
async fn a_call_a_hook_stops_stores_nothing() {
    let plugins = vec![
        plugin("prompt", |cx| {
            cx.add_prompt(Reminder);
            Ok(())
        }),
        plugin("gate", |cx| {
            let calls = Mutex::new(0);
            cx.add_hook(ScriptedHook {
                id: "gate",
                priority: 0,
                points: vec![HookPoint::BeforeModel],
                log: Default::default(),
                decide: Box::new(move |_| {
                    let mut n = calls.lock().unwrap();
                    *n += 1;
                    if *n == 1 {
                        HookDecision::stop("not now")
                    } else {
                        HookDecision::Continue
                    }
                }),
            });
            Ok(())
        }),
    ];
    let mut h = harness(vec![Ok(text_reply("ok"))], plugins, config());
    let r = h.session.run_turn("first").await;
    assert!(matches!(r.reason, TerminationReason::HookStop { .. }));
    // The stopped call's reminder was never sent, so it isn't in the history.
    assert_eq!(h.session.history().events()[0].message.content.len(), 1);
    h.session.run_turn("second").await;
    let reqs = h.script.requests.lock().unwrap();
    assert_eq!(reqs.len(), 1);
    assert_eq!(
        format!("{:?}", reqs[0].messages)
            .matches("remember the plan")
            .count(),
        1
    );
}

#[tokio::test]
async fn repeated_cut_off_tool_calls_end_the_turn() {
    let cut = || {
        let mut r = tool_reply(&[("t1", "echo", json!({"text":"partial"}))]);
        r.stop_reason = StopReason::MaxTokens;
        Ok(r)
    };
    // A complete response in between resets the count.
    let mut h = harness(
        vec![
            cut(),
            cut(),
            Ok(tool_reply(&[("t2", "echo", json!({"text":"ok"}))])),
            cut(),
            cut(),
            cut(),
            Ok(text_reply("never")),
        ],
        vec![],
        config(),
    );
    let r = h.session.run_turn("go").await;
    match &r.reason {
        TerminationReason::Error { message } => {
            assert!(message.contains("3 times in a row"), "{message}")
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(r.steps, 6);
    let last = h.session.history().last().unwrap();
    assert!(tool_results(&last.message)[0].1.contains("output limit"));
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
    assert_each_request_extends_the_last(&h.script.requests.lock().unwrap());
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
async fn hook_notes_reach_the_bus_and_after_model_notes_go_with_the_next_call_only() {
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
    let count = |i: usize| {
        format!("{:?}", reqs[i].messages)
            .matches("after-model note")
            .count()
    };
    // The note from the first response goes with the next call and stays in history;
    // the one from the turn's last response is dropped, not carried into the next turn.
    assert_eq!(count(1), 1);
    assert_eq!(count(2), 1);
    assert_each_request_extends_the_last(&reqs);
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

/// A tool whose spec is given; never called.
struct SpecTool(ToolSpec);

#[async_trait::async_trait]
impl Tool for SpecTool {
    fn spec(&self) -> ToolSpec {
        self.0.clone()
    }
    async fn call(&self, _input: Value, _cx: &Cx) -> ToolOutput {
        ToolOutput::error("not called")
    }
}

#[test]
fn tools_model_apis_would_reject_fail_registration() {
    let spec = |name: &str, schema: Value| ToolSpec {
        name: name.into(),
        description: "x".into(),
        input_schema: schema,
        read_only: true,
    };
    let object = json!({"type": "object", "properties": {}});
    for (bad, expected) in [
        (spec("has space", object.clone()), "must be 1 to 64"),
        (spec(&"x".repeat(65), object.clone()), "must be 1 to 64"),
        (spec("t", json!({"type": "string"})), "\"type\": \"object\""),
        (spec("t", json!("object")), "must be a JSON object"),
        (
            spec("t", json!({"type": "object", "properties": []})),
            "\"properties\"",
        ),
        (
            spec("t", json!({"type": "object", "required": "a"})),
            "\"required\"",
        ),
    ] {
        let p = vec![plugin("ext", move |cx| {
            cx.add_tool(SpecTool(bad.clone()));
            Ok(())
        })];
        let err = build(p, &Config::default()).err().unwrap().to_string();
        assert!(
            err.contains("plugin `ext`") && err.contains(expected),
            "{err}"
        );
    }
    // Names that aren't snake_case are accepted.
    for name in ["github__get-issue", "Tool_1"] {
        let ok = spec(name, object.clone());
        let p = vec![plugin("ext", move |cx| {
            cx.add_tool(SpecTool(ok.clone()));
            Ok(())
        })];
        assert!(build(p, &Config::default()).is_ok(), "{name}");
    }
    // The prefixes of MCP and external plugin tools are theirs alone: a built-in tool
    // can't pose as one, and one can't pose as a built-in.
    for name in ["mcp__github__get_issue", "ext__x__y"] {
        let reserved = spec(name, object.clone());
        let p = vec![plugin("ext", move |cx| {
            cx.add_tool(SpecTool(reserved.clone()));
            Ok(())
        })];
        let err = build(p, &Config::default()).err().unwrap().to_string();
        assert!(err.contains("reserved"), "{err}");
    }
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

/// A provider that is only picked, never called.
#[derive(Clone)]
struct Named(&'static str, Option<&'static str>);

#[async_trait::async_trait]
impl ModelProvider for Named {
    fn id(&self) -> &str {
        self.0
    }
    async fn stream(
        &self,
        _request: ModelRequest,
        _cancel: CancellationToken,
    ) -> Result<ModelStream, ModelError> {
        Err(ModelError::Other(anyhow::anyhow!("not called")))
    }
    fn default_model(&self) -> Option<&str> {
        self.1
    }
}

#[test]
fn provider_and_model_come_from_the_registered_providers() {
    let open = |providers: &[Named], toml: &str| {
        let providers = providers.to_vec();
        let p = vec![plugin("providers", move |cx| {
            for n in &providers {
                cx.add_provider(n.clone());
            }
            Ok(())
        })];
        let cfg = Config::parse(toml).unwrap();
        let registry = Arc::new(build(p, &cfg).unwrap());
        Session::builder(registry, Arc::new(cfg)).build()
    };
    let with_default = Named("first", Some("first-model"));
    let without = Named("second", None);

    // Nothing configured: the first registered provider and its default model.
    let s = open(&[with_default.clone(), without.clone()], "").unwrap();
    assert_eq!((s.provider_id(), s.model()), ("first", "first-model"));
    // A build with one provider that has no default model needs model.id.
    let err = open(std::slice::from_ref(&without), "").err().unwrap();
    assert!(
        format!("{err:#}").contains("`second` has no default model"),
        "{err:#}"
    );
    let s = open(std::slice::from_ref(&without), "[model]\nid = \"m\"").unwrap();
    assert_eq!((s.provider_id(), s.model()), ("second", "m"));
    let s = open(
        &[with_default, without],
        "[model]\nprovider = \"second\"\nid = \"m\"",
    )
    .unwrap();
    assert_eq!(s.provider_id(), "second");
    let err = open(&[], "").err().unwrap();
    assert!(format!("{err:#}").contains("no model provider plugin is enabled"));
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
