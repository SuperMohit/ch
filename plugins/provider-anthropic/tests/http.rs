//! HTTP-level tests against a local mock server: headers, retries, error mapping,
//! cancellation.

use std::time::{Duration, Instant};

use ch_kernel::reexports::CancellationToken;
use ch_kernel::{
    ContentBlock, Message, ModelError, ModelEvent, ModelProvider, ModelRequest, StopReason,
    ToolSpec, complete,
};
use ch_provider_anthropic::{AnthropicConfig, AnthropicProvider};
use futures::StreamExt;
use serde_json::{Value, json};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

fn provider(server: &MockServer, max_retries: u32) -> AnthropicProvider {
    AnthropicProvider::new(AnthropicConfig {
        base_url: server.uri(),
        max_retries,
        retry_base_ms: 10,
        ..Default::default()
    })
    .unwrap()
    .with_api_key("test-key")
}

fn request() -> ModelRequest {
    ModelRequest {
        model: "claude-opus-5-5".into(),
        system: vec!["You are terse.".into()],
        messages: vec![Message::user("hi")],
        tools: vec![ToolSpec {
            name: "get_time".into(),
            description: "Returns the time".into(),
            input_schema: json!({"type": "object", "properties": {}}),
            read_only: true,
        }],
        max_tokens: 1024,
        temperature: None,
        thinking_budget: None,
        purpose: "agent".into(),
        volatile_tail: 0,
    }
}

fn sse(events: &[Value]) -> String {
    events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
        .collect()
}

fn ok_body() -> ResponseTemplate {
    let body = sse(&[
        json!({"type":"message_start","message":{"id":"m","type":"message","role":"assistant","content":[],"usage":{"input_tokens":10,"output_tokens":1}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"get_time","input":{}}}),
        json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"tz\":"}}),
        json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"UTC\"}"}}),
        json!({"type":"content_block_stop","index":1}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":7}}),
        json!({"type":"message_stop"}),
    ]);
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(body)
}

fn error_body(status: u16, kind: &str, message: &str) -> ResponseTemplate {
    ResponseTemplate::new(status)
        .set_body_json(json!({"type":"error","error":{"type":kind,"message":message}}))
}

#[tokio::test]
async fn streams_and_sends_the_right_headers() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "test-key"))
        .and(header("anthropic-version", "2023-06-01"))
        .respond_with(ok_body())
        .expect(1)
        .mount(&server)
        .await;

    let p = provider(&server, 0);
    let mut stream = p.stream(request(), CancellationToken::new()).await.unwrap();
    let mut events = Vec::new();
    while let Some(ev) = stream.next().await {
        events.push(ev.unwrap());
    }
    assert_eq!(events[0], ModelEvent::TextDelta("ok".into()));
    let Some(ModelEvent::Completed(resp)) = events.last() else {
        panic!("{events:?}")
    };
    assert_eq!(resp.stop_reason, StopReason::ToolUse);
    assert_eq!(resp.usage.input_tokens, 10);
    assert_eq!(resp.usage.output_tokens, 7);
    assert_eq!(
        resp.message.content[1],
        ContentBlock::ToolUse {
            id: "toolu_1".into(),
            name: "get_time".into(),
            input: json!({"tz": "UTC"}),
        }
    );

    let sent: Vec<Request> = server.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&sent[0].body).unwrap();
    assert_eq!(body["model"], "claude-opus-5-5");
    assert_eq!(body["stream"], true);
    assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
    assert_eq!(body["tools"][0]["name"], "get_time");
    // Not the official host: no beta-only fields.
    assert!(body.get("fallbacks").is_none());
    assert!(sent[0].headers.get("anthropic-beta").is_none());
}

#[tokio::test]
async fn retries_overloaded_then_succeeds() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(error_body(529, "overloaded_error", "Overloaded"))
        .up_to_n_times(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ok_body())
        .mount(&server)
        .await;

    let p = provider(&server, 4);
    let resp = complete(&p, request(), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(resp.message.text(), "ok");
    assert_eq!(server.received_requests().await.unwrap().len(), 3);
}

#[tokio::test]
async fn retries_rate_limit_honouring_retry_after() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            error_body(429, "rate_limit_error", "slow down").insert_header("retry-after", "0"),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ok_body())
        .mount(&server)
        .await;

    let p = provider(&server, 2);
    let resp = complete(&p, request(), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(resp.message.text(), "ok");
}

#[tokio::test]
async fn retries_an_error_event_before_any_output() {
    let server = MockServer::start().await;
    let overloaded = sse(&[
        json!({"type":"message_start","message":{"id":"m","type":"message","role":"assistant","content":[],"usage":{"input_tokens":10,"output_tokens":1}}}),
        json!({"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}),
    ]);
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(overloaded),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ok_body())
        .mount(&server)
        .await;

    let resp = complete(&provider(&server, 2), request(), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(resp.message.text(), "ok");
}

#[tokio::test]
async fn exhausted_retries_map_to_final_errors() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(error_body(529, "overloaded_error", "Overloaded"))
        .mount(&server)
        .await;
    let err = complete(&provider(&server, 2), request(), CancellationToken::new())
        .await
        .unwrap_err();
    match err {
        ModelError::Unavailable(m) => assert!(m.contains("gave up after 2 retries"), "{m}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(server.received_requests().await.unwrap().len(), 3);

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(error_body(429, "rate_limit_error", "slow down"))
        .mount(&server)
        .await;
    let err = complete(&provider(&server, 1), request(), CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(err, ModelError::RateLimited(_)), "{err:?}");
}

#[tokio::test]
async fn client_errors_are_not_retried() {
    let cases = [
        (
            400,
            "invalid_request_error",
            "prompt is too long: 1000123 tokens > 1000000 maximum",
        ),
        (400, "invalid_request_error", "messages.0: bad role"),
        (401, "authentication_error", "invalid x-api-key"),
    ];
    for (status, kind, message) in cases {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(error_body(status, kind, message))
            .mount(&server)
            .await;
        let err = complete(&provider(&server, 3), request(), CancellationToken::new())
            .await
            .unwrap_err();
        match (status, message.starts_with("prompt"), err) {
            (400, true, ModelError::ContextOverflow(m)) => assert!(m.contains("too long")),
            (400, false, ModelError::InvalidRequest(m)) => assert!(m.contains("bad role")),
            (401, _, ModelError::Auth(m)) => assert!(m.contains("invalid x-api-key")),
            (_, _, other) => panic!("{status} {message}: {other:?}"),
        }
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
}

#[tokio::test]
async fn cancellation_while_waiting_for_the_server() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ok_body().set_delay(Duration::from_secs(30)))
        .mount(&server)
        .await;
    let cancel = CancellationToken::new();
    let c = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        c.cancel();
    });
    let started = Instant::now();
    let err = provider(&server, 0)
        .stream(request(), cancel)
        .await
        .err()
        .unwrap();
    assert!(matches!(err, ModelError::Cancelled), "{err:?}");
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn cancellation_during_backoff() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            error_body(529, "overloaded_error", "Overloaded").insert_header("retry-after", "30"),
        )
        .mount(&server)
        .await;
    let cancel = CancellationToken::new();
    let c = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        c.cancel();
    });
    let started = Instant::now();
    let err = provider(&server, 4)
        .stream(request(), cancel)
        .await
        .err()
        .unwrap();
    assert!(matches!(err, ModelError::Cancelled), "{err:?}");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[test]
fn registers_without_an_api_key() {
    let cfg = ch_kernel::Config::parse(
        "[plugins.provider-anthropic]\napi_key_env = \"CH_TEST_SURELY_UNSET_KEY_VAR\"\nmax_retries = 1",
    )
    .unwrap();
    let registry = ch_kernel::RegistryBuilder::build(
        vec![ch_provider_anthropic::plugin()],
        &cfg,
        std::path::Path::new("/tmp"),
    )
    .unwrap();
    let p = registry.provider(Some("anthropic")).unwrap();
    assert_eq!(p.context_window("claude-opus-5-5"), Some(1_000_000));
    assert_eq!(p.context_window("unknown"), None);
}

#[tokio::test]
async fn missing_key_fails_at_call_time_with_auth() {
    let p = AnthropicProvider::new(AnthropicConfig {
        api_key_env: "CH_TEST_SURELY_UNSET_KEY_VAR".into(),
        ..Default::default()
    })
    .unwrap();
    let err = p
        .stream(request(), CancellationToken::new())
        .await
        .err()
        .unwrap();
    match err {
        ModelError::Auth(m) => assert!(m.contains("CH_TEST_SURELY_UNSET_KEY_VAR")),
        other => panic!("{other:?}"),
    }
}
