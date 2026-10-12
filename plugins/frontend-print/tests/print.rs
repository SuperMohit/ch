//! End-to-end tests of the print frontend with a scripted provider and fake tools.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use ch_frontend_print::{OutputFormat, PrintOptions, PrintResult, exit_code, run_with};
use ch_kernel::reexports::CancellationToken;
use ch_kernel::*;
use futures::stream;
use serde_json::{Value, json};

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
        // Stream text in two pieces to exercise delta handling.
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
        input_tokens: 100,
        output_tokens: 10,
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

fn call(id: &str, name: &str, input: Value) -> ModelResponse {
    ModelResponse {
        message: Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: id.into(),
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
            read_only: true,
        }
    }
    async fn call(&self, input: Value, _cx: &Cx) -> ToolOutput {
        ToolOutput::ok(input["text"].as_str().unwrap_or("").to_string())
    }
    fn summarize(&self, input: &Value) -> String {
        input["text"].as_str().unwrap_or("").to_string()
    }
}

/// Asks for approval on every `echo` call.
struct AskHook;

#[async_trait::async_trait]
impl Hook for AskHook {
    fn id(&self) -> &str {
        "ask"
    }
    fn points(&self) -> &[HookPoint] {
        &[HookPoint::PreTool]
    }
    async fn on(&self, _event: &mut HookEvent, _cx: &Cx) -> HookDecision {
        HookDecision::ask("testing", Some("echo".into()))
    }
}

struct TestPlugin {
    script: Mutex<Option<Vec<ModelResponse>>>,
    ask: bool,
    options: Option<PrintOptions>,
}

impl Plugin for TestPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest::new("test", "test plugin")
    }
    fn register(&self, cx: &mut RegisterCx<'_>) -> anyhow::Result<()> {
        let script = self.script.lock().unwrap().take().unwrap_or_default();
        cx.add_provider(Scripted(Mutex::new(script.into())));
        cx.add_tool(Echo);
        if self.ask {
            cx.add_hook(AskHook);
        }
        if let Some(o) = &self.options {
            cx.provide_service(Arc::new(o.clone()))?;
        }
        Ok(())
    }
}

fn session(script: Vec<ModelResponse>, ask: bool, approver: &str, extra: &str) -> Session {
    let cfg = Config::parse(&format!(
        "[model]\nprovider = \"scripted\"\nid = \"m\"\n[model.pricing]\ninput = 1.0\noutput = \
         2.0\n{extra}"
    ))
    .unwrap();
    let plugins: Vec<Arc<dyn Plugin>> = vec![
        Arc::new(TestPlugin {
            script: Mutex::new(Some(script)),
            ask,
            options: Some(PrintOptions {
                prompt: "from service".into(),
                ..Default::default()
            }),
        }),
        ch_frontend_print::plugin(),
    ];
    let registry =
        Arc::new(RegistryBuilder::build(plugins, &cfg, std::path::Path::new("/tmp")).unwrap());
    Session::builder(registry, Arc::new(cfg))
        .approver(approver)
        .build()
        .unwrap()
}

async fn run(
    session: Session,
    format: OutputFormat,
    verbose: bool,
) -> (PrintResult, String, String) {
    let opts = PrintOptions {
        prompt: "do it".into(),
        output_format: format,
        verbose,
    };
    let mut out = Vec::new();
    let mut err = Vec::new();
    let r = run_with(session, &opts, &mut out, &mut err).await.unwrap();
    (
        r,
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

#[tokio::test]
async fn text_mode_streams_text_and_ends_with_newline() {
    let s = session(
        vec![
            text("Looking."),
            call("1", "echo", json!({"text": "hi"})),
            text("All done"),
        ],
        false,
        "auto-deny",
        "",
    );
    // The first response has no tool calls, so the turn ends after it.
    let (r, out, err) = run(s, OutputFormat::Text, false).await;
    assert_eq!(r.reason, "done");
    assert_eq!(out, "Looking.\n");
    assert!(err.is_empty(), "{err}");
}

#[tokio::test]
async fn text_mode_separates_messages_and_verbose_prints_tools() {
    let mut resp = call("1", "echo", json!({"text": "hi"}));
    resp.message
        .content
        .insert(0, ContentBlock::text("Let me check."));
    let s = session(vec![resp, text("Done.")], false, "auto-deny", "");
    let (r, out, err) = run(s, OutputFormat::Text, true).await;
    assert_eq!(r.reason, "done");
    assert_eq!(r.steps, 2);
    assert_eq!(out, "Let me check.\n\nDone.\n");
    assert!(err.contains("● echo hi"), "{err}");
    assert!(err.contains("  ⎿ 1 line"), "{err}");
}

#[tokio::test]
async fn json_mode_prints_one_result_object() {
    let s = session(vec![text("Hello there")], false, "auto-deny", "");
    let (r, out, _) = run(s, OutputFormat::Json, false).await;
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 1, "{out}");
    let v: Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(v["type"], "result");
    assert_eq!(v["reason"], "done");
    assert_eq!(v["final_text"], "Hello there");
    assert_eq!(v["steps"], 1);
    assert_eq!(v["usage"]["input_tokens"], 100);
    assert_eq!(v["session_id"], r.session_id.as_str());
    assert!(v["cost"].as_f64().unwrap() > 0.0);
    assert!(v.get("duration_ms").is_some());
}

#[tokio::test]
async fn stream_json_prints_events_then_result() {
    let s = session(
        vec![call("1", "echo", json!({"text": "x"})), text("ok")],
        false,
        "auto-deny",
        "",
    );
    let (_, out, _) = run(s, OutputFormat::StreamJson, false).await;
    let values: Vec<Value> = out
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let types: Vec<&str> = values.iter().map(|v| v["type"].as_str().unwrap()).collect();
    assert_eq!(types.first(), Some(&"session_start"));
    assert!(types.contains(&"tool_call_start"));
    assert!(types.contains(&"turn_end"));
    assert!(types.contains(&"session_end"));
    assert_eq!(types.last(), Some(&"result"));
}

#[tokio::test]
async fn auto_deny_denies_and_auto_allow_allows() {
    let script = || vec![call("1", "echo", json!({"text": "secret"})), text("ok")];
    let s = session(script(), true, "auto-deny", "");
    let (_, out, _) = run(s, OutputFormat::StreamJson, false).await;
    let end = out
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .find(|v| v["type"] == "tool_call_end")
        .unwrap();
    assert_eq!(end["is_error"], true);
    assert!(end["content"].as_str().unwrap().contains("--allow 'echo'"));

    let s = session(script(), true, "auto-allow", "");
    let (_, out, _) = run(s, OutputFormat::StreamJson, false).await;
    let end = out
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .find(|v| v["type"] == "tool_call_end")
        .unwrap();
    assert_eq!(end["is_error"], false);
    assert_eq!(end["content"], "secret");
}

#[tokio::test]
async fn step_limit_maps_to_exit_code_2() {
    let s = session(
        vec![
            call("1", "echo", json!({"text": "a"})),
            call("2", "echo", json!({"text": "b"})),
        ],
        false,
        "auto-deny",
        "[agent]\nmax_steps = 1\n",
    );
    let (r, _, err) = run(s, OutputFormat::Text, false).await;
    assert_eq!(r.reason, "step_limit");
    assert!(err.contains("ch: stopped (step_limit)"), "{err}");
    assert_eq!(exit_code(&TerminationReason::StepLimit), 2);
}

#[tokio::test]
async fn options_can_come_from_a_service() {
    let s = session(vec![text("x")], false, "auto-deny", "");
    let opts = ch_frontend_print::resolve_options(&s).unwrap();
    assert_eq!(opts.prompt, "from service");
}

#[tokio::test]
async fn empty_prompt_is_an_error() {
    let s = session(vec![], false, "auto-deny", "");
    let opts = PrintOptions::default();
    let mut out = Vec::new();
    let mut err = Vec::new();
    let e = run_with(s, &opts, &mut out, &mut err).await.unwrap_err();
    assert!(e.to_string().contains("no prompt"));
}
