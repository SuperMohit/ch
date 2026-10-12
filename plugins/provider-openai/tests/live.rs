//! Live test against a real server. Needs `OPENAI_API_KEY` (or a local server via
//! `CH_LIVE_OPENAI_BASE_URL`); run with
//! `cargo test -p ch-provider-openai --test live -- --ignored`.

use ch_kernel::reexports::CancellationToken;
use ch_kernel::{
    ContentBlock, Message, ModelEvent, ModelProvider, ModelRequest, Role, StopReason, ToolSpec,
};
use ch_provider_openai::{OpenAiConfig, OpenAiProvider};
use futures::StreamExt;
use serde_json::json;

#[tokio::test]
#[ignore = "needs OPENAI_API_KEY and network"]
async fn streamed_tool_use_round_trip() {
    let mut cfg = OpenAiConfig::default();
    if let Ok(url) = std::env::var("CH_LIVE_OPENAI_BASE_URL") {
        cfg.base_url = url;
    }
    let provider = OpenAiProvider::new(cfg).unwrap();
    let model = std::env::var("CH_LIVE_OPENAI_MODEL").unwrap_or("gpt-4.1-mini".into());
    let mut request = ModelRequest {
        model,
        system: vec!["Use the get_weather tool to answer weather questions.".into()],
        messages: vec![Message::user("What's the weather in Paris?")],
        tools: vec![ToolSpec {
            name: "get_weather".into(),
            description: "Returns the current weather for a city.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {"city": {"type": "string"}},
                "required": ["city"]
            }),
            read_only: true,
        }],
        max_tokens: 1024,
        temperature: None,
        thinking_budget: None,
        purpose: "live-test".into(),
        volatile_tail: 0,
    };

    let mut stream = provider
        .stream(request.clone(), CancellationToken::new())
        .await
        .unwrap();
    let mut first = None;
    let mut saw_start = false;
    while let Some(ev) = stream.next().await {
        match ev.unwrap() {
            ModelEvent::ToolUseStart { .. } => saw_start = true,
            ModelEvent::Completed(r) => first = Some(r),
            _ => {}
        }
    }
    let first = first.expect("Completed event");
    assert!(saw_start);
    assert_eq!(first.stop_reason, StopReason::ToolUse);
    let call = first.message.tool_calls().remove(0);
    assert!(call.input["city"].as_str().unwrap().contains("Paris"));

    request.messages.push(first.message.clone());
    request.messages.push(Message {
        role: Role::User,
        content: vec![ContentBlock::ToolResult {
            tool_use_id: call.id,
            content: "18°C and sunny".into(),
            is_error: false,
        }],
    });
    let second = ch_kernel::complete(&provider, request, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(second.stop_reason, StopReason::EndTurn);
    assert!(second.message.text().contains("18"));
}
