//! Characterization tests of the turn loop: a scripted model, a captured terminal and
//! keys sent once the screen shows what they answer.

use std::collections::VecDeque;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ch_kernel::reexports::CancellationToken;
use ch_kernel::*;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use futures::stream;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::RuleSaved;
use crate::approval::{ApprovalBridge, ReplApprover};
use crate::format::strip_ansi;
use crate::repl::{run_command, run_turn};
use crate::terminal::{KeyPoller, Term};

struct Scripted(Mutex<VecDeque<ModelResponse>>);

#[async_trait::async_trait]
impl ModelProvider for Scripted {
    fn id(&self) -> &str {
        "scripted"
    }
    async fn stream(
        &self,
        _request: ModelRequest,
        _cancel: CancellationToken,
    ) -> Result<ModelStream, ModelError> {
        let resp = self
            .0
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| text("(script exhausted)"));
        let mut evs = Vec::new();
        let t = resp.message.text();
        if !t.is_empty() {
            let mid = t.len() / 2;
            evs.push(Ok(ModelEvent::TextDelta(t[..mid].to_string())));
            evs.push(Ok(ModelEvent::TextDelta(t[mid..].to_string())));
        }
        evs.push(Ok(ModelEvent::Completed(resp)));
        Ok(Box::pin(stream::iter(evs)))
    }
}

fn usage() -> Usage {
    Usage {
        input_tokens: 1200,
        output_tokens: 34,
        ..Default::default()
    }
}

fn text(t: &str) -> ModelResponse {
    ModelResponse {
        message: Message::assistant(t),
        stop_reason: StopReason::EndTurn,
        usage: usage(),
    }
}

fn call(name: &str, input: Value) -> ModelResponse {
    ModelResponse {
        message: Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "t1".into(),
                name: name.into(),
                input,
            }],
        },
        stop_reason: StopReason::ToolUse,
        usage: usage(),
    }
}

struct Echo;

#[async_trait::async_trait]
impl Tool for Echo {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "echo".into(),
            description: "echo".into(),
            input_schema: json!({"type": "object"}),
            read_only: false,
        }
    }
    async fn call(&self, input: Value, _cx: &Cx) -> ToolOutput {
        ToolOutput::ok(input["text"].as_str().unwrap_or("").to_string())
    }
    fn summarize(&self, input: &Value) -> String {
        input["text"].as_str().unwrap_or("").to_string()
    }
}

/// Runs until the turn is cancelled.
struct Wait;

#[async_trait::async_trait]
impl Tool for Wait {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "wait".into(),
            description: "wait".into(),
            input_schema: json!({"type": "object"}),
            read_only: true,
        }
    }
    async fn call(&self, _input: Value, cx: &Cx) -> ToolOutput {
        cx.cancel.cancelled().await;
        ToolOutput::error("interrupted")
    }
}

/// `/slow`: runs until the turn-or-command cancel is set.
struct UntilCancelled;

#[async_trait::async_trait]
impl Command for UntilCancelled {
    fn name(&self) -> &str {
        "slow"
    }
    fn description(&self) -> &str {
        "runs until cancelled"
    }
    async fn run(&self, _args: &str, cx: &mut CmdCx<'_>) -> anyhow::Result<Outcome> {
        let cancel = cx.session.cancel_handle();
        while !cancel.is_cancelled() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        Ok(Outcome::Message("stopped".into()))
    }
}

/// Asks for approval of every `echo` call.
struct AskEcho;

#[async_trait::async_trait]
impl Hook for AskEcho {
    fn id(&self) -> &str {
        "ask"
    }
    fn points(&self) -> &[HookPoint] {
        &[HookPoint::PreTool]
    }
    async fn on(&self, event: &HookEvent, _cx: &Cx) -> HookDecision {
        match event.tool_call() {
            Some(c) if c.name == "echo" => {
                HookDecision::ask("echo needs approval", Some("echo:*".into()))
            }
            _ => HookDecision::Continue,
        }
    }
}

struct TestPlugin {
    script: Mutex<Vec<ModelResponse>>,
    bridge: Arc<ApprovalBridge>,
    saved: Arc<Mutex<Vec<String>>>,
}

impl Plugin for TestPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest::new("test", "test plugin")
    }
    fn register(&self, cx: &mut RegisterCx<'_>) -> anyhow::Result<()> {
        let script = std::mem::take(&mut *self.script.lock().unwrap());
        cx.add_provider(Scripted(Mutex::new(script.into())));
        cx.add_tool(Echo);
        cx.add_tool(Wait);
        cx.add_hook(AskEcho);
        cx.add_command(UntilCancelled);
        cx.add_approver(ReplApprover {
            bridge: self.bridge.clone(),
        });
        let saved = self.saved.clone();
        cx.provide_service(Arc::new(RuleSaved(Box::new(move |rule| {
            saved.lock().unwrap().push(rule.to_string())
        }))))?;
        Ok(())
    }
}

/// The bytes written to the terminal.
#[derive(Clone, Default)]
struct Screen(Arc<Mutex<Vec<u8>>>);

impl Write for Screen {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Screen {
    /// The text without styling, with raw-mode `\r\n` back to `\n`.
    fn text(&self) -> String {
        let raw = String::from_utf8_lossy(&self.0.lock().unwrap()).to_string();
        strip_ansi(&raw).replace("\r\n", "\n")
    }
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ctrl_c() -> KeyEvent {
    KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
}

fn chars(s: &str) -> Vec<KeyEvent> {
    s.chars().map(|c| key(KeyCode::Char(c))).collect()
}

/// What to type once the screen shows `.0` (after the previous step's text). `None`
/// closes the key reader instead.
type Step = (&'static str, Option<Vec<KeyEvent>>);

/// Runs one turn of `script` with `steps` typed in, and returns a transcript: the
/// screen, then the tool results the model got, the rules saved and the project config.
fn new_session(
    script: Vec<ModelResponse>,
    workspace: &std::path::Path,
    saved: Arc<Mutex<Vec<String>>>,
) -> (Session, Arc<ApprovalBridge>) {
    let bridge = Arc::new(ApprovalBridge::new());
    let plugin = TestPlugin {
        script: Mutex::new(script),
        bridge: bridge.clone(),
        saved,
    };
    let cfg = Config::parse("[model]\nprovider = \"scripted\"\nid = \"m\"\n").unwrap();
    let plugins: Vec<Arc<dyn Plugin>> = vec![Arc::new(plugin)];
    let registry = Arc::new(RegistryBuilder::build(plugins, &cfg, workspace).unwrap());
    let session = Session::builder(registry, Arc::new(cfg))
        .workspace(workspace)
        .build()
        .unwrap();
    bridge.set_active(true);
    (session, bridge)
}

async fn transcript(script: Vec<ModelResponse>, steps: Vec<Step>) -> String {
    let dir = tempfile::tempdir().unwrap();
    let saved: Arc<Mutex<Vec<String>>> = Default::default();
    let (mut session, bridge) = new_session(script, dir.path(), saved.clone());
    let mut approvals = bridge.take_receiver().unwrap();

    let screen = Screen::default();
    let mut term = Term::capture(screen.clone(), || Some((80, 24)));
    let (tx, rx) = mpsc::unbounded_channel();
    let typist = {
        let screen = screen.clone();
        tokio::spawn(async move {
            let mut seen = 0;
            let mut tx = Some(tx);
            for (wait_for, keys) in steps {
                loop {
                    let text = screen.text();
                    if let Some(i) = text[seen..].find(wait_for) {
                        seen += i + wait_for.len();
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
                match keys {
                    Some(keys) => {
                        for k in keys {
                            let _ = tx.as_ref().unwrap().send(k);
                        }
                    }
                    None => tx = None,
                }
            }
            // Keep the reader open until the turn is over.
            std::future::pending::<()>().await;
            drop(tx);
        })
    };

    run_turn(
        &mut session,
        "go".into(),
        &mut approvals,
        &mut term,
        Some(KeyPoller::from_keys(rx)),
    )
    .await;
    typist.abort();

    let mut out = regex::Regex::new(r"\d+\.\ds")
        .unwrap()
        .replace_all(&screen.text(), "<time>")
        .to_string();
    out.push_str("--- tool results\n");
    for ev in session.history().events() {
        for block in &ev.message.content {
            if let ContentBlock::ToolResult {
                content, is_error, ..
            } = block
            {
                out.push_str(&format!("{content} (error: {is_error})\n"));
            }
        }
    }
    out.push_str(&format!("--- saved {:?}\n", saved.lock().unwrap()));
    if let Ok(config) = std::fs::read_to_string(dir.path().join(".ch/config.toml")) {
        out.push_str(&format!("--- .ch/config.toml\n{config}"));
    }
    out
}

fn echo_then_done() -> Vec<ModelResponse> {
    vec![call("echo", json!({"text": "hi"})), text("done")]
}

#[tokio::test]
async fn text_turn() {
    let t = transcript(vec![text("Hello there.\nSecond line.")], vec![]).await;
    insta::assert_snapshot!(t);
}

#[tokio::test]
async fn approve_with_y() {
    let t = transcript(echo_then_done(), vec![("y yes", Some(chars("y")))]).await;
    insta::assert_snapshot!(t);
}

#[tokio::test]
async fn deny_with_a_reason() {
    let mut keys = chars("too risky");
    keys.push(key(KeyCode::Backspace));
    keys.push(key(KeyCode::Char('Y')));
    keys.push(key(KeyCode::Enter));
    let t = transcript(
        echo_then_done(),
        vec![("y yes", Some(chars("n"))), ("why", Some(keys))],
    )
    .await;
    insta::assert_snapshot!(t);
}

#[tokio::test]
async fn deny_quietly_with_esc() {
    let t = transcript(
        echo_then_done(),
        vec![("y yes", Some(vec![key(KeyCode::Esc)]))],
    )
    .await;
    insta::assert_snapshot!(t);
}

#[tokio::test]
async fn always_allow_and_save_the_rule() {
    let t = transcript(
        echo_then_done(),
        vec![("y yes", Some(chars("a"))), ("[y/N]", Some(chars("y")))],
    )
    .await;
    insta::assert_snapshot!(t);
}

#[tokio::test]
async fn always_allow_without_saving() {
    let t = transcript(
        echo_then_done(),
        vec![("y yes", Some(chars("a"))), ("[y/N]", Some(chars("n")))],
    )
    .await;
    insta::assert_snapshot!(t);
}

#[tokio::test]
async fn ctrl_c_at_the_prompt_cancels_the_turn() {
    let t = transcript(echo_then_done(), vec![("y yes", Some(vec![ctrl_c()]))]).await;
    insta::assert_snapshot!(t);
}

#[tokio::test]
async fn key_reader_closing_at_the_prompt_denies() {
    let t = transcript(echo_then_done(), vec![("y yes", None)]).await;
    insta::assert_snapshot!(t);
}

#[tokio::test]
async fn esc_interrupts_a_running_tool() {
    let t = transcript(
        vec![call("wait", json!({})), text("never sent")],
        vec![("● wait", Some(vec![key(KeyCode::Esc), key(KeyCode::Esc)]))],
    )
    .await;
    insta::assert_snapshot!(t);
}

#[tokio::test]
async fn esc_cancels_a_slash_command_but_not_the_next_turn() {
    let dir = tempfile::tempdir().unwrap();
    let (mut session, bridge) =
        new_session(vec![text("still here")], dir.path(), Default::default());
    let (tx, rx) = mpsc::unbounded_channel();
    tx.send(key(KeyCode::Esc)).unwrap();
    let result = run_command(&mut session, "/slow", Some(KeyPoller::from_keys(rx))).await;
    assert_eq!(result.unwrap().unwrap(), Outcome::Message("stopped".into()));

    let mut approvals = bridge.take_receiver().unwrap();
    let screen = Screen::default();
    let mut term = Term::capture(screen.clone(), || Some((80, 24)));
    run_turn(&mut session, "go".into(), &mut approvals, &mut term, None).await;
    let text = screen.text();
    assert!(text.contains("still here"), "{text}");
    assert!(!text.contains("interrupted"), "{text}");
}
