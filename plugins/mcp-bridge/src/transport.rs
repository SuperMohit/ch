//! Newline-delimited JSON-RPC over a server's stdin and stdout (ADR 0006 §10).
//!
//! Unlike rmcp's own stdio transport, this one fails the call a response belongs to
//! when the response can't be used: one over [`MAX_MESSAGE`] bytes, or one rmcp can't
//! read. Without that, the call would wait for its timeout. Lines that aren't JSON-RPC
//! messages (a server printing a banner to stdout) are logged and skipped.

use std::io;
use std::sync::{Arc, LazyLock};

use regex::bytes::Regex;
use rmcp::RoleClient;
use rmcp::service::{RxJsonRpcMessage, TxJsonRpcMessage};
use rmcp::transport::Transport;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncRead, AsyncWrite, AsyncWriteExt as _, BufReader};
use tokio::sync::Mutex;

/// Largest message read from a server.
pub const MAX_MESSAGE: usize = 16 * 1024 * 1024;

/// How much of an oversized line is kept to find its id.
const KEEP_HEAD: usize = 64 * 1024;
const KEEP_TAIL: usize = 1024;

/// JSON-RPC "internal error".
const INTERNAL_ERROR: i64 = -32603;

pub struct LineTransport<R, W> {
    read: BufReader<R>,
    write: Arc<Mutex<Option<W>>>,
    max: usize,
    name: String,
    /// The line read so far. Kept here, not in the `receive` future: rmcp polls
    /// `receive` inside a `select!` and drops it when another branch is ready, and bytes
    /// already consumed from the reader would be lost with it.
    partial: Vec<u8>,
    /// Set once the line passed `max`: its first and last bytes, to find its id.
    over: Option<(Vec<u8>, Vec<u8>)>,
}

impl<R, W> LineTransport<R, W>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send + 'static,
{
    /// `name` is the server's, for log lines.
    #[cfg(test)]
    fn new(read: R, write: W, name: &str) -> Self {
        Self::with_writer(read, Arc::new(Mutex::new(Some(write))), name)
    }

    /// A transport writing to `write`, which the caller can also close (taking it out of
    /// the `Option` closes the server's stdin).
    pub fn with_writer(read: R, write: Arc<Mutex<Option<W>>>, name: &str) -> Self {
        Self {
            read: BufReader::new(read),
            write,
            max: MAX_MESSAGE,
            name: name.to_string(),
            partial: Vec::new(),
            over: None,
        }
    }

    #[cfg(test)]
    fn with_max(mut self, max: usize) -> Self {
        self.max = max;
        self
    }

    /// Reads one line of at most `max` bytes; a longer one is skipped, keeping its start
    /// and end. Cancel-safe: dropping the future between reads loses nothing, and the
    /// next call goes on with the same line.
    async fn read_line(&mut self) -> Line {
        loop {
            let Self {
                read,
                partial,
                over,
                max,
                ..
            } = self;
            let available = match read.fill_buf().await {
                Ok(b) => b,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => return Line::End,
            };
            if available.is_empty() {
                // A last line without a newline still counts.
                return match over.take() {
                    Some((head, tail)) => Line::Oversized { head, tail },
                    None if !partial.is_empty() => Line::Message(std::mem::take(partial)),
                    None => Line::End,
                };
            }
            let newline = available.iter().position(|b| *b == b'\n');
            let take = newline.map_or(available.len(), |i| i + 1);
            let chunk = &available[..newline.unwrap_or(available.len())];
            match over {
                None if partial.len() + chunk.len() <= *max => partial.extend_from_slice(chunk),
                None => {
                    let mut head = std::mem::take(partial);
                    head.extend_from_slice(chunk);
                    let tail = head[head.len().saturating_sub(KEEP_TAIL)..].to_vec();
                    head.truncate(KEEP_HEAD);
                    *over = Some((head, tail));
                }
                Some((head, tail)) => {
                    if head.len() < KEEP_HEAD {
                        let room = KEEP_HEAD - head.len();
                        head.extend_from_slice(&chunk[..chunk.len().min(room)]);
                    }
                    tail.extend_from_slice(chunk);
                    let cut = tail.len().saturating_sub(KEEP_TAIL);
                    tail.drain(..cut);
                }
            }
            read.consume(take);
            if newline.is_some() {
                return match over.take() {
                    Some((head, tail)) => Line::Oversized { head, tail },
                    None => Line::Message(std::mem::take(partial)),
                };
            }
        }
    }
}

enum Line {
    Message(Vec<u8>),
    Oversized { head: Vec<u8>, tail: Vec<u8> },
    End,
}

/// An error response to request `id`, as rmcp reads one.
fn error_for(id: Value, message: &str) -> Option<RxJsonRpcMessage<RoleClient>> {
    serde_json::from_value(json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": INTERNAL_ERROR, "message": message },
    }))
    .ok()
}

/// The id of a response, when `v` is one.
fn response_id(v: &Value) -> Option<Value> {
    let obj = v.as_object()?;
    if obj.contains_key("method") || !(obj.contains_key("result") || obj.contains_key("error")) {
        return None;
    }
    obj.get("id")
        .filter(|id| id.is_number() || id.is_string())
        .cloned()
}

/// What the first bytes of a JSON object say about its top level.
#[derive(Debug, Default, PartialEq)]
struct Head {
    id: Option<Value>,
    /// A `result` or `error` key: a response.
    response: bool,
    /// A `method` key: a request or notification from the server.
    method: bool,
}

/// Reads the top-level keys of a JSON object from its first bytes.
fn scan_head(b: &[u8]) -> Head {
    let mut head = Head::default();
    let mut depth = 0i32;
    let mut expect_key = false;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'{' => {
                depth += 1;
                expect_key = depth == 1;
                i += 1;
            }
            b'[' => {
                depth += 1;
                i += 1;
            }
            b'}' | b']' => {
                depth -= 1;
                i += 1;
            }
            b',' => {
                expect_key = depth == 1;
                i += 1;
            }
            b'"' => {
                let Some(end) = string_end(b, i) else {
                    break;
                };
                if depth == 1 && expect_key {
                    expect_key = false;
                    let key = &b[i + 1..end];
                    let mut j = skip_ws(b, end + 1);
                    if b.get(j) != Some(&b':') {
                        break;
                    }
                    j = skip_ws(b, j + 1);
                    match key {
                        b"id" => head.id = scalar(&b[j..]),
                        b"result" | b"error" => head.response = true,
                        b"method" => head.method = true,
                        _ => {}
                    }
                    i = j;
                } else {
                    i = end + 1;
                }
            }
            _ => i += 1,
        }
    }
    head
}

fn skip_ws(b: &[u8], mut i: usize) -> usize {
    while b.get(i).is_some_and(u8::is_ascii_whitespace) {
        i += 1;
    }
    i
}

/// Index of the quote closing the string that opens at `start`.
fn string_end(b: &[u8], start: usize) -> Option<usize> {
    let mut i = start + 1;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'"' => return Some(i),
            _ => i += 1,
        }
    }
    None
}

/// A number or string at the start of `b`. A number running to the end of `b` may be
/// cut, so it doesn't count.
fn scalar(b: &[u8]) -> Option<Value> {
    let end = if b.first() == Some(&b'"') {
        string_end(b, 0)? + 1
    } else {
        b.iter().position(|c| !(c.is_ascii_digit() || *c == b'-'))?
    };
    serde_json::from_slice::<Value>(&b[..end])
        .ok()
        .filter(|v| v.is_number() || v.is_string())
}

/// A top-level `"id"` that ends the object, as in `..., "id": 7}`.
static TAIL_ID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"[,{]\s*"id"\s*:\s*(-?[0-9]+|"(?:[^"\\]|\\.)*")\s*\}\s*$"#).unwrap()
});

fn id_from_tail(b: &[u8]) -> Option<Value> {
    let caps = TAIL_ID.captures(b)?;
    serde_json::from_slice(caps.get(1)?.as_bytes()).ok()
}

impl<R, W> Transport<RoleClient> for LineTransport<R, W>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send + 'static,
{
    type Error = io::Error;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let write = self.write.clone();
        let line = serde_json::to_vec(&item);
        async move {
            let mut line = line.map_err(io::Error::other)?;
            line.push(b'\n');
            let mut guard = write.lock().await;
            let Some(w) = guard.as_mut() else {
                return Err(io::Error::new(
                    io::ErrorKind::NotConnected,
                    "the server's stdin is closed",
                ));
            };
            w.write_all(&line).await?;
            w.flush().await
        }
    }

    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleClient>> {
        loop {
            match self.read_line().await {
                Line::End => return None,
                Line::Oversized { head, tail } => {
                    // Only a response answers one of ch's calls; ids of requests the
                    // server sends are its own numbering.
                    let top = scan_head(&head);
                    let id = (top.response && !top.method)
                        .then(|| top.id.or_else(|| id_from_tail(&tail)))
                        .flatten();
                    tracing::warn!(
                        server = %self.name,
                        ?id,
                        "MCP server sent a message over {} bytes; skipped it",
                        self.max
                    );
                    if let Some(id) = id {
                        let message = format!(
                            "the server's response was over {} MB, the most ch reads",
                            self.max / (1024 * 1024)
                        );
                        if let Some(m) = error_for(id, &message) {
                            return Some(m);
                        }
                    }
                }
                Line::Message(line) => {
                    let text = line.strip_suffix(b"\r").unwrap_or(&line);
                    if text.iter().all(u8::is_ascii_whitespace) {
                        continue;
                    }
                    match serde_json::from_slice::<RxJsonRpcMessage<RoleClient>>(text) {
                        Ok(m) => return Some(m),
                        Err(e) => {
                            let value = serde_json::from_slice::<Value>(text).ok();
                            if let Some(id) = value.as_ref().and_then(response_id) {
                                tracing::warn!(server = %self.name, "unreadable MCP response: {e}");
                                let message =
                                    format!("the server's response couldn't be read: {e}");
                                if let Some(m) = error_for(id, &message) {
                                    return Some(m);
                                }
                            }
                            tracing::debug!(
                                server = %self.name,
                                "skipped a line that isn't a JSON-RPC message ({} bytes)",
                                text.len()
                            );
                        }
                    }
                }
            }
        }
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        let taken = self.write.lock().await.take();
        if let Some(mut w) = taken {
            let _ = w.shutdown().await;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_from_the_ends_of_big_messages() {
        let head = scan_head(br#"{"jsonrpc":"2.0","result":{"id":99,"content":[{"text":"xxxx"#);
        assert_eq!((head.id, head.response, head.method), (None, true, false));
        let head = scan_head(br#"{"jsonrpc":"2.0", "id" : "abc\"d","result":{"#);
        assert_eq!(head.id, Some(json!("abc\"d")));
        let head = scan_head(br#"{"jsonrpc":"2.0","id":42,"result":{"content":[{"text":"xx"#);
        assert_eq!((head.id, head.response), (Some(json!(42)), true));
        let head = scan_head(br#"{"jsonrpc":"2.0","id":3,"method":"sampling/x","params":{"#);
        assert_eq!(
            (head.id, head.response, head.method),
            (Some(json!(3)), false, true)
        );
        // A number cut at the end of the kept bytes isn't trusted.
        assert_eq!(scan_head(br#"{"id":12"#).id, None);
        assert_eq!(
            id_from_tail(br#"xx"}]},"jsonrpc":"2.0","id":7}"#),
            Some(json!(7))
        );
        assert_eq!(
            id_from_tail(br#"xx"}]},"jsonrpc":"2.0","id":7}  "#),
            Some(json!(7))
        );
        assert_eq!(id_from_tail(br#"xx"}],"id":7},"x":1}"#), None);
    }

    #[test]
    fn only_responses_have_an_id_to_fail() {
        assert_eq!(response_id(&json!({"id": 1, "result": {}})), Some(json!(1)));
        assert_eq!(response_id(&json!({"id": 1, "method": "x"})), None);
        assert_eq!(response_id(&json!({"id": null, "error": {}})), None);
    }

    #[tokio::test]
    async fn lines_are_skipped_or_answered() {
        let big = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{{\"content\":[{{\"type\":\"text\",\"text\":\"{}\"}}]}}}}\n",
            "x".repeat(200)
        );
        let input = format!(
            "banner text\n\n{big}{{\"jsonrpc\":\"1.0\",\"id\":4,\"result\":{{}}}}\n\
             {{\"jsonrpc\":\"2.0\",\"method\":\"notifications/tools/list_changed\"}}"
        );
        let (_w, sink) = tokio::io::duplex(64);
        let mut t = LineTransport::new(input.as_bytes(), sink, "t").with_max(100);
        let first = serde_json::to_value(t.receive().await.unwrap()).unwrap();
        assert_eq!(first["id"], 3, "{first}");
        assert!(
            first["error"]["message"].as_str().unwrap().contains("over"),
            "{first}"
        );
        let second = serde_json::to_value(t.receive().await.unwrap()).unwrap();
        assert_eq!(second["id"], 4, "{second}");
        assert!(
            second["error"]["message"]
                .as_str()
                .unwrap()
                .contains("couldn't be read"),
            "{second}"
        );
        let third = serde_json::to_value(t.receive().await.unwrap()).unwrap();
        assert_eq!(third["method"], "notifications/tools/list_changed");
        assert!(t.receive().await.is_none());
    }

    #[tokio::test]
    async fn a_dropped_receive_keeps_the_half_read_line() {
        use tokio::io::AsyncWriteExt as _;
        let (mut server, client) = tokio::io::duplex(1024);
        let (_w, sink) = tokio::io::duplex(64);
        let mut t = LineTransport::new(client, sink, "t");
        server
            .write_all(br#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","#)
            .await
            .unwrap();
        let first = tokio::time::timeout(std::time::Duration::from_millis(50), t.receive()).await;
        assert!(first.is_err(), "nothing complete yet");
        server.write_all(br#""text":"hi"}]}}"#).await.unwrap();
        server.write_all(b"\n").await.unwrap();
        let m = serde_json::to_value(t.receive().await.unwrap()).unwrap();
        assert_eq!(m["id"], 1, "{m}");
        assert_eq!(m["result"]["content"][0]["text"], "hi", "{m}");
    }

    #[tokio::test]
    async fn an_oversized_request_from_the_server_fails_nothing() {
        let input = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":5,\"method\":\"x\",\"params\":{{\"a\":\"{}\"}}}}\n\
             {{\"jsonrpc\":\"2.0\",\"method\":\"notifications/tools/list_changed\"}}\n",
            "x".repeat(200)
        );
        let (_w, sink) = tokio::io::duplex(64);
        let mut t = LineTransport::new(input.as_bytes(), sink, "t").with_max(100);
        let next = serde_json::to_value(t.receive().await.unwrap()).unwrap();
        assert_eq!(next["method"], "notifications/tools/list_changed", "{next}");
    }
}
