//! One MCP tool as a kernel [`Tool`], and how its results become observations.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use ch_kernel::{Cx, Tool, ToolOutput, ToolSpec};
use rmcp::RoleClient;
use rmcp::model::{
    CallToolRequest, CallToolRequestParams, CancelledNotificationParam, ClientRequest, ServerResult,
};
use rmcp::service::{Peer, PeerRequestOptions, ServiceError};
use serde_json::Value;

use crate::redact::Redactor;

/// What the tools of one connection share.
pub struct Link {
    /// The server's name, for messages.
    pub server: String,
    pub peer: Peer<RoleClient>,
    pub call_timeout: Duration,
    pub redactor: Arc<Redactor>,
}

pub struct McpTool {
    spec: ToolSpec,
    /// The name the server knows the tool by.
    raw: String,
    link: Arc<Link>,
}

impl McpTool {
    /// The kernel tool for one entry of `tools/list`, or `None` without a name.
    pub fn from_listed(tool: &Value, prefix: &str, link: &Arc<Link>) -> Option<Self> {
        let raw = tool.get("name")?.as_str()?.to_string();
        let description = tool
            .get("description")
            .and_then(Value::as_str)
            .or_else(|| tool.get("title").and_then(Value::as_str))
            .unwrap_or_default()
            .to_string();
        let input_schema = tool.get("inputSchema").cloned().unwrap_or(Value::Null);
        Some(Self {
            spec: ToolSpec {
                name: ch_kernel::tool_source::outside_tool_name(prefix, &raw),
                description,
                input_schema,
                // Ignored: the session sets it from the user's `read_only_tools`.
                read_only: false,
            },
            raw,
            link: link.clone(),
        })
    }

    fn fail(&self, what: &str) -> ToolOutput {
        let text = format!(
            "`{}` (MCP server `{}`): {what}",
            self.spec.name, self.link.server
        );
        ToolOutput::error(self.link.redactor.redact(&text))
    }
}

#[async_trait]
impl Tool for McpTool {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    async fn call(&self, input: Value, cx: &Cx) -> ToolOutput {
        let arguments = match input {
            Value::Object(map) => map,
            Value::Null => Default::default(),
            other => {
                return ToolOutput::error(format!(
                    "`{}` takes a JSON object, got {other}",
                    self.spec.name
                ));
            }
        };
        let params = CallToolRequestParams::new(self.raw.clone()).with_arguments(arguments);
        let request = ClientRequest::CallToolRequest(CallToolRequest::new(params));
        let timeout = self.link.call_timeout;
        let peer = &self.link.peer;
        // The timeout is ours, not rmcp's: rmcp's awaits its cancel notification, which
        // waits behind a write blocked on a full pipe. Ours sends it from a task.
        let sent = std::sync::Mutex::new(None);
        let call = async {
            let handle = peer
                .send_request_with_option(request, PeerRequestOptions::no_options())
                .await?;
            *sent.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle.id.clone());
            handle.await_response().await
        };
        let (result, why) = tokio::select! {
            r = tokio::time::timeout(timeout, call) => match r {
                Ok(r) => (Some(r), ""),
                Err(_) => (None, "timeout"),
            },
            _ = cx.cancel.cancelled() => (None, "cancelled"),
        };
        match result {
            Some(Ok(ServerResult::CallToolResult(result))) => match serde_json::to_value(result) {
                Ok(v) => convert_result(&v, &self.link.redactor),
                Err(e) => self.fail(&format!("its result couldn't be read: {e}")),
            },
            Some(Ok(_)) => self.fail("it answered with something other than a tool result"),
            Some(Err(e)) => self.fail(&describe_error(&e)),
            None => {
                let id = sent.lock().unwrap_or_else(|e| e.into_inner()).take();
                let reason = if why == "timeout" {
                    "the call timed out"
                } else {
                    "the user interrupted the turn"
                };
                if let Some(id) = id {
                    let peer = peer.clone();
                    let param = CancelledNotificationParam::new(Some(id), Some(reason.into()));
                    tokio::spawn(async move {
                        let _ = peer.notify_cancelled(param).await;
                    });
                }
                if why == "timeout" {
                    self.fail(&format!(
                        "no answer within {} s, so ch asked the server to cancel the call",
                        timeout.as_secs()
                    ))
                } else {
                    self.fail("cancelled: the user interrupted the turn")
                }
            }
        }
    }
}

/// A call's failure, for the model.
fn describe_error(e: &ServiceError) -> String {
    match e {
        ServiceError::McpError(data) => format!("the server returned an error: {}", data.message),
        ServiceError::TransportClosed | ServiceError::TransportSend(_) => {
            "the connection to the server is closed (it may have exited)".into()
        }
        ServiceError::Cancelled { reason } => format!(
            "the call was cancelled{}",
            reason
                .as_deref()
                .map(|r| format!(": {r}"))
                .unwrap_or_default()
        ),
        other => format!("the call failed: {other}"),
    }
}

/// A `tools/call` result (as wire JSON) as an observation: text items joined, images
/// and audio as placeholders, `structuredContent` when there is no text, and
/// `isError` as an error. All of it redacted.
pub fn convert_result(result: &Value, redactor: &Redactor) -> ToolOutput {
    let mut parts: Vec<String> = Vec::new();
    let mut has_text = false;
    for item in result
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let s = |k: &str| item.get(k).and_then(Value::as_str);
        match s("type").unwrap_or_default() {
            "text" => {
                has_text = true;
                parts.push(s("text").unwrap_or_default().to_string());
            }
            kind @ ("image" | "audio") => parts.push(omitted(
                kind,
                s("mimeType").unwrap_or("unknown type"),
                s("data").unwrap_or_default(),
            )),
            "resource" => {
                let res = item.get("resource");
                let r = |k: &str| res.and_then(|r| r.get(k)).and_then(Value::as_str);
                match r("text") {
                    Some(text) => {
                        has_text = true;
                        parts.push(text.to_string());
                    }
                    None => parts.push(omitted(
                        "resource",
                        r("mimeType").unwrap_or("unknown type"),
                        r("blob").unwrap_or_default(),
                    )),
                }
            }
            "resource_link" => {
                parts.push(format!("[resource link: {}]", s("uri").unwrap_or_default()))
            }
            other => parts.push(format!("[{other} content omitted]")),
        }
    }
    if !has_text && let Some(structured) = result.get("structuredContent").filter(|v| !v.is_null())
    {
        parts.push(structured.to_string());
    }
    let text = if parts.is_empty() {
        "(no output)".to_string()
    } else {
        parts.join("\n")
    };
    let text = redactor.redact(&text);
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        ToolOutput::error(text)
    } else {
        ToolOutput::ok(text)
    }
}

/// `[image omitted: image/png, 48 KB]`, sized from the base64 data.
fn omitted(kind: &str, mime: &str, base64: &str) -> String {
    let bytes = base64.trim_end_matches('=').len() * 3 / 4;
    let size = if bytes >= 1024 {
        format!("{} KB", bytes.div_ceil(1024))
    } else {
        format!("{bytes} B")
    };
    format!(
        "[{kind} omitted: {}, {size}]",
        ch_kernel::util::printable(mime)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn convert(v: Value) -> ToolOutput {
        convert_result(&v, &Redactor::new(["hunter2-token".to_string()]))
    }

    #[test]
    fn text_is_joined_and_media_omitted() {
        let out = convert(json!({"content": [
            {"type": "text", "text": "one"},
            {"type": "image", "mimeType": "image/png", "data": "A".repeat(65536)},
            {"type": "audio", "mimeType": "audio/wav", "data": "AAAA"},
            {"type": "text", "text": "two hunter2-token"},
            {"type": "resource", "resource": {"uri": "file:///a", "text": "res"}},
            {"type": "resource_link", "uri": "file:///b", "name": "b"},
        ], "structuredContent": {"ignored": true}}));
        assert!(!out.is_error);
        assert_eq!(
            out.content,
            "one\n[image omitted: image/png, 48 KB]\n[audio omitted: audio/wav, 3 B]\n\
             two [redacted]\nres\n[resource link: file:///b]"
        );
    }

    #[test]
    fn structured_content_without_text_and_errors() {
        let out = convert(json!({"content": [], "structuredContent": {"n": 1}}));
        assert_eq!(out.content, r#"{"n":1}"#);
        let out = convert(json!({"content": [{"type": "text", "text": "bad"}], "isError": true}));
        assert!(out.is_error);
        assert_eq!(out.content, "bad");
        assert_eq!(convert(json!({})).content, "(no output)");
    }
}
