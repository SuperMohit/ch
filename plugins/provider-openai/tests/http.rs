//! HTTP-level tests against a local mock server: auth header, retries, error mapping,
//! cancellation.

use std::time::{Duration, Instant};

use ch_kernel::reexports::CancellationToken;
use ch_kernel::{Message, ModelError, ModelProvider, ModelRequest, StopReason, ToolSpec, complete};
use ch_provider_openai::{OpenAiConfig, OpenAiProvider};
use serde_json::{Value, json};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn provider(server: &MockServer, max_retries: u32) -> OpenAiProvider {
    OpenAiProvider::new(OpenAiConfig {
        base_url: format!("{}/v1", server.uri()),
        api_key_env: "CH_TEST_SURELY_UNSET_OPENAI_VAR".into(),
        max_retries,
        retry_base_ms: 10,
        context_window: Some(32_000),
        ..Default::default()
    })
    .unwrap()
}

fn request() -> ModelRequest {
    ModelRequest {
        model: "local-model".into(),
        system: vec!["Be brief.".into()],
        messages: vec![Message::user("hi")],
        tools: vec![ToolSpec {
            name: "get_time".into(),
            description: "Returns the time".into(),
            input_schema: json!({"type": "object", "properties": {}}),
            read_only: true,
        }],
        max_tokens: 256,
        temperature: Some(0.0),
        thinking_budget: None,
        purpose: "agent".into(),
        volatile_tail: 0,
    }
}

fn ok_body() -> ResponseTemplate {
    let chunks = [
        json!({"choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"get_time","arguments":""}}]},"finish_reason":null}]}),
        json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"tz\":\"UTC\"}"}}]},"finish_reason":null}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
        json!({"choices":[],"usage":{"prompt_tokens":30,"completion_tokens":5}}),
    ];
    let mut body: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
    body.push_str("data: [DONE]\n\n");
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(body)
}

fn error_body(status: u16, message: &str, code: &str) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_json(
        json!({"error":{"message":message,"type":"invalid_request_error","code":code}}),
    )
}

#[tokio::test]
async fn streams_tool_calls_without_a_key_on_local_servers() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ok_body())
        .expect(1)
        .mount(&server)
        .await;
    let p = provider(&server, 0);
    assert_eq!(p.context_window("anything"), Some(32_000));
    let resp = complete(&p, request(), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(resp.stop_reason, StopReason::ToolUse);
    assert_eq!(resp.message.tool_calls()[0].input, json!({"tz": "UTC"}));
    assert_eq!(resp.usage.input_tokens, 30);

    let sent = server.received_requests().await.unwrap();
    assert!(sent[0].headers.get("authorization").is_none());
    let body: Value = serde_json::from_slice(&sent[0].body).unwrap();
    assert_eq!(body["stream"], true);
    assert_eq!(body["max_tokens"], 256);
    assert_eq!(body["messages"][0]["role"], "system");
}

#[tokio::test]
async fn sends_bearer_key_when_set() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(header("authorization", "Bearer sk-test"))
        .respond_with(ok_body())
        .expect(1)
        .mount(&server)
        .await;
    complete(
        &provider(&server, 0).with_api_key("sk-test"),
        request(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn retries_429_and_5xx_then_succeeds() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            error_body(429, "Rate limit reached", "rate_limit_exceeded")
                .insert_header("retry-after", "0"),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503).set_body_string("busy"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ok_body())
        .mount(&server)
        .await;
    let resp = complete(&provider(&server, 3), request(), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(resp.stop_reason, StopReason::ToolUse);
    assert_eq!(server.received_requests().await.unwrap().len(), 3);
}

#[tokio::test]
async fn exhausted_retries_and_client_errors() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(502).set_body_string("bad gateway"))
        .mount(&server)
        .await;
    let err = complete(&provider(&server, 2), request(), CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(err, ModelError::Unavailable(_)), "{err:?}");
    assert_eq!(server.received_requests().await.unwrap().len(), 3);

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(error_body(
            400,
            "This model's maximum context length is 8192 tokens.",
            "context_length_exceeded",
        ))
        .mount(&server)
        .await;
    let err = complete(&provider(&server, 2), request(), CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(err, ModelError::ContextOverflow(_)), "{err:?}");
    assert_eq!(server.received_requests().await.unwrap().len(), 1);

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(error_body(401, "Incorrect API key", "invalid_api_key"))
        .mount(&server)
        .await;
    let err = complete(&provider(&server, 2), request(), CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(err, ModelError::Auth(_)), "{err:?}");
}

#[tokio::test]
async fn cancellation_while_waiting() {
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

#[test]
fn registers_without_an_api_key() {
    let cfg = ch_kernel::Config::parse(
        "[plugins.provider-openai]\napi_key_env = \"CH_TEST_SURELY_UNSET_OPENAI_VAR\"\ncontext_window = 128000",
    )
    .unwrap();
    let registry = ch_kernel::RegistryBuilder::build(
        vec![ch_provider_openai::plugin()],
        &cfg,
        std::path::Path::new("/tmp"),
    )
    .unwrap();
    let p = registry.provider("openai").unwrap();
    assert_eq!(p.context_window("gpt-x"), Some(128_000));
}
