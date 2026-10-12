//! `web_fetch` tests against a local mock server.

use std::path::Path;
use std::time::Duration;

use ch_kernel::{Config, Cx, RegistryBuilder, Tool, ToolOutput};
use ch_tool_web_fetch::{WebFetchConfig, WebFetchTool};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn tool_with(config: WebFetchConfig) -> WebFetchTool {
    WebFetchTool::new(config).unwrap()
}

async fn fetch(tool: &WebFetchTool, input: Value) -> ToolOutput {
    tool.call(input, &Cx::detached("/tmp")).await
}

async fn serve(route: &str, response: ResponseTemplate) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(route))
        .respond_with(response)
        .mount(&server)
        .await;
    server
}

const PAGE: &str = r#"<!doctype html><html><head><title>Docs</title>
<style>body { color: red }</style><script>trackEverything()</script></head>
<body><nav><a href="/">Home</a> | <a href="/about">About</a></nav>
<h1>Install</h1><p>Run <code>cargo add foo</code>, then see <a href="https://foo.dev/guide">the guide</a>.</p>
<ul><li>fast</li><li>small</li></ul></body></html>"#;

#[tokio::test]
async fn html_becomes_markdown() {
    let server = serve(
        "/docs",
        ResponseTemplate::new(200).set_body_raw(PAGE, "text/html; charset=utf-8"),
    )
    .await;
    let url = format!("{}/docs", server.uri());
    let out = fetch(&tool_with(Default::default()), json!({ "url": url })).await;
    assert!(!out.is_error, "{}", out.content);
    let c = &out.content;
    assert!(c.starts_with(&format!("Fetched {url} (text/html")), "{c}");
    assert!(c.contains("# Install"), "{c}");
    assert!(c.contains("`cargo add foo`"), "{c}");
    assert!(c.contains("[the guide](https://foo.dev/guide)"), "{c}");
    assert!(c.contains("fast") && c.contains("small"), "{c}");
    assert!(!c.contains("trackEverything"), "{c}");
    assert!(!c.contains("color: red"), "{c}");
    assert!(!c.contains("About"), "{c}");
    assert_eq!(out.metadata["status"], json!(200));
    assert_eq!(out.metadata["truncated"], json!(false));
}

#[tokio::test]
async fn text_and_raw_formats() {
    let server = serve(
        "/docs",
        ResponseTemplate::new(200).set_body_raw(PAGE, "text/html"),
    )
    .await;
    let url = format!("{}/docs", server.uri());
    let tool = tool_with(Default::default());
    let text = fetch(&tool, json!({ "url": url, "format": "text" })).await;
    assert!(text.content.contains("see the guide."), "{}", text.content);
    assert!(!text.content.contains("](https"), "{}", text.content);
    let raw = fetch(&tool, json!({ "url": url, "format": "raw" })).await;
    assert!(raw.content.contains("<script>trackEverything()</script>"));
    let bad = fetch(&tool, json!({ "url": url, "format": "pdf" })).await;
    assert!(bad.is_error && bad.content.contains("Unknown format"));
}

#[tokio::test]
async fn json_passes_through() {
    let body = r#"{"name":"ch","tags":["a","b"]}"#;
    let server = serve(
        "/api",
        ResponseTemplate::new(200).set_body_raw(body, "application/json"),
    )
    .await;
    let out = fetch(
        &tool_with(Default::default()),
        json!({ "url": format!("{}/api", server.uri()) }),
    )
    .await;
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.ends_with(body), "{}", out.content);
}

#[tokio::test]
async fn plain_text_passes_through() {
    let server = serve(
        "/notes.txt",
        ResponseTemplate::new(200).set_body_raw("line *one*\nline <two>", "text/plain"),
    )
    .await;
    let out = fetch(
        &tool_with(Default::default()),
        json!({ "url": format!("{}/notes.txt", server.uri()) }),
    )
    .await;
    assert!(
        out.content.ends_with("line *one*\nline <two>"),
        "{}",
        out.content
    );
}

#[tokio::test]
async fn http_404_is_a_recoverable_error_with_excerpt() {
    let server = serve(
        "/gone",
        ResponseTemplate::new(404).set_body_raw(
            "<html><body><h1>Page not found</h1><p>Sorry.</p></body></html>",
            "text/html",
        ),
    )
    .await;
    let out = fetch(
        &tool_with(Default::default()),
        json!({ "url": format!("{}/gone", server.uri()) }),
    )
    .await;
    assert!(out.is_error);
    assert!(
        out.content.starts_with("HTTP 404 Not Found"),
        "{}",
        out.content
    );
    assert!(
        out.content.contains("Body excerpt: Page not found Sorry."),
        "{}",
        out.content
    );
    assert!(out.content.contains("Check the URL"), "{}", out.content);
    assert_eq!(out.metadata["status"], json!(404));
}

#[tokio::test]
async fn http_500_suggests_retry() {
    let server = serve("/boom", ResponseTemplate::new(503)).await;
    let out = fetch(
        &tool_with(Default::default()),
        json!({ "url": format!("{}/boom", server.uri()) }),
    )
    .await;
    assert!(out.is_error);
    assert!(out.content.starts_with("HTTP 503"), "{}", out.content);
    assert!(out.content.contains("retry"), "{}", out.content);
}

#[tokio::test]
async fn non_http_schemes_and_bad_urls_are_rejected() {
    let tool = tool_with(Default::default());
    for url in [
        "file:///etc/passwd",
        "ftp://example.com/x",
        "javascript:alert(1)",
    ] {
        let out = fetch(&tool, json!({ "url": url })).await;
        assert!(out.is_error, "{url}");
        assert!(
            out.content.contains("Only http and https"),
            "{}",
            out.content
        );
    }
    let out = fetch(&tool, json!({ "url": "example.com/page" })).await;
    assert!(
        out.is_error && out.content.contains("not a valid URL"),
        "{}",
        out.content
    );
    let out = fetch(&tool, json!({})).await;
    assert!(out.is_error && out.content.contains("Missing `url`"));
}

#[tokio::test]
async fn oversize_bodies_are_cut_at_max_bytes() {
    let body = "a".repeat(50_000);
    let server = serve(
        "/big",
        ResponseTemplate::new(200).set_body_raw(body, "text/plain"),
    )
    .await;
    let tool = tool_with(WebFetchConfig {
        max_bytes: 1_000,
        ..Default::default()
    });
    let out = fetch(&tool, json!({ "url": format!("{}/big", server.uri()) })).await;
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(out.metadata["bytes"], json!(1_000));
    assert_eq!(out.metadata["truncated"], json!(true));
    assert!(out.content.contains(&"a".repeat(1_000)));
    assert!(!out.content.contains(&"a".repeat(1_001)));
    assert!(
        out.content
            .ends_with("the rest of the resource was not fetched]"),
        "{}",
        out.content
    );
}

#[tokio::test]
async fn binary_content_types_are_rejected() {
    let server = serve(
        "/logo.png",
        ResponseTemplate::new(200).set_body_raw(vec![0x89, b'P', b'N', b'G', 0, 1, 2], "image/png"),
    )
    .await;
    let out = fetch(
        &tool_with(Default::default()),
        json!({ "url": format!("{}/logo.png", server.uri()) }),
    )
    .await;
    assert!(out.is_error);
    assert!(
        out.content.contains("binary content (image/png)"),
        "{}",
        out.content
    );
}

#[tokio::test]
async fn connection_failures_are_recoverable() {
    // Grab a free port, then close it so the connection is refused.
    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let out = fetch(
        &tool_with(Default::default()),
        json!({ "url": format!("http://127.0.0.1:{port}/") }),
    )
    .await;
    assert!(out.is_error);
    assert!(out.content.contains("Could not connect"), "{}", out.content);
}

#[tokio::test]
async fn slow_servers_time_out() {
    let server = serve(
        "/slow",
        ResponseTemplate::new(200)
            .set_body_string("late")
            .set_delay(Duration::from_secs(5)),
    )
    .await;
    let tool = tool_with(WebFetchConfig {
        timeout_secs: 1,
        ..Default::default()
    });
    let out = fetch(&tool, json!({ "url": format!("{}/slow", server.uri()) })).await;
    assert!(out.is_error);
    assert!(
        out.content.contains("timed out after 1s"),
        "{}",
        out.content
    );
}

#[tokio::test]
async fn redirects_are_followed_and_reported() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/old"))
        .respond_with(ResponseTemplate::new(301).insert_header("location", "/new"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/new"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("moved here", "text/plain"))
        .mount(&server)
        .await;
    let old = format!("{}/old", server.uri());
    let out = fetch(&tool_with(Default::default()), json!({ "url": old })).await;
    assert!(!out.is_error, "{}", out.content);
    assert!(
        out.content.contains(&format!("redirected from {old}")),
        "{}",
        out.content
    );
    assert!(out.content.ends_with("moved here"));
}

#[tokio::test]
async fn redirects_to_another_site_are_reported_not_followed() {
    let other = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("secret"))
        .expect(0)
        .mount(&other)
        .await;
    let target = format!("{}/internal", other.uri());
    let server = serve(
        "/go",
        ResponseTemplate::new(302).insert_header("location", target.as_str()),
    )
    .await;
    let out = fetch(
        &tool_with(Default::default()),
        json!({ "url": format!("{}/go", server.uri()) }),
    )
    .await;
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.contains("different site"), "{}", out.content);
    assert!(out.content.contains(&target), "{}", out.content);
    assert!(!out.content.contains("secret"));
    assert_eq!(out.metadata["redirect_to"], json!(target));
}

#[tokio::test]
async fn too_many_same_site_redirects_are_an_error() {
    let server = serve(
        "/loop",
        ResponseTemplate::new(302).insert_header("location", "/loop"),
    )
    .await;
    let tool = tool_with(WebFetchConfig {
        max_redirects: 2,
        ..Default::default()
    });
    let out = fetch(&tool, json!({ "url": format!("{}/loop", server.uri()) })).await;
    assert!(out.is_error);
    assert!(
        out.content.contains("more than 2 redirects"),
        "{}",
        out.content
    );
}

#[test]
fn plugin_registers_a_read_only_tool() {
    let registry = RegistryBuilder::build(
        vec![ch_tool_web_fetch::plugin()],
        &Config::default(),
        Path::new("/tmp"),
    )
    .unwrap();
    let tool = registry.tool("web_fetch").unwrap();
    let spec = tool.spec();
    assert!(spec.read_only);
    assert_eq!(spec.input_schema["required"], json!(["url"]));
    let d = &spec.description;
    let order: Vec<usize> = ["When:", "Protocol:", "Error modes:", "Side effects:"]
        .iter()
        .map(|s| d.find(s).unwrap())
        .collect();
    assert!(order.windows(2).all(|w| w[0] < w[1]));
    assert_eq!(
        tool.summarize(&json!({"url": "https://docs.rs/tokio"})),
        "https://docs.rs/tokio"
    );
}
