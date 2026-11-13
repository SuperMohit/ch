//! Streamable HTTP servers: static headers, and no redirect to another origin.

mod common;

use ch_kernel::{Services, SourceStatus};
use common::harness;
use serde_json::{Value, json};
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// Answers each JSON-RPC request with the same id.
struct Answer(Value);

impl Respond for Answer {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
        ResponseTemplate::new(200)
            .insert_header("content-type", "application/json")
            .set_body_json(json!({"jsonrpc": "2.0", "id": body["id"], "result": self.0}))
    }
}

async fn mcp_server() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "initialize"})))
        .respond_with(Answer(json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "http-test", "version": "2"}
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(
            json!({"method": "notifications/initialized"}),
        ))
        .respond_with(ResponseTemplate::new(202))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "tools/list"})))
        .respond_with(Answer(json!({"tools": [
            {"name": "lookup", "description": "Looks up.", "inputSchema": {"type": "object"}}
        ]})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "tools/call"})))
        .respond_with(Answer(json!({"content": [
            {"type": "text", "text": "found; Authorization: Bearer tok-1234567890"}
        ]})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(405))
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn headers_are_sent_and_results_redacted() {
    let server = mcp_server().await;
    let dir = tempfile::tempdir().unwrap();
    let config = format!(
        "[[mcp_servers]]\nname = \"web\"\nurl = \"{}/mcp\"\n\
         headers = {{ Authorization = \"Bearer tok-1234567890\", X-Team = \"ch\" }}\n",
        server.uri()
    );
    let mut h = harness(dir.path(), &config, Services::default()).await;
    assert_eq!(h.status("web"), Some(SourceStatus::Ready { tools: 1 }));
    let (out, err) = h.call("mcp__web__lookup", json!({"q": "x"})).await;
    assert!(!err, "{out}");
    assert!(!out.contains("tok-1234567890"), "{out}");

    let requests = server.received_requests().await.unwrap();
    let posts: Vec<_> = requests
        .iter()
        .filter(|r| r.method.as_str() == "POST")
        .collect();
    assert!(!posts.is_empty());
    for r in posts {
        assert_eq!(
            r.headers.get("authorization").unwrap(),
            "Bearer tok-1234567890"
        );
        assert_eq!(r.headers.get("x-team").unwrap(), "ch");
    }
    h.session.end().await;
}

#[tokio::test]
async fn headers_never_follow_a_redirect_to_another_origin() {
    let elsewhere = mcp_server().await;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .respond_with(
            ResponseTemplate::new(307)
                .insert_header("location", format!("{}/mcp", elsewhere.uri())),
        )
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let config = format!(
        "[[mcp_servers]]\nname = \"web\"\nurl = \"{}/mcp\"\n\
         headers = {{ Authorization = \"Bearer tok-1234567890\" }}\nstartup_timeout_secs = 5\n",
        server.uri()
    );
    let h = harness(dir.path(), &config, Services::default()).await;
    let Some(SourceStatus::Failed { reason }) = h.status("web") else {
        panic!("{:?}", h.status("web"));
    };
    assert!(!reason.contains("tok-1234567890"), "{reason}");
    let followed = elsewhere.received_requests().await.unwrap();
    assert!(
        followed
            .iter()
            .all(|r| r.headers.get("authorization").is_none()),
        "the header went to another origin"
    );
    assert!(
        followed.is_empty(),
        "the redirect was followed: {followed:?}"
    );
}
