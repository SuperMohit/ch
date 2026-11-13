//! Tools from sources that start with the session and change while it runs (ADR 0006).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ch_kernel::reexports::CancellationToken;
use ch_kernel::*;
use ch_testkit::{ScriptedSource, echo_tool, echo_tool_described};
use futures::stream;
use serde_json::{Value, json};
use tokio::sync::broadcast::Receiver;

// ---------- fakes ----------

/// Records requests and answers from a script; reports `mode` for tool changes.
#[derive(Clone)]
struct Recorder {
    mode: Arc<Mutex<ToolChanges>>,
    replies: Arc<Mutex<VecDeque<ModelResponse>>>,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
}

impl Recorder {
    fn new(mode: ToolChanges, replies: Vec<ModelResponse>) -> Self {
        Self {
            mode: Arc::new(Mutex::new(mode)),
            replies: Arc::new(Mutex::new(replies.into())),
            requests: Arc::default(),
        }
    }

    fn requests(&self) -> Vec<ModelRequest> {
        self.requests.lock().unwrap().clone()
    }

    fn last(&self) -> ModelRequest {
        self.requests().pop().expect("a request")
    }

    fn push(&self, replies: Vec<ModelResponse>) {
        self.replies.lock().unwrap().extend(replies);
    }
}

#[async_trait::async_trait]
impl ModelProvider for Recorder {
    fn id(&self) -> &str {
        "recorder"
    }

    async fn stream(
        &self,
        request: ModelRequest,
        _cancel: CancellationToken,
    ) -> Result<ModelStream, ModelError> {
        self.requests.lock().unwrap().push(request);
        let reply = (self.replies.lock().unwrap().pop_front()).unwrap_or_else(|| text("ok"));
        Ok(Box::pin(stream::iter(vec![Ok(ModelEvent::Completed(
            reply,
        ))])))
    }

    fn context_window(&self, _model: &str) -> Option<u64> {
        Some(100_000)
    }

    fn tool_changes(&self, _model: &str) -> ToolChanges {
        *self.mode.lock().unwrap()
    }
}

fn text(t: &str) -> ModelResponse {
    ModelResponse {
        message: Message::assistant(t),
        stop_reason: StopReason::EndTurn,
        usage: Usage::default(),
    }
}

fn call(id: &str, name: &str) -> ModelResponse {
    ModelResponse {
        message: Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: id.into(),
                name: name.into(),
                input: json!({"x": "1"}),
            }],
        },
        stop_reason: StopReason::ToolUse,
        usage: Usage::default(),
    }
}

/// A built-in tool, so the first request has one that never changes.
struct Builtin;

#[async_trait::async_trait]
impl Tool for Builtin {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read".into(),
            description: "Reads.".into(),
            input_schema: json!({"type": "object"}),
            read_only: true,
        }
    }
    async fn call(&self, _input: Value, _cx: &Cx) -> ToolOutput {
        ToolOutput::ok("read")
    }
}

/// A system prompt that would differ on every call if it were rendered again.
struct Counting(Mutex<u32>);

#[async_trait::async_trait]
impl PromptContributor for Counting {
    fn id(&self) -> &str {
        "counting"
    }
    async fn system_blocks(&self, cx: &Cx) -> Vec<String> {
        let mut n = self.0.lock().unwrap();
        *n += 1;
        let tools: Vec<&str> = cx.tools.iter().map(|t| t.name.as_str()).collect();
        vec![format!("render {n}; tools: {}", tools.join(", "))]
    }
}

/// Stops every before_model call while `on` is set.
struct Gate(Arc<Mutex<bool>>);

#[async_trait::async_trait]
impl Hook for Gate {
    fn id(&self) -> &str {
        "gate"
    }
    fn points(&self) -> &[HookPoint] {
        &[HookPoint::BeforeModel]
    }
    async fn on(&self, _event: &HookEvent, _cx: &Cx) -> HookDecision {
        if *self.0.lock().unwrap() {
            HookDecision::stop("gate closed")
        } else {
            HookDecision::Continue
        }
    }
}

/// The read-only flag pre_tool hooks see for each call, and whether `cx.tools` lists
/// the tool.
struct SeesReadOnly(Arc<Mutex<Vec<(String, bool)>>>);

#[async_trait::async_trait]
impl Hook for SeesReadOnly {
    fn id(&self) -> &str {
        "sees"
    }
    fn points(&self) -> &[HookPoint] {
        &[HookPoint::PreTool]
    }
    async fn on(&self, event: &HookEvent, cx: &Cx) -> HookDecision {
        if let HookEvent::PreTool { call, read_only } = event {
            assert!(cx.has_tool(&call.name), "{} isn't in cx.tools", call.name);
            self.0.lock().unwrap().push((call.name.clone(), *read_only));
        }
        HookDecision::Continue
    }
}

struct Run {
    session: Session,
    model: Recorder,
    events: Receiver<AgentEvent>,
    gate: Arc<Mutex<bool>>,
    read_only: Arc<Mutex<Vec<(String, bool)>>>,
}

fn base(
    model: &Recorder,
    gate: &Arc<Mutex<bool>>,
    ro: &Arc<Mutex<Vec<(String, bool)>>>,
) -> Arc<dyn Plugin> {
    let (model, gate, ro) = (model.clone(), gate.clone(), ro.clone());
    struct Base(Recorder, Arc<Mutex<bool>>, Arc<Mutex<Vec<(String, bool)>>>);
    impl Plugin for Base {
        fn manifest(&self) -> PluginManifest {
            PluginManifest::new("base", "test")
        }
        fn register(&self, cx: &mut RegisterCx<'_>) -> anyhow::Result<()> {
            cx.add_provider(self.0.clone());
            cx.add_tool(Builtin);
            cx.add_prompt(Counting(Mutex::new(0)));
            cx.add_hook(Gate(self.1.clone()));
            cx.add_hook(SeesReadOnly(self.2.clone()));
            Ok(())
        }
    }
    Arc::new(Base(model, gate, ro))
}

fn config() -> Config {
    Config::parse("[model]\nprovider = \"recorder\"\nid = \"m\"\n").unwrap()
}

fn session(model: Recorder, sources: Vec<ScriptedSource>, store: Option<SessionStore>) -> Run {
    let gate = Arc::new(Mutex::new(false));
    let read_only = Arc::default();
    let plugins = vec![
        base(&model, &gate, &read_only),
        ScriptedSource::plugin("sources", sources),
    ];
    let cfg = config();
    let registry = RegistryBuilder::build(plugins, &cfg, std::path::Path::new("/tmp")).unwrap();
    let mut builder = Session::builder(Arc::new(registry), Arc::new(cfg)).workspace("/tmp");
    if let Some(store) = store {
        builder = builder.store(store);
    }
    let session = builder.build().unwrap();
    let events = session.events().subscribe();
    Run {
        session,
        model,
        events,
        gate,
        read_only,
    }
}

impl Run {
    fn drain(&mut self) -> Vec<AgentEvent> {
        let mut out = Vec::new();
        while let Ok(e) = self.events.try_recv() {
            out.push(e);
        }
        out
    }

    fn warnings(&mut self) -> Vec<String> {
        (self.drain().into_iter())
            .filter_map(|e| match e {
                AgentEvent::Warning { message } => Some(message),
                _ => None,
            })
            .collect()
    }
}

fn mcp(names: &[&str]) -> Vec<Arc<dyn Tool>> {
    names
        .iter()
        .map(|n| echo_tool(&format!("mcp__fake__{n}")))
        .collect()
}

fn names(specs: &[ToolSpec]) -> Vec<&str> {
    specs.iter().map(|s| s.name.as_str()).collect()
}

fn ledger(req: &ModelRequest) -> Vec<(String, bool, bool)> {
    (req.tool_ledger.as_ref().expect("a native request").iter())
        .map(|e| (e.spec.name.clone(), e.deferred, e.active))
        .collect()
}

/// The `ToolsChanged` blocks of the request's last message.
fn changes(req: &ModelRequest) -> Vec<ContentBlock> {
    (req.messages.last().unwrap().content.iter())
        .filter(|b| matches!(b, ContentBlock::ToolsChanged { .. }))
        .cloned()
        .collect()
}

fn changed(added: &[&str], removed: &[&str]) -> ContentBlock {
    ContentBlock::ToolsChanged {
        added: added.iter().map(|s| s.to_string()).collect(),
        removed: removed.iter().map(|s| s.to_string()).collect(),
    }
}

fn result_text(req: &ModelRequest) -> String {
    (req.messages.last().unwrap().content.iter())
        .find_map(|b| match b {
            ContentBlock::ToolResult { content, .. } => Some(content.clone()),
            _ => None,
        })
        .expect("a tool result")
}

// ---------- tests ----------

#[tokio::test]
async fn a_source_starts_with_the_session_and_its_tools_are_in_the_first_request() {
    let src = ScriptedSource::new("mcp:fake", "mcp__fake__").with_tools(mcp(&["a"]));
    let model = Recorder::new(
        ToolChanges::Native,
        vec![call("c1", "mcp__fake__a"), text("done")],
    );
    let mut run = session(model, vec![src.clone()], None);
    let r = run.session.run_turn("go").await;
    assert_eq!(r.reason, TerminationReason::Done);

    let first = &run.model.requests()[0];
    assert_eq!(names(&first.tools), ["read", "mcp__fake__a"]);
    assert_eq!(
        ledger(first),
        [
            ("read".into(), false, true),
            ("mcp__fake__a".into(), false, true)
        ]
    );
    assert!(changes(first).is_empty());
    // The system prompt was rendered with the source's tools.
    assert_eq!(first.system, ["render 1; tools: read, mcp__fake__a"]);
    // The tool says it is read-only; the session doesn't take a source's word for it.
    assert_eq!(
        *run.read_only.lock().unwrap(),
        [("mcp__fake__a".to_string(), false)]
    );
    assert_eq!(
        result_text(&run.model.last()),
        "mcp__fake__a ran with {\"x\":\"1\"}"
    );

    run.session.end().await;
    assert_eq!((src.starts(), src.stops()), (1, 1));
    assert_eq!(
        run.session.tool_sources(),
        [("mcp:fake".to_string(), SourceStatus::Stopped)]
    );
}

#[tokio::test]
async fn a_tool_added_mid_session_is_deferred_and_announced() {
    let src = ScriptedSource::new("mcp:fake", "mcp__fake__").with_tools(mcp(&["a"]));
    let model = Recorder::new(ToolChanges::Native, vec![text("one")]);
    let mut run = session(model, vec![src.clone()], None);
    run.session.run_turn("first").await;
    let before = run.model.last();

    src.set_tools(mcp(&["a", "b"]));
    run.model
        .push(vec![call("c1", "mcp__fake__b"), text("two")]);
    run.drain();
    run.session.run_turn("second").await;

    let reqs = run.model.requests();
    let second = &reqs[1];
    // Earlier definitions are sent unchanged and in order; the new one is appended.
    assert_eq!(
        ledger(second),
        [
            ("read".into(), false, true),
            ("mcp__fake__a".into(), false, true),
            ("mcp__fake__b".into(), true, true)
        ]
    );
    assert_eq!(&ledger(second)[..2], &ledger(&before)[..]);
    assert_eq!(changes(second), [changed(&["mcp__fake__b"], &[])]);
    assert_eq!(
        second.system, before.system,
        "the system prompt isn't rendered again"
    );
    assert_eq!(
        names(&second.tools),
        ["read", "mcp__fake__a", "mcp__fake__b"]
    );
    // The block is stored, so the next call repeats it where it was.
    assert_eq!(
        &reqs[2].messages[..second.messages.len()],
        &second.messages[..]
    );
    assert!(changes(&reqs[2]).is_empty());
    assert_eq!(result_text(&reqs[2]), "mcp__fake__b ran with {\"x\":\"1\"}");
    assert!(run.drain().iter().any(|e| matches!(
        e,
        AgentEvent::ToolsChanged { added, removed } if added == &["mcp__fake__b"] && removed.is_empty()
    )));
}

#[tokio::test]
async fn a_withdrawn_tool_keeps_its_definition_and_calls_to_it_fail() {
    let src = ScriptedSource::new("mcp:fake", "mcp__fake__").with_tools(mcp(&["a"]));
    let model = Recorder::new(ToolChanges::Native, vec![text("one")]);
    let mut run = session(model, vec![src.clone()], None);
    run.session.run_turn("first").await;

    src.exit("the server exited (status 1)");
    run.model
        .push(vec![call("c1", "mcp__fake__a"), text("two")]);
    run.session.run_turn("second").await;

    let reqs = run.model.requests();
    assert_eq!(changes(&reqs[1]), [changed(&[], &["mcp__fake__a"])]);
    assert_eq!(
        ledger(&reqs[1]),
        [
            ("read".into(), false, true),
            ("mcp__fake__a".into(), false, false)
        ]
    );
    assert_eq!(names(&reqs[1].tools), ["read"]);
    assert_eq!(
        result_text(&reqs[2]),
        "`mcp__fake__a` is no longer available. Use one of the tools you have now."
    );
    assert_eq!(
        run.session.tool_sources(),
        [(
            "mcp:fake".to_string(),
            SourceStatus::Exited {
                reason: "the server exited (status 1)".into()
            }
        )]
    );

    // Back again with the same definition: the entry is reused.
    src.set_tools(mcp(&["a"]));
    run.session.run_turn("third").await;
    let third = run.model.last();
    assert_eq!(changes(&third), [changed(&["mcp__fake__a"], &[])]);
    assert_eq!(ledger(&third).len(), 2);
}

/// A tool whose call makes its source exit.
struct Quit(ScriptedSource);

#[async_trait::async_trait]
impl Tool for Quit {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "mcp__fake__quit".into(),
            description: "Stops the server.".into(),
            input_schema: json!({"type": "object"}),
            read_only: false,
        }
    }
    async fn call(&self, _input: Value, _cx: &Cx) -> ToolOutput {
        self.0.exit("connection reset");
        ToolOutput::ok("bye")
    }
}

#[tokio::test]
async fn a_call_to_a_tool_whose_source_just_exited_gets_the_reason() {
    let src = ScriptedSource::new("mcp:fake", "mcp__fake__");
    let src = src
        .clone()
        .with_tools(vec![Arc::new(Quit(src)), echo_tool("mcp__fake__a")]);
    // One response calls both: the source exits while the first call runs, before the
    // model could hear of it.
    let both = ModelResponse {
        message: Message {
            role: Role::Assistant,
            content: [("c1", "mcp__fake__quit"), ("c2", "mcp__fake__a")]
                .map(|(id, name)| ContentBlock::ToolUse {
                    id: id.into(),
                    name: name.into(),
                    input: json!({}),
                })
                .to_vec(),
        },
        stop_reason: StopReason::ToolUse,
        usage: Usage::default(),
    };
    let model = Recorder::new(ToolChanges::Native, vec![both, text("done")]);
    let mut run = session(model, vec![src], None);
    run.session.run_turn("go").await;
    let last = run.model.last();
    let results: Vec<String> = (last.messages.last().unwrap().content.iter())
        .filter_map(|b| match b {
            ContentBlock::ToolResult { content, .. } => Some(content.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        results,
        [
            "bye",
            "`mcp__fake__a` is no longer available: connection reset."
        ]
    );
    // The same request tells the model both tools are gone.
    assert_eq!(
        changes(&last),
        [changed(&[], &["mcp__fake__quit", "mcp__fake__a"])]
    );
}

#[tokio::test]
async fn a_change_waits_when_a_hook_stops_the_call() {
    let src = ScriptedSource::new("mcp:fake", "mcp__fake__").with_tools(mcp(&["a"]));
    let model = Recorder::new(ToolChanges::Native, vec![text("one")]);
    let mut run = session(model, vec![src.clone()], None);
    run.session.run_turn("first").await;

    src.set_tools(mcp(&["a", "b"]));
    *run.gate.lock().unwrap() = true;
    let r = run.session.run_turn("stopped").await;
    assert!(
        matches!(r.reason, TerminationReason::HookStop { .. }),
        "{r:?}"
    );
    assert_eq!(run.model.requests().len(), 1);
    assert_eq!(
        run.session.tool_ledger().entries().len(),
        2,
        "nothing committed"
    );

    *run.gate.lock().unwrap() = false;
    run.session.run_turn("again").await;
    let last = run.model.last();
    assert_eq!(changes(&last), [changed(&["mcp__fake__b"], &[])]);
    assert_eq!(run.session.tool_ledger().entries().len(), 3);
}

#[tokio::test]
async fn without_native_changes_the_current_tools_are_sent_with_a_note() {
    let src = ScriptedSource::new("mcp:fake", "mcp__fake__").with_tools(mcp(&["a"]));
    let model = Recorder::new(ToolChanges::Replace, vec![text("one")]);
    let mut run = session(model, vec![src.clone()], None);
    run.session.run_turn("first").await;
    src.set_tools(mcp(&["b"]));
    run.session.run_turn("second").await;
    let last = run.model.last();
    assert!(last.tool_ledger.is_none());
    assert_eq!(names(&last.tools), ["read", "mcp__fake__b"]);
    assert_eq!(
        changes(&last),
        [changed(&["mcp__fake__b"], &["mcp__fake__a"])]
    );
}

#[tokio::test]
async fn a_model_that_cant_take_changes_keeps_its_tools_until_clear() {
    let src = ScriptedSource::new("mcp:fake", "mcp__fake__").with_tools(mcp(&["a"]));
    let model = Recorder::new(ToolChanges::Frozen, vec![text("one")]);
    let mut run = session(model, vec![src.clone()], None);
    run.session.run_turn("first").await;
    let first = run.model.last();

    src.set_tools(mcp(&["b"]));
    run.drain();
    run.session.run_turn("second").await;
    let second = run.model.last();
    assert_eq!(second.tools, first.tools);
    assert!(changes(&second).is_empty());
    let warnings = run.warnings();
    assert_eq!(
        warnings,
        [
            "this model can't take new tools in a running conversation; these tools are \
          available after /clear: mcp__fake__b"
        ]
    );
    // Warned once.
    run.session.run_turn("third").await;
    assert!(run.warnings().is_empty());

    // The first list is kept, the system prompt too, and a withdrawn tool fails.
    assert_eq!(run.model.last().system, first.system);
    src.exit("gone");
    run.model.push(vec![call("c1", "mcp__fake__a"), text("ok")]);
    run.session.run_turn("call it").await;
    let last = run.model.last();
    assert_eq!(last.tools, first.tools);
    assert_eq!(
        result_text(&last),
        "`mcp__fake__a` is no longer available: gone."
    );
    src.set_tools(mcp(&["b"]));

    run.session.clear().unwrap();
    run.session.run_turn("fresh").await;
    let fresh = run.model.last();
    assert_eq!(names(&fresh.tools), ["read", "mcp__fake__b"]);
    assert_eq!(fresh.system, ["render 2; tools: read, mcp__fake__b"]);
}

#[tokio::test]
async fn sources_that_fail_or_start_slowly_dont_hold_up_the_session() {
    let broken =
        ScriptedSource::new("mcp:broken", "mcp__broken__").failing("exited with status 127");
    let slow = ScriptedSource::new("mcp:slow", "mcp__slow__")
        .with_tools(vec![echo_tool("mcp__slow__s")])
        .slow(Duration::from_millis(300), Duration::from_millis(20));
    let model = Recorder::new(ToolChanges::Native, vec![text("one")]);
    let mut run = session(model, vec![broken, slow], None);
    run.session.run_turn("first").await;
    assert_eq!(names(&run.model.last().tools), ["read"]);
    let events = run.drain();
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::ToolSource { source, status: SourceStatus::Failed { reason } }
            if source == "mcp:broken" && reason == "exited with status 127"
    )));
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::Warning { message } if message.contains("`mcp:slow` is still starting")
    )));

    tokio::time::sleep(Duration::from_millis(400)).await;
    run.session.run_turn("second").await;
    assert_eq!(
        changes(&run.model.last()),
        [changed(&["mcp__slow__s"], &[])]
    );
}

#[tokio::test]
async fn a_source_only_loses_the_tools_that_break_a_rule() {
    let mut bad_schema = echo_tool("mcp__fake__bad").spec();
    bad_schema.input_schema = json!({"type": "string"});
    struct Fixed(ToolSpec);
    #[async_trait::async_trait]
    impl Tool for Fixed {
        fn spec(&self) -> ToolSpec {
            self.0.clone()
        }
        async fn call(&self, _input: Value, _cx: &Cx) -> ToolOutput {
            ToolOutput::ok("")
        }
    }
    let long = "d".repeat(5000);
    let src = ScriptedSource::new("mcp:fake", "mcp__fake__").with_tools(vec![
        echo_tool("mcp__fake__ok"),
        echo_tool("read"),
        echo_tool("mcp__other__x"),
        Arc::new(Fixed(bad_schema)),
        echo_tool_described("mcp__fake__long", &long),
        echo_tool("mcp__fake__ok"),
    ]);
    // A second source offering a name the first one has.
    let twin = ScriptedSource::new("mcp:twin", "mcp__fake__").with_tools(mcp(&["ok"]));
    let model = Recorder::new(ToolChanges::Native, vec![text("one")]);
    let mut run = session(model, vec![src, twin], None);
    run.session.run_turn("first").await;

    let first = run.model.last();
    assert_eq!(
        names(&first.tools),
        ["read", "mcp__fake__ok", "mcp__fake__long"]
    );
    let long = &first.tools[2].description;
    assert!(long.len() <= 4096 && long.ends_with('…'), "{}", long.len());
    let warnings = run.warnings();
    for expected in [
        "`read` doesn't start with `mcp__fake__`",
        "`mcp__other__x` doesn't start with `mcp__fake__`",
        "\"type\": \"object\"",
        "`mcp__fake__ok` is listed twice",
        "`mcp:twin` also offers a tool named `mcp__fake__ok`",
    ] {
        assert!(
            warnings.iter().any(|w| w.contains(expected)),
            "{expected} in {warnings:#?}"
        );
    }
}

#[tokio::test]
async fn a_read_only_flag_comes_only_from_the_users_config() {
    let src = ScriptedSource::new("mcp:fake", "mcp__fake__")
        .with_tools(mcp(&["a", "b"]))
        .read_only(&["mcp__fake__b"]);
    let model = Recorder::new(
        ToolChanges::Native,
        vec![
            call("c1", "mcp__fake__a"),
            call("c2", "mcp__fake__b"),
            text("done"),
        ],
    );
    let mut run = session(model, vec![src], None);
    run.session.run_turn("go").await;
    assert_eq!(
        *run.read_only.lock().unwrap(),
        [
            ("mcp__fake__a".to_string(), false),
            ("mcp__fake__b".to_string(), true)
        ]
    );
}

#[tokio::test]
async fn a_resumed_session_keeps_its_prompt_and_hears_what_changed() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path());
    let src = ScriptedSource::new("mcp:fake", "mcp__fake__").with_tools(mcp(&["a", "b"]));
    let model = Recorder::new(ToolChanges::Native, vec![text("one")]);
    let mut run = session(model, vec![src], Some(store.clone()));
    run.session.run_turn("first").await;
    let first = run.model.last();
    let id = run.session.id().to_string();
    run.session.end().await;
    drop(run);

    // The next run has another server version: `a` is gone, `c` is new, and the prompt
    // contributor would render something else.
    let src = ScriptedSource::new("mcp:fake", "mcp__fake__").with_tools(mcp(&["b", "c"]));
    let model = Recorder::new(ToolChanges::Native, vec![text("two")]);
    let gate = Arc::new(Mutex::new(false));
    let ro = Arc::default();
    let plugins = vec![
        base(&model, &gate, &ro),
        ScriptedSource::plugin("sources", vec![src]),
    ];
    let cfg = config();
    let registry = RegistryBuilder::build(plugins, &cfg, std::path::Path::new("/tmp")).unwrap();
    let mut session = Session::builder(Arc::new(registry), Arc::new(cfg))
        .workspace("/tmp")
        .store(store.clone())
        .restore(store.load(&id).unwrap())
        .build()
        .unwrap();
    session.run_turn("second").await;
    let second = model.last();
    assert_eq!(second.system, first.system);
    let specs = |r: &ModelRequest| -> Vec<ToolSpec> {
        let l = r.tool_ledger.as_ref().unwrap();
        l.iter().map(|e| e.spec.clone()).collect()
    };
    assert_eq!(specs(&second)[..3], specs(&first)[..]);
    assert_eq!(
        ledger(&second),
        [
            ("read".into(), false, true),
            ("mcp__fake__a".into(), false, false),
            ("mcp__fake__b".into(), false, true),
            ("mcp__fake__c".into(), true, true)
        ]
    );
    assert_eq!(
        changes(&second),
        [changed(&["mcp__fake__c"], &["mcp__fake__a"])]
    );
}

#[tokio::test]
async fn a_source_added_mid_session_joins_at_the_next_call_and_can_be_removed() {
    let model = Recorder::new(ToolChanges::Native, vec![text("one")]);
    let mut run = session(model, vec![], None);
    run.session.run_turn("first").await;

    let src =
        ScriptedSource::new("mcp:late", "mcp__late__").with_tools(vec![echo_tool("mcp__late__x")]);
    run.session
        .add_tool_source(Arc::new(src.clone()))
        .await
        .unwrap();
    assert!(
        run.session
            .add_tool_source(Arc::new(src.clone()))
            .await
            .is_err(),
        "the same id twice"
    );
    run.session.run_turn("second").await;
    assert_eq!(
        changes(&run.model.last()),
        [changed(&["mcp__late__x"], &[])]
    );

    assert!(run.session.remove_tool_source("mcp:late").await);
    assert_eq!(src.stops(), 1);
    run.session.run_turn("third").await;
    assert_eq!(
        changes(&run.model.last()),
        [changed(&[], &["mcp__late__x"])]
    );
}

#[tokio::test]
async fn a_stale_instance_of_a_source_cant_change_its_replacement() {
    let src = ScriptedSource::new("mcp:fake", "mcp__fake__").with_tools(mcp(&["a"]));
    let other = ScriptedSource::new("mcp:other", "mcp__other__")
        .with_tools(vec![echo_tool("mcp__other__o")]);
    let model = Recorder::new(ToolChanges::Native, vec![text("one")]);
    let mut run = session(model, vec![src.clone(), other], None);
    run.session.run_turn("first").await;
    let old = src.sink();

    // A reconnect: the old instance is removed and a new one added under the same id.
    assert!(run.session.remove_tool_source("mcp:fake").await);
    run.session
        .add_tool_source(Arc::new(src.clone()))
        .await
        .unwrap();
    old.exited("the old process noticed its pipe closed");
    old.set_tools(mcp(&["stale"]));
    assert_eq!(
        run.session.tool_sources(),
        [
            ("mcp:fake".to_string(), SourceStatus::Ready { tools: 1 }),
            ("mcp:other".to_string(), SourceStatus::Ready { tools: 1 }),
        ],
        "the new instance keeps its place and its tools"
    );
    run.session.run_turn("second").await;
    assert!(changes(&run.model.last()).is_empty());

    // After the session ends, a late exit doesn't turn "stopped" into "exited".
    let current = src.sink();
    run.session.end().await;
    run.drain();
    current.exited("pipe closed");
    assert!(run.drain().is_empty());
    assert_eq!(
        run.session.tool_sources()[0],
        ("mcp:fake".to_string(), SourceStatus::Stopped)
    );
}

#[tokio::test]
async fn a_source_whose_start_panics_is_reported_as_failed() {
    let src = ScriptedSource::new("mcp:boom", "mcp__boom__").panicking();
    let model = Recorder::new(ToolChanges::Native, vec![text("one")]);
    let mut run = session(model, vec![src], None);
    run.session.run_turn("first").await;
    assert_eq!(
        run.session.tool_sources(),
        [(
            "mcp:boom".to_string(),
            SourceStatus::Failed {
                reason: "it panicked while starting".into()
            }
        )]
    );
}

#[tokio::test]
async fn resuming_an_old_file_announces_the_tools_of_running_sources() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path());
    // A conversation saved before prompts and ledgers were kept.
    let model = Recorder::new(ToolChanges::Native, vec![text("one")]);
    let mut old = session(model, vec![], Some(store.clone()));
    old.session.run_turn("first").await;
    let id = old.session.id().to_string();
    old.session.end().await;
    drop(old);
    let path = dir.path().join(format!("{id}.jsonl"));
    let saved = std::fs::read_to_string(&path).unwrap();
    let without: Vec<&str> = (saved.lines())
        .filter(|l| !l.starts_with("{\"type\":\"prompt\""))
        .collect();
    assert_eq!(without.len() + 1, saved.lines().count());
    std::fs::write(&path, without.join("\n") + "\n").unwrap();

    // `/resume` in a session whose sources are already running.
    let src = ScriptedSource::new("mcp:fake", "mcp__fake__").with_tools(mcp(&["a"]));
    let model = Recorder::new(ToolChanges::Native, vec![text("again")]);
    let mut run = session(model, vec![src], Some(store.clone()));
    run.session.start().await;
    run.session.restore(store.load(&id).unwrap()).unwrap();
    run.session.run_turn("go on").await;
    let req = run.model.last();
    assert_eq!(
        ledger(&req),
        [
            ("read".into(), false, true),
            ("mcp__fake__a".into(), true, true)
        ]
    );
    assert_eq!(changes(&req), [changed(&["mcp__fake__a"], &[])]);
    // The prompt is rendered with the tools the old requests had.
    assert_eq!(req.system, ["render 1; tools: read"]);
}

#[tokio::test]
async fn switching_to_a_model_that_cant_take_changes_keeps_every_first_definition() {
    let src = ScriptedSource::new("mcp:fake", "mcp__fake__").with_tools(mcp(&["a"]));
    let model = Recorder::new(ToolChanges::Native, vec![text("one")]);
    let mut run = session(model, vec![src.clone()], None);
    run.session.run_turn("first").await;
    src.set_tools(mcp(&["b"]));
    run.session.run_turn("second").await;

    *run.model.mode.lock().unwrap() = ToolChanges::Frozen;
    run.drain();
    run.session.run_turn("third").await;
    let last = run.model.last();
    assert!(last.tool_ledger.is_none());
    // The first request's tools, the withdrawn one included.
    assert_eq!(names(&last.tools), ["read", "mcp__fake__a"]);
    let warnings = run.warnings();
    assert!(
        warnings.iter().any(|w| w.contains("can't repeat here")),
        "{warnings:#?}"
    );
}
