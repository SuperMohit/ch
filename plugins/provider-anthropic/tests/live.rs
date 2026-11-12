//! Live test against the real API. Needs `ANTHROPIC_API_KEY`; run with
//! `cargo test -p ch-provider-anthropic --test live -- --ignored`.

use ch_kernel::reexports::CancellationToken;
use ch_kernel::{
    ContentBlock, Message, ModelEvent, ModelProvider, ModelRequest, Role, StopReason, ToolSpec,
};
use ch_provider_anthropic::{AnthropicConfig, AnthropicProvider};
use futures::StreamExt;
use serde_json::json;

fn weather_tool() -> ToolSpec {
    ToolSpec {
        name: "get_weather".into(),
        description: "Returns the current weather for a city.".into(),
        input_schema: json!({
            "type": "object",
            "properties": {"city": {"type": "string"}},
            "required": ["city"]
        }),
        read_only: true,
    }
}

#[tokio::test]
#[ignore = "needs ANTHROPIC_API_KEY and network"]
async fn streamed_tool_use_round_trip() {
    let model = std::env::var("CH_LIVE_ANTHROPIC_MODEL").unwrap_or("claude-haiku-5-5".into());
    let provider = AnthropicProvider::new(AnthropicConfig {
        effort: "low".into(),
        ..Default::default()
    })
    .unwrap();
    let mut request = ModelRequest {
        model,
        system: vec!["Use the get_weather tool to answer weather questions.".into()],
        messages: vec![Message::user("What's the weather in Paris?")],
        tools: vec![weather_tool()],
        max_tokens: 2048,
        temperature: None,
        thinking_budget: None,
        purpose: "live-test".into(),
    };

    let mut stream = provider
        .stream(request.clone(), CancellationToken::new())
        .await
        .unwrap();
    let mut saw_tool_start = false;
    let mut first = None;
    while let Some(ev) = stream.next().await {
        match ev.unwrap() {
            ModelEvent::ToolUseStart { name, .. } => {
                assert_eq!(name, "get_weather");
                saw_tool_start = true;
            }
            ModelEvent::Completed(resp) => first = Some(resp),
            _ => {}
        }
    }
    let first = first.expect("Completed event");
    assert!(saw_tool_start);
    assert_eq!(first.stop_reason, StopReason::ToolUse);
    let call = first.message.tool_calls().remove(0);
    assert!(call.input["city"].as_str().unwrap().contains("Paris"));
    assert!(first.usage.input_tokens + first.usage.cache_read_tokens > 0);

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

/// The context plugin drops the newest step's thinking when it trims, while that step's
/// `tool_use` is answered by the request's last message. The API accepts a trailing run
/// of thinking blocks removed; this checks it with enforcement on, as new accounts have
/// it by default.
#[tokio::test]
#[ignore = "needs ANTHROPIC_API_KEY and network"]
async fn a_tool_result_is_accepted_after_its_call_s_thinking_is_dropped() {
    let model = std::env::var("CH_LIVE_ANTHROPIC_MODEL").unwrap_or("claude-haiku-5-5".into());
    let provider = AnthropicProvider::new(AnthropicConfig {
        prefix_mismatch_behavior: "error".into(),
        ..Default::default()
    })
    .unwrap();
    let mut request = ModelRequest {
        model,
        system: vec!["Use the get_weather tool to answer weather questions.".into()],
        messages: vec![Message::user(
            "Think about which city I mean, then check the weather in the capital of France.",
        )],
        tools: vec![weather_tool()],
        max_tokens: 4096,
        temperature: None,
        thinking_budget: Some(2048),
        purpose: "live-test".into(),
    };
    let first = ch_kernel::complete(&provider, request.clone(), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(first.stop_reason, StopReason::ToolUse);
    let thinking = |b: &ContentBlock| {
        matches!(
            b,
            ContentBlock::Thinking { .. } | ContentBlock::RedactedThinking { .. }
        )
    };
    assert!(
        first.message.content.iter().any(thinking),
        "the model must think for this test to mean anything: {:?}",
        first.message
    );
    let call = first.message.tool_calls().remove(0);

    let mut stripped = first.message.clone();
    stripped.content.retain(|b| !thinking(b));
    request.messages.push(stripped);
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
}
