//! The `web_fetch` tool.

use async_trait::async_trait;
use ch_kernel::{Cx, Tool, ToolOutput, ToolSpec};
use reqwest::Client;
use serde_json::{Value, json};
use url::Url;

use crate::WebFetchConfig;
use crate::convert::{Kind, classify, excerpt, html_to_markdown, html_to_text};
use crate::fetch::{self, Fetched};

/// How to present an HTML page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    Markdown,
    Text,
    Raw,
}

/// Fetches http(s) URLs and returns their content as text.
#[derive(Debug, Clone)]
pub struct WebFetchTool {
    config: WebFetchConfig,
    client: Client,
}

impl WebFetchTool {
    /// Builds the tool and its HTTP client. Fails only if the client can't be built.
    pub fn new(config: WebFetchConfig) -> anyhow::Result<Self> {
        let client = fetch::client(&config)?;
        Ok(Self { config, client })
    }

    fn description(&self) -> String {
        let c = &self.config;
        format!(
            "Fetch a web page or other HTTP resource and return its content as text.

When: reading documentation, API references, changelogs, issues or any page the user \
links to or that you need in order to answer accurately. Not for files in the workspace.

Protocol:
- `url` must be an absolute http:// or https:// URL. Redirects within the same site are \
followed (up to {}); a redirect to another site is reported instead, so fetch that URL in \
a new call if you still need it.
- `format`: `markdown` (default) converts HTML to markdown and drops scripts, styles and \
navigation; `text` returns only the visible text; `raw` returns the body unchanged. JSON \
and plain-text responses are returned as-is in every format.
- The download stops at {} bytes and long results are truncated in the middle, so fetch \
the most specific page you can rather than a site index.
- This is a plain GET without cookies or credentials. Pages that render with JavaScript \
may come back nearly empty; look for an API, raw file or docs URL instead.

Error modes:
- Not http(s), or not a valid URL: rejected. Pass a full URL like https://example.com/page.
- DNS or connection failure, or a timeout after {}s: check the host for typos, retry \
once, or use another source.
- A host name that resolves to a private or local address: refused. To reach a local \
server, put its IP address or localhost in the URL.
- HTTP 4xx/5xx: the error includes the status and a short body excerpt. 404 means the \
page moved or the URL is wrong; 401/403 means it needs login, so it can't be fetched; \
429 and 5xx may work on a later retry.
- Binary content (images, PDFs, archives): rejected. Find an HTML or text version.

Side effects: none in the workspace. The request is visible to the remote server.",
            c.max_redirects, c.max_bytes, c.timeout_secs
        )
    }

    async fn fetch(&self, input: &Value) -> ToolOutput {
        let raw_url = match input.get("url").and_then(Value::as_str).map(str::trim) {
            Some(u) if !u.is_empty() => u,
            _ => {
                return ToolOutput::error(
                    "Missing `url`: pass an absolute http:// or https:// URL as a string.",
                );
            }
        };
        let format = match input.get("format") {
            None | Some(Value::Null) => Format::Markdown,
            Some(v) => match v.as_str() {
                Some("markdown") => Format::Markdown,
                Some("text") => Format::Text,
                Some("raw") => Format::Raw,
                _ => {
                    return ToolOutput::error(format!(
                        "Unknown format {v}. Use \"markdown\" (default), \"text\" or \"raw\"."
                    ));
                }
            },
        };
        let url = match Url::parse(raw_url) {
            Ok(u) => u,
            Err(e) => {
                return ToolOutput::error(format!(
                    "`{raw_url}` is not a valid URL ({e}). Pass an absolute URL such as \
                     https://example.com/page."
                ));
            }
        };
        if !matches!(url.scheme(), "http" | "https") {
            return ToolOutput::error(format!(
                "Only http and https URLs can be fetched (got `{}:`). For local files use \
                 the file tools; otherwise pass an http(s) URL.",
                url.scheme()
            ));
        }

        let fetched =
            match fetch::get(&self.client, &url, self.config.max_bytes, &self.config).await {
                Ok(f) => f,
                Err(message) => {
                    return ToolOutput::error(message).with_metadata(json!({"url": url.as_str()}));
                }
            };
        self.render(&url, format, fetched)
    }

    fn render(&self, url: &Url, format: Format, fetched: Fetched) -> ToolOutput {
        if let Some(to) = &fetched.redirect_to {
            return ToolOutput::ok(format!(
                "{} redirects (HTTP {}) to {to}, which is on a different site, so it was not \
                 followed. Call web_fetch with that URL if you still need it.",
                fetched.final_url,
                fetched.status.as_u16()
            ))
            .with_metadata(json!({
                "url": url.as_str(),
                "final_url": fetched.final_url.as_str(),
                "status": fetched.status.as_u16(),
                "redirect_to": to.as_str(),
            }));
        }
        let ct = fetched.content_type.as_deref();
        let metadata = json!({
            "url": url.as_str(),
            "final_url": fetched.final_url.as_str(),
            "status": fetched.status.as_u16(),
            "content_type": ct,
            "bytes": fetched.body.len(),
            "truncated": fetched.truncated,
        });

        if fetched.status.is_client_error() || fetched.status.is_server_error() {
            let body = excerpt(ct, &fetched.body, 300);
            let hint = match fetched.status.as_u16() {
                401 | 403 => {
                    "The page needs authorization and can't be fetched; find a public source."
                }
                404 | 410 => "Check the URL for typos or find where the page moved.",
                429 => "The server is rate limiting; wait before retrying or use another source.",
                s if s >= 500 => {
                    "The server failed; a later retry may work, or use another source."
                }
                _ => "Check the URL and request.",
            };
            let excerpt = if body.is_empty() {
                String::new()
            } else {
                format!(" Body excerpt: {body}")
            };
            return ToolOutput::error(format!(
                "HTTP {} from {}.{excerpt}\n{hint}",
                fetched.status, fetched.final_url
            ))
            .with_metadata(metadata);
        }

        let kind = classify(ct, &fetched.body);
        if kind == Kind::Binary {
            return ToolOutput::error(format!(
                "{} returned binary content ({}), which web_fetch can't show as text. Look \
                 for an HTML or plain-text version of this resource.",
                fetched.final_url,
                ct.unwrap_or("no content type")
            ))
            .with_metadata(metadata);
        }

        let text = String::from_utf8_lossy(&fetched.body);
        let body = match (kind, format) {
            (Kind::Html, Format::Markdown) => html_to_markdown(&text),
            (Kind::Html, Format::Text) => html_to_text(&text),
            _ => text.into_owned(),
        };

        let mut out = format!(
            "Fetched {} ({}, {} bytes)",
            fetched.final_url,
            ct.unwrap_or("unknown content type"),
            fetched.body.len()
        );
        if fetched.final_url != *url {
            out.push_str(&format!(" — redirected from {url}"));
        }
        out.push_str("\n\n");
        if body.trim().is_empty() {
            out.push_str(
                "(the page has no text content; it may be rendered by JavaScript. Try \
                 format \"raw\" or find an API or docs URL)",
            );
        } else {
            out.push_str(&body);
        }
        if fetched.truncated {
            out.push_str(&format!(
                "\n\n[download stopped at {} bytes (max_bytes); the rest of the resource \
                 was not fetched]",
                self.config.max_bytes
            ));
        }
        ToolOutput::ok(out).with_metadata(metadata)
    }
}

#[async_trait]
impl Tool for WebFetchTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "web_fetch".into(),
            description: self.description(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "Absolute http:// or https:// URL to fetch."
                    },
                    "format": {
                        "type": "string",
                        "enum": ["markdown", "text", "raw"],
                        "description": "How to return HTML: markdown (default), text, or raw."
                    }
                },
                "required": ["url"],
                "additionalProperties": false
            }),
            read_only: true,
        }
    }

    async fn call(&self, input: Value, cx: &Cx) -> ToolOutput {
        tokio::select! {
            out = self.fetch(&input) => out,
            _ = cx.cancel.cancelled() => ToolOutput::error("Cancelled before the fetch finished."),
        }
    }

    fn summarize(&self, input: &Value) -> String {
        let url = input.get("url").and_then(Value::as_str).unwrap_or("");
        ch_kernel::util::truncate_middle(url, 120)
    }
}
