//! SSE stream → [`ModelEvent`]s.
//!
//! [`Assembler`] consumes Messages API stream events one at a time and builds the final
//! assistant message: text, thinking (with its signature), redacted thinking and tool_use
//! blocks (whose `input_json_delta` fragments are joined and parsed at block stop).

use std::collections::{BTreeMap, VecDeque};

use ch_kernel::reexports::CancellationToken;
use ch_kernel::{
    ContentBlock, Message, ModelError, ModelEvent, ModelResponse, ModelStream, Role, StopReason,
    Usage,
};
use eventsource_stream::Eventsource;
use futures::stream::BoxStream;
use futures::{Stream, StreamExt};
use serde_json::{Value, json};

use crate::errors::{Failure, from_error_type};

/// One server-sent event: its `event:` name and `data:` payload.
#[derive(Debug, Clone, PartialEq)]
pub struct SseEvent {
    pub event: String,
    pub data: String,
}

/// A stream of SSE events; errors are transport errors, already described.
pub type SseStream = BoxStream<'static, Result<SseEvent, String>>;

/// Parses a byte stream (an HTTP body) into SSE events.
pub fn sse_events<S, B, E>(bytes: S) -> SseStream
where
    S: Stream<Item = Result<B, E>> + Send + 'static,
    B: AsRef<[u8]> + Send + 'static,
    E: std::fmt::Display + Send + 'static,
{
    bytes
        .eventsource()
        .map(|r| {
            r.map(|e| SseEvent {
                event: e.event,
                data: e.data,
            })
            .map_err(|e| format!("stream read failed: {e}"))
        })
        .boxed()
}

#[derive(Debug)]
enum Block {
    Text(String),
    Thinking {
        text: String,
        signature: String,
    },
    Redacted(String),
    ToolUse {
        id: String,
        name: String,
        json: String,
    },
    /// Server-side refusal fallback switch point.
    Fallback,
    /// A block type this provider doesn't map (server tools, citations, ...).
    Ignored,
}

/// Builds the response from stream events.
#[derive(Debug, Default)]
pub struct Assembler {
    open: BTreeMap<usize, Block>,
    closed: BTreeMap<usize, Block>,
    usage: Usage,
    stop_reason: Option<String>,
    content_started: bool,
    finished: bool,
}

impl Assembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether any content block has started (output has been produced).
    pub fn content_started(&self) -> bool {
        self.content_started
    }

    /// Whether `message_stop` was seen.
    pub fn finished(&self) -> bool {
        self.finished
    }

    /// Handles one SSE event and returns the model events it produces.
    pub fn handle(&mut self, ev: &SseEvent) -> Result<Vec<ModelEvent>, Failure> {
        if ev.data.trim().is_empty() {
            return Ok(Vec::new());
        }
        let data: Value = serde_json::from_str(&ev.data).map_err(|e| {
            Failure::fatal(ModelError::Other(anyhow::anyhow!(
                "malformed stream event `{}`: {e}",
                ev.event
            )))
        })?;
        let kind = data["type"].as_str().unwrap_or(ev.event.as_str());
        let mut out = Vec::new();
        match kind {
            "message_start" => self.merge_usage(&data["message"]["usage"]),
            "content_block_start" => {
                self.content_started = true;
                let index = index_of(&data);
                let cb = &data["content_block"];
                let block = match cb["type"].as_str().unwrap_or("") {
                    "text" => {
                        let text = cb["text"].as_str().unwrap_or("").to_string();
                        if !text.is_empty() {
                            out.push(ModelEvent::TextDelta(text.clone()));
                        }
                        Block::Text(text)
                    }
                    "thinking" => {
                        let text = cb["thinking"].as_str().unwrap_or("").to_string();
                        if !text.is_empty() {
                            out.push(ModelEvent::ThinkingDelta(text.clone()));
                        }
                        Block::Thinking {
                            text,
                            signature: cb["signature"].as_str().unwrap_or("").to_string(),
                        }
                    }
                    "redacted_thinking" => {
                        Block::Redacted(cb["data"].as_str().unwrap_or("").to_string())
                    }
                    "tool_use" => {
                        let id = cb["id"].as_str().unwrap_or("").to_string();
                        let name = cb["name"].as_str().unwrap_or("").to_string();
                        out.push(ModelEvent::ToolUseStart {
                            id: id.clone(),
                            name: name.clone(),
                        });
                        // Input normally arrives as deltas; keep a non-empty initial value.
                        let json = match &cb["input"] {
                            Value::Object(m) if !m.is_empty() => cb["input"].to_string(),
                            _ => String::new(),
                        };
                        Block::ToolUse { id, name, json }
                    }
                    "fallback" => Block::Fallback,
                    _ => Block::Ignored,
                };
                self.open.insert(index, block);
            }
            "content_block_delta" => {
                let index = index_of(&data);
                let delta = &data["delta"];
                let text = |key: &str| delta[key].as_str().unwrap_or("").to_string();
                match (self.open.get_mut(&index), delta["type"].as_str()) {
                    (Some(Block::Text(buf)), Some("text_delta")) => {
                        let t = text("text");
                        buf.push_str(&t);
                        if !t.is_empty() {
                            out.push(ModelEvent::TextDelta(t));
                        }
                    }
                    (Some(Block::Thinking { text: buf, .. }), Some("thinking_delta")) => {
                        let t = text("thinking");
                        buf.push_str(&t);
                        if !t.is_empty() {
                            out.push(ModelEvent::ThinkingDelta(t));
                        }
                    }
                    (Some(Block::Thinking { signature, .. }), Some("signature_delta")) => {
                        signature.push_str(&text("signature"));
                    }
                    (Some(Block::ToolUse { id, json, .. }), Some("input_json_delta")) => {
                        let t = text("partial_json");
                        json.push_str(&t);
                        if !t.is_empty() {
                            out.push(ModelEvent::ToolInputDelta {
                                id: id.clone(),
                                len: t.len(),
                            });
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                let index = index_of(&data);
                if let Some(block) = self.open.remove(&index) {
                    self.closed.insert(index, block);
                }
            }
            "message_delta" => {
                if let Some(reason) = data["delta"]["stop_reason"].as_str() {
                    self.stop_reason = Some(reason.to_string());
                }
                self.merge_usage(&data["usage"]);
            }
            "message_stop" => {
                self.finished = true;
                out.push(ModelEvent::Completed(self.response()));
            }
            "error" => {
                let err = &data["error"];
                return Err(from_error_type(
                    err["type"].as_str().unwrap_or(""),
                    err["message"].as_str().unwrap_or("unknown stream error"),
                ));
            }
            // "ping" and event types added later.
            _ => {}
        }
        Ok(out)
    }

    fn merge_usage(&mut self, usage: &Value) {
        let get = |k: &str| usage.get(k).and_then(Value::as_u64);
        if let Some(n) = get("input_tokens") {
            self.usage.input_tokens = n;
        }
        if let Some(n) = get("output_tokens") {
            self.usage.output_tokens = n;
        }
        if let Some(n) = get("cache_read_input_tokens") {
            self.usage.cache_read_tokens = n;
        }
        if let Some(n) = get("cache_creation_input_tokens") {
            self.usage.cache_write_tokens = n;
        }
    }

    /// The assembled response from the blocks closed so far.
    pub fn response(&self) -> ModelResponse {
        // After a mid-output refusal fallback, thinking and tool_use blocks before the
        // last fallback marker must not be echoed back.
        let boundary = self
            .closed
            .iter()
            .filter(|(_, b)| matches!(b, Block::Fallback))
            .map(|(i, _)| *i)
            .max();
        let mut content = Vec::new();
        for (index, block) in &self.closed {
            let before_boundary = boundary.is_some_and(|b| *index < b);
            match block {
                Block::Text(text) if !text.is_empty() => {
                    content.push(ContentBlock::Text { text: text.clone() })
                }
                Block::Thinking { text, signature } if !before_boundary => {
                    if text.is_empty() && signature.is_empty() {
                        continue;
                    }
                    content.push(ContentBlock::Thinking {
                        thinking: text.clone(),
                        signature: (!signature.is_empty()).then(|| signature.clone()),
                    });
                }
                Block::Redacted(data) if !before_boundary => {
                    content.push(ContentBlock::RedactedThinking { data: data.clone() })
                }
                Block::ToolUse { id, name, json } if !before_boundary => {
                    content.push(ContentBlock::ToolUse {
                        id: id.clone(),
                        name: name.clone(),
                        input: parse_tool_input(json),
                    })
                }
                _ => {}
            }
        }
        let has_tool_use = content
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolUse { .. }));
        let stop_reason = match self.stop_reason.as_deref() {
            Some("end_turn") => StopReason::EndTurn,
            Some("tool_use") => StopReason::ToolUse,
            Some("max_tokens") => StopReason::MaxTokens,
            Some("stop_sequence") => StopReason::StopSequence,
            Some("refusal") => StopReason::Refusal,
            Some("model_context_window_exceeded") => StopReason::ContextWindowExceeded,
            Some(other) => StopReason::Other(other.to_string()),
            None if has_tool_use => StopReason::ToolUse,
            None => StopReason::EndTurn,
        };
        ModelResponse {
            message: Message {
                role: Role::Assistant,
                content,
            },
            stop_reason,
            usage: self.usage,
        }
    }
}

fn index_of(data: &Value) -> usize {
    data["index"].as_u64().unwrap_or(0) as usize
}

/// Parses accumulated tool input JSON. Empty input is `{}`. Input that is not a JSON
/// object (truncated by `max_tokens`, or malformed) is kept as
/// `{"_invalid_json": "<raw>"}` so the tool rejects it with a validation error the model
/// can see, and the history stays valid for the API.
pub fn parse_tool_input(raw: &str) -> Value {
    if raw.trim().is_empty() {
        return json!({});
    }
    match serde_json::from_str::<Value>(raw) {
        Ok(v @ Value::Object(_)) => v,
        _ => json!({ "_invalid_json": raw }),
    }
}

/// Reads events from `sse` until the first content block starts (or the message ends),
/// so errors that arrive before any output can still be retried. Then returns the stream
/// of model events for the rest of the response.
pub async fn start(mut sse: SseStream, cancel: CancellationToken) -> Result<ModelStream, Failure> {
    let mut asm = Assembler::new();
    let mut pending = VecDeque::new();
    while !asm.content_started() && !asm.finished() {
        let next = tokio::select! {
            _ = cancel.cancelled() => return Err(Failure::fatal(ModelError::Cancelled)),
            next = sse.next() => next,
        };
        match next {
            None => {
                return Err(Failure::retry(
                    ModelError::Unavailable("stream closed before the response started".into()),
                    None,
                ));
            }
            Some(Err(e)) => return Err(Failure::retry(ModelError::Unavailable(e), None)),
            Some(Ok(ev)) => pending.extend(asm.handle(&ev)?),
        }
    }
    Ok(into_model_stream(sse, asm, pending, cancel))
}

struct State {
    sse: SseStream,
    asm: Assembler,
    pending: VecDeque<ModelEvent>,
    cancel: CancellationToken,
    done: bool,
}

fn into_model_stream(
    sse: SseStream,
    asm: Assembler,
    pending: VecDeque<ModelEvent>,
    cancel: CancellationToken,
) -> ModelStream {
    let state = State {
        sse,
        asm,
        pending,
        cancel,
        done: false,
    };
    futures::stream::unfold(state, |mut st| async move {
        loop {
            if let Some(ev) = st.pending.pop_front() {
                return Some((Ok(ev), st));
            }
            if st.done || st.asm.finished() {
                return None;
            }
            let next = tokio::select! {
                _ = st.cancel.cancelled() => {
                    st.done = true;
                    return Some((Err(ModelError::Cancelled), st));
                }
                next = st.sse.next() => next,
            };
            match next {
                None => {
                    st.done = true;
                    return Some((
                        Err(ModelError::Unavailable(
                            "the response stream ended before message_stop".into(),
                        )),
                        st,
                    ));
                }
                Some(Err(e)) => {
                    st.done = true;
                    return Some((Err(ModelError::Unavailable(e)), st));
                }
                Some(Ok(ev)) => match st.asm.handle(&ev) {
                    Ok(evs) => st.pending.extend(evs),
                    Err(f) => {
                        st.done = true;
                        return Some((Err(f.error), st));
                    }
                },
            }
        }
    })
    .boxed()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feeds hand-written SSE text through the real parser, split into small chunks so
    /// events straddle chunk boundaries.
    fn sse(text: &str) -> SseStream {
        let bytes: Vec<Result<Vec<u8>, std::io::Error>> =
            text.as_bytes().chunks(7).map(|c| Ok(c.to_vec())).collect();
        sse_events(futures::stream::iter(bytes))
    }

    fn event(name: &str, data: Value) -> String {
        format!("event: {name}\ndata: {data}\n\n")
    }

    async fn collect(text: &str) -> (Vec<ModelEvent>, Option<ModelError>) {
        let stream = match start(sse(text), CancellationToken::new()).await {
            Ok(s) => s,
            Err(f) => return (Vec::new(), Some(f.error)),
        };
        let mut evs = Vec::new();
        let mut stream = stream;
        while let Some(item) = stream.next().await {
            match item {
                Ok(ev) => evs.push(ev),
                Err(e) => return (evs, Some(e)),
            }
        }
        (evs, None)
    }

    fn completed(evs: &[ModelEvent]) -> &ModelResponse {
        let done: Vec<_> = evs
            .iter()
            .filter_map(|e| match e {
                ModelEvent::Completed(r) => Some(r),
                _ => None,
            })
            .collect();
        assert_eq!(done.len(), 1, "exactly one Completed: {evs:?}");
        assert!(matches!(evs.last(), Some(ModelEvent::Completed(_))));
        done[0]
    }

    fn message_start() -> String {
        event(
            "message_start",
            json!({"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"claude-opus-5-5","content":[],"stop_reason":null,"usage":{"input_tokens":120,"output_tokens":1,"cache_creation_input_tokens":50,"cache_read_input_tokens":2000}}}),
        )
    }

    #[tokio::test]
    async fn text_stream_with_usage() {
        let text = [
            message_start(),
            event("ping", json!({"type":"ping"})),
            event(
                "content_block_start",
                json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            ),
            event(
                "content_block_delta",
                json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}),
            ),
            event(
                "content_block_delta",
                json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":", world"}}),
            ),
            event(
                "content_block_stop",
                json!({"type":"content_block_stop","index":0}),
            ),
            event(
                "message_delta",
                json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":12}}),
            ),
            event("message_stop", json!({"type":"message_stop"})),
        ]
        .concat();
        let (evs, err) = collect(&text).await;
        assert!(err.is_none(), "{err:?}");
        assert_eq!(evs[0], ModelEvent::TextDelta("Hello".into()));
        assert_eq!(evs[1], ModelEvent::TextDelta(", world".into()));
        let resp = completed(&evs);
        assert_eq!(resp.message.text(), "Hello, world");
        assert_eq!(resp.stop_reason, StopReason::EndTurn);
        assert_eq!(
            resp.usage,
            Usage {
                input_tokens: 120,
                output_tokens: 12,
                cache_read_tokens: 2000,
                cache_write_tokens: 50,
            }
        );
    }

    #[tokio::test]
    async fn thinking_with_signature_and_multiple_tool_calls() {
        let text = [
            message_start(),
            event(
                "content_block_start",
                json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
            ),
            event(
                "content_block_delta",
                json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Let me "}}),
            ),
            event(
                "content_block_delta",
                json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"look."}}),
            ),
            event(
                "content_block_delta",
                json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"EqQBCgIYAhIM"}}),
            ),
            event(
                "content_block_stop",
                json!({"type":"content_block_stop","index":0}),
            ),
            event(
                "content_block_start",
                json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_a","name":"read_file","input":{}}}),
            ),
            event(
                "content_block_delta",
                json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":""}}),
            ),
            event(
                "content_block_delta",
                json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"pa"}}),
            ),
            event(
                "content_block_delta",
                json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"th\": \"src/ma"}}),
            ),
            event(
                "content_block_delta",
                json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"in.rs\"}"}}),
            ),
            event(
                "content_block_stop",
                json!({"type":"content_block_stop","index":1}),
            ),
            event(
                "content_block_start",
                json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_b","name":"list_files","input":{}}}),
            ),
            event(
                "content_block_stop",
                json!({"type":"content_block_stop","index":2}),
            ),
            event(
                "message_delta",
                json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":80}}),
            ),
            event("message_stop", json!({"type":"message_stop"})),
        ]
        .concat();
        let (evs, err) = collect(&text).await;
        assert!(err.is_none(), "{err:?}");
        assert_eq!(evs[0], ModelEvent::ThinkingDelta("Let me ".into()));
        assert_eq!(evs[1], ModelEvent::ThinkingDelta("look.".into()));
        assert_eq!(
            evs[2],
            ModelEvent::ToolUseStart {
                id: "toolu_a".into(),
                name: "read_file".into()
            }
        );
        let resp = completed(&evs);
        assert_eq!(resp.stop_reason, StopReason::ToolUse);
        assert_eq!(
            resp.message.content[0],
            ContentBlock::Thinking {
                thinking: "Let me look.".into(),
                signature: Some("EqQBCgIYAhIM".into()),
            }
        );
        let calls = resp.message.tool_calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].input, json!({"path": "src/main.rs"}));
        assert_eq!(calls[1].id, "toolu_b");
        assert_eq!(calls[1].input, json!({}));
    }

    #[tokio::test]
    async fn redacted_thinking_is_kept() {
        let text = [
            message_start(),
            event(
                "content_block_start",
                json!({"type":"content_block_start","index":0,"content_block":{"type":"redacted_thinking","data":"opaque=="}}),
            ),
            event(
                "content_block_stop",
                json!({"type":"content_block_stop","index":0}),
            ),
            event(
                "message_delta",
                json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}),
            ),
            event("message_stop", json!({"type":"message_stop"})),
        ]
        .concat();
        let (evs, _) = collect(&text).await;
        let resp = completed(&evs);
        assert_eq!(
            resp.message.content,
            vec![ContentBlock::RedactedThinking {
                data: "opaque==".into(),
            }]
        );
    }

    #[tokio::test]
    async fn error_before_content_is_retryable() {
        let text = [
            message_start(),
            event(
                "error",
                json!({"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}),
            ),
        ]
        .concat();
        match start(sse(&text), CancellationToken::new()).await {
            Err(f) => {
                assert!(f.retryable);
                assert!(matches!(f.error, ModelError::Unavailable(_)));
            }
            Ok(_) => panic!("expected failure"),
        }
    }

    #[tokio::test]
    async fn error_mid_stream_is_mapped() {
        let base = [
            message_start(),
            event(
                "content_block_start",
                json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            ),
            event(
                "content_block_delta",
                json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"partial"}}),
            ),
        ]
        .concat();
        let overloaded = base.clone()
            + &event(
                "error",
                json!({"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}),
            );
        let (evs, err) = collect(&overloaded).await;
        assert_eq!(evs, vec![ModelEvent::TextDelta("partial".into())]);
        assert!(matches!(err, Some(ModelError::Unavailable(_))));

        let overflow = base.clone()
            + &event(
                "error",
                json!({"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 1000500 tokens > 1000000 maximum"}}),
            );
        let (_, err) = collect(&overflow).await;
        assert!(matches!(err, Some(ModelError::ContextOverflow(_))));

        // A stream that just stops is an error, not a silent success.
        let (_, err) = collect(&base).await;
        assert!(matches!(err, Some(ModelError::Unavailable(_))));
    }

    #[tokio::test]
    async fn fallback_drops_blocks_before_the_boundary() {
        let text = [
            message_start(),
            event(
                "content_block_start",
                json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
            ),
            event(
                "content_block_delta",
                json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"s1"}}),
            ),
            event(
                "content_block_stop",
                json!({"type":"content_block_stop","index":0}),
            ),
            event(
                "content_block_start",
                json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":"Sure"}}),
            ),
            event(
                "content_block_stop",
                json!({"type":"content_block_stop","index":1}),
            ),
            event(
                "content_block_start",
                json!({"type":"content_block_start","index":2,"content_block":{"type":"fallback","from":{"model":"claude-opus-5-5"},"to":{"model":"claude-opus-4-8"}}}),
            ),
            event(
                "content_block_stop",
                json!({"type":"content_block_stop","index":2}),
            ),
            event(
                "content_block_start",
                json!({"type":"content_block_start","index":3,"content_block":{"type":"text","text":" thing."}}),
            ),
            event(
                "content_block_stop",
                json!({"type":"content_block_stop","index":3}),
            ),
            event(
                "message_delta",
                json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}),
            ),
            event("message_stop", json!({"type":"message_stop"})),
        ]
        .concat();
        let (evs, _) = collect(&text).await;
        let resp = completed(&evs);
        assert_eq!(resp.message.text(), "Sure thing.");
        assert!(
            !resp
                .message
                .content
                .iter()
                .any(|b| matches!(b, ContentBlock::Thinking { .. }))
        );
    }

    #[tokio::test]
    async fn cancellation_ends_the_stream() {
        let cancel = CancellationToken::new();
        let head = [
            message_start(),
            event(
                "content_block_start",
                json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"hi"}}),
            ),
        ]
        .concat();
        // The body never finishes: head, then pending forever.
        let first: Result<Vec<u8>, std::io::Error> = Ok(head.into_bytes());
        let bytes = futures::stream::iter(vec![first]).chain(futures::stream::pending());
        let mut stream = start(sse_events(bytes), cancel.clone()).await.unwrap();
        assert_eq!(
            stream.next().await.unwrap().unwrap(),
            ModelEvent::TextDelta("hi".into())
        );
        cancel.cancel();
        assert!(matches!(
            stream.next().await,
            Some(Err(ModelError::Cancelled))
        ));
        assert!(stream.next().await.is_none());
    }

    #[test]
    fn tool_input_parsing() {
        assert_eq!(parse_tool_input(""), json!({}));
        assert_eq!(parse_tool_input("  "), json!({}));
        assert_eq!(parse_tool_input("{\"a\":1}"), json!({"a": 1}));
        assert_eq!(
            parse_tool_input("{\"a\":"),
            json!({"_invalid_json": "{\"a\":"})
        );
        assert_eq!(parse_tool_input("[1]"), json!({"_invalid_json": "[1]"}));
    }
}
