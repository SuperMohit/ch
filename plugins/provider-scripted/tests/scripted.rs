use std::sync::Arc;

use ch_kernel::reexports::CancellationToken;
use ch_kernel::{
    Config, ContentBlock, Message, ModelError, ModelEvent, ModelProvider, ModelRequest, Plugin,
    RegistryBuilder, StopReason, complete,
};
use ch_provider_scripted::{
    EXHAUSTED, ScriptError, ScriptStep, ScriptedPlugin, ScriptedProvider, parse_script,
};
use futures::StreamExt;
use serde_json::json;

fn request(text: &str) -> ModelRequest {
    ModelRequest {
        model: "scripted".into(),
        system: vec!["system".into()],
        messages: vec![Message::user(text)],
        tools: Vec::new(),
        max_tokens: 100,
        temperature: None,
        thinking_budget: None,
        purpose: "agent".into(),
        volatile_tail: 0,
    }
}

const SCRIPT: &str = r#"[
  {"thinking": "plan", "text": "Reading the file.",
   "tool_calls": [{"name": "read_file", "input": {"path": "a.rs"}}, {"name": "list_files"}],
   "usage": {"input_tokens": 500, "output_tokens": 40}},
  {"error": "rate_limited"},
  {"text": "All done."}
]"#;

#[tokio::test]
async fn replays_script_with_events_ids_and_usage() {
    let p = ScriptedProvider::new(parse_script(SCRIPT).unwrap());
    let mut stream = p
        .stream(request("go"), CancellationToken::new())
        .await
        .unwrap();
    let mut events = Vec::new();
    while let Some(ev) = stream.next().await {
        events.push(ev.unwrap());
    }
    assert!(matches!(events[0], ModelEvent::ThinkingDelta(_)));
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            ModelEvent::TextDelta(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "Reading the file.");
    assert!(
        events
            .iter()
            .filter(|e| matches!(e, ModelEvent::TextDelta(_)))
            .count()
            > 1
    );
    assert!(events.contains(&ModelEvent::ToolUseStart {
        id: "call_0_1".into(),
        name: "list_files".into()
    }));
    let Some(ModelEvent::Completed(resp)) = events.last() else {
        panic!("{events:?}")
    };
    assert_eq!(resp.stop_reason, StopReason::ToolUse);
    assert_eq!(resp.usage.input_tokens, 500);
    assert_eq!(resp.usage.output_tokens, 40);
    let calls = resp.message.tool_calls();
    assert_eq!(calls[0].id, "call_0_0");
    assert_eq!(calls[0].input, json!({"path": "a.rs"}));
    assert_eq!(calls[1].input, json!({}));
    assert!(matches!(
        resp.message.content[0],
        ContentBlock::Thinking { .. }
    ));

    let err = complete(&p, request("again"), CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(err, ModelError::RateLimited(_)));

    let resp = complete(&p, request("third"), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(resp.message.text(), "All done.");
    assert_eq!(resp.stop_reason, StopReason::EndTurn);
    assert!(resp.usage.input_tokens > 0, "estimated when not scripted");

    let resp = complete(&p, request("more"), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(resp.message.text(), EXHAUSTED);

    let seen: Vec<String> = p.requests().iter().map(|r| r.messages[0].text()).collect();
    assert_eq!(seen, ["go", "again", "third", "more"]);
    assert_eq!(p.steps_remaining(), 0);
}

#[tokio::test]
async fn error_steps() {
    let p = ScriptedProvider::new(vec![
        ScriptStep::error(ScriptError::ContextOverflow),
        ScriptStep::error(ScriptError::Unavailable),
    ]);
    let c = CancellationToken::new();
    assert!(matches!(
        p.stream(request("a"), c.clone()).await.err(),
        Some(ModelError::ContextOverflow(_))
    ));
    assert!(matches!(
        p.stream(request("b"), c.clone()).await.err(),
        Some(ModelError::Unavailable(_))
    ));
    c.cancel();
    assert!(matches!(
        p.stream(request("c"), c).await.err(),
        Some(ModelError::Cancelled)
    ));
}

#[tokio::test]
async fn overflow_by_estimated_size_does_not_consume_steps() {
    let p = ScriptedProvider::new(vec![ScriptStep::text("ok")]).with_context_window(50);
    assert_eq!(p.context_window("x"), Some(50));
    let big = "x".repeat(1000);
    let err = complete(&p, request(&big), CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(err, ModelError::ContextOverflow(_)), "{err:?}");
    assert_eq!(p.steps_used(), 0);
    let resp = complete(&p, request("small"), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(resp.message.text(), "ok");
    assert_eq!(p.requests().len(), 2);
}

#[test]
fn bad_scripts_are_rejected() {
    assert!(parse_script("{}").is_err());
    assert!(parse_script(r#"[{"error": "meteor"}]"#).is_err());
    assert!(parse_script(r#"[{"txt": "typo"}]"#).is_err());
    assert_eq!(parse_script("[{}]").unwrap(), vec![ScriptStep::default()]);
}

#[tokio::test]
async fn plugin_loads_script_relative_to_workspace() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("fixtures")).unwrap();
    std::fs::write(dir.path().join("fixtures/script.json"), SCRIPT).unwrap();
    let cfg = Config::parse(
        r#"
        [plugins]
        enable = ["provider-scripted"]
        [plugins.provider-scripted]
        script = "fixtures/script.json"
        context_window = 1000
        "#,
    )
    .unwrap();
    let plugins: Vec<Arc<dyn Plugin>> = vec![ch_provider_scripted::plugin()];
    let registry = RegistryBuilder::build(plugins, &cfg, dir.path()).unwrap();
    let p = registry.provider("scripted").unwrap();
    assert_eq!(p.context_window("any"), Some(1000));
    let resp = complete(p.as_ref(), request("go"), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(resp.message.tool_calls().len(), 2);

    // Off by default.
    let registry = RegistryBuilder::build(
        vec![ch_provider_scripted::plugin()],
        &Config::default(),
        dir.path(),
    )
    .unwrap();
    assert!(registry.provider("scripted").is_err());

    // A missing script file is a registration error that names the setting.
    let cfg = Config::parse(
        r#"
        [plugins]
        enable = ["provider-scripted"]
        [plugins.provider-scripted]
        script = "nope.json"
        "#,
    )
    .unwrap();
    let err = RegistryBuilder::build(vec![ch_provider_scripted::plugin()], &cfg, dir.path())
        .err()
        .unwrap();
    assert!(format!("{err:#}").contains("nope.json"), "{err:#}");
}

#[tokio::test]
async fn in_memory_plugin_exposes_requests() {
    let plugin =
        ScriptedPlugin::from_steps(vec![ScriptStep::tool("bash", json!({"command": "ls"}))]);
    let handle = plugin.provider().unwrap();
    let cfg = Config::parse("[plugins]\nenable = [\"provider-scripted\"]").unwrap();
    let registry =
        RegistryBuilder::build(vec![Arc::new(plugin)], &cfg, std::path::Path::new("/tmp")).unwrap();
    let p = registry.provider("scripted").unwrap();
    let resp = complete(p.as_ref(), request("list"), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(resp.message.tool_calls()[0].id, "call_0_0");
    assert_eq!(handle.requests().len(), 1);
    assert_eq!(handle.requests()[0].messages[0].text(), "list");
}
