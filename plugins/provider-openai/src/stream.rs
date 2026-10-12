//! Chat-completions SSE chunks → [`ModelEvent`]s.
//!
//! Text arrives in `choices[0].delta.content`. Tool calls arrive in
//! `choices[0].delta.tool_calls`, keyed by `index`: the id and name come first, then the
//! arguments stream in pieces. Usage arrives in a final chunk with no choices (requested
//! with `stream_options.include_usage`), then `data: [DONE]`.

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

use crate::errors::{Failure, from_stream_error};

/// A stream of SSE `data:` payloads; errors are transport errors, already described.
pub type SseStream = BoxStream<'static, Result<String, String>>;

/// Parses a byte stream (an HTTP body) into SSE data payloads.
pub fn sse_data<S, B, E>(bytes: S) -> SseStream
where
    S: Stream<Item = Result<B, E>> + Send + 'static,
    B: AsRef<[u8]> + Send + 'static,
    E: std::fmt::Display + Send + 'static,
{
    bytes
        .eventsource()
        .map(|r| {
            r.map(|e| e.data)
                .map_err(|e| format!("stream read failed: {e}"))
        })
        .boxed()
}

#[derive(Debug, Default)]
struct PendingCall {
    id: String,
    name: String,
    arguments: String,
    announced: bool,
}

/// Builds the response from stream chunks.
#[derive(Debug, Default)]
pub struct Assembler {
    text: String,
    reasoning: String,
    calls: BTreeMap<u64, PendingCall>,
    finish_reason: Option<String>,
    usage: Usage,
    content_started: bool,
    finished: bool,
}

impl Assembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether any output has arrived.
    pub fn content_started(&self) -> bool {
        self.content_started
    }

    /// Whether the response is complete (`[DONE]` seen).
    pub fn finished(&self) -> bool {
        self.finished
    }

    /// Whether a finish reason arrived (enough to complete if `[DONE]` never comes).
    pub fn has_finish_reason(&self) -> bool {
        self.finish_reason.is_some()
    }

    /// Handles one `data:` payload and returns the model events it produces.
    pub fn handle(&mut self, data: &str) -> Result<Vec<ModelEvent>, Failure> {
        let data = data.trim();
        if data.is_empty() {
            return Ok(Vec::new());
        }
        if data == "[DONE]" {
            return Ok(self.finish());
        }
        let chunk: Value = serde_json::from_str(data).map_err(|e| {
            Failure::fatal(ModelError::Other(anyhow::anyhow!(
                "malformed stream chunk: {e}"
            )))
        })?;
        if chunk.get("error").is_some() || chunk["object"] == "error" {
            return Err(from_stream_error(&chunk));
        }
        if let Some(usage) = chunk.get("usage").filter(|u| u.is_object()) {
            self.merge_usage(usage);
        }
        let mut out = Vec::new();
        let Some(choice) = chunk["choices"].get(0) else {
            return Ok(out);
        };
        let delta = &choice["delta"];
        if let Some(t) = delta["content"].as_str().filter(|t| !t.is_empty()) {
            self.content_started = true;
            self.text.push_str(t);
            out.push(ModelEvent::TextDelta(t.to_string()));
        }
        // vLLM/SGLang/DeepSeek use `reasoning_content`; OpenRouter uses `reasoning`.
        let reasoning = delta["reasoning_content"]
            .as_str()
            .or_else(|| delta["reasoning"].as_str());
        if let Some(t) = reasoning.filter(|t| !t.is_empty()) {
            self.content_started = true;
            self.reasoning.push_str(t);
            out.push(ModelEvent::ThinkingDelta(t.to_string()));
        }
        if let Some(calls) = delta["tool_calls"].as_array() {
            self.content_started = true;
            for (pos, tc) in calls.iter().enumerate() {
                let index = tc["index"].as_u64().unwrap_or(pos as u64);
                let entry = self.calls.entry(index).or_default();
                if let Some(id) = tc["id"].as_str().filter(|s| !s.is_empty()) {
                    entry.id = id.to_string();
                }
                let f = &tc["function"];
                if let Some(name) = f["name"].as_str().filter(|s| !s.is_empty()) {
                    entry.name.push_str(name);
                }
                let args = f["arguments"].as_str().unwrap_or("");
                entry.arguments.push_str(args);
                if !entry.announced && !entry.name.is_empty() {
                    entry.announced = true;
                    if entry.id.is_empty() {
                        entry.id = format!("call_{index}");
                    }
                    out.push(ModelEvent::ToolUseStart {
                        id: entry.id.clone(),
                        name: entry.name.clone(),
                    });
                }
                if entry.announced && !args.is_empty() {
                    out.push(ModelEvent::ToolInputDelta {
                        id: entry.id.clone(),
                        len: args.len(),
                    });
                }
            }
        }
        if let Some(reason) = choice["finish_reason"].as_str() {
            self.finish_reason = Some(reason.to_string());
        }
        Ok(out)
    }

    fn merge_usage(&mut self, usage: &Value) {
        let prompt = usage["prompt_tokens"].as_u64().unwrap_or(0);
        let cached = usage["prompt_tokens_details"]["cached_tokens"]
            .as_u64()
            .unwrap_or(0)
            .min(prompt);
        self.usage = Usage {
            input_tokens: prompt - cached,
            output_tokens: usage["completion_tokens"].as_u64().unwrap_or(0),
            cache_read_tokens: cached,
            cache_write_tokens: 0,
        };
    }

    /// Marks the response complete and returns the `Completed` event.
    pub fn finish(&mut self) -> Vec<ModelEvent> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        vec![ModelEvent::Completed(self.response())]
    }

    /// The assembled response so far.
    pub fn response(&self) -> ModelResponse {
        let mut content = Vec::new();
        if !self.reasoning.is_empty() {
            content.push(ContentBlock::Thinking {
                thinking: self.reasoning.clone(),
                signature: None,
            });
        }
        if !self.text.is_empty() {
            content.push(ContentBlock::text(self.text.clone()));
        }
        for (index, call) in &self.calls {
            let id = if call.id.is_empty() {
                format!("call_{index}")
            } else {
                call.id.clone()
            };
            content.push(ContentBlock::ToolUse {
                id,
                name: call.name.clone(),
                input: parse_arguments(&call.arguments),
            });
        }
        let stop_reason = match self.finish_reason.as_deref() {
            Some("length") => StopReason::MaxTokens,
            // Some servers report "stop" even when they called tools.
            _ if !self.calls.is_empty() => StopReason::ToolUse,
            Some("stop") | None => StopReason::EndTurn,
            Some("tool_calls") | Some("function_call") => StopReason::ToolUse,
            Some(other) => StopReason::Other(other.to_string()),
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

/// Parses streamed tool-call arguments. Empty → `{}`; anything that isn't a JSON object
/// is kept as `{"_invalid_json": "<raw>"}` so the tool reports a validation error.
pub fn parse_arguments(raw: &str) -> Value {
    if raw.trim().is_empty() {
        return json!({});
    }
    match serde_json::from_str::<Value>(raw) {
        Ok(v @ Value::Object(_)) => v,
        _ => json!({ "_invalid_json": raw }),
    }
}

/// Reads chunks until output starts (so errors before any output can be retried), then
/// returns the stream of model events.
pub async fn start(mut sse: SseStream, cancel: CancellationToken) -> Result<ModelStream, Failure> {
    let mut asm = Assembler::new();
    let mut pending = VecDeque::new();
    while !asm.content_started() && !asm.finished() {
        let next = tokio::select! {
            _ = cancel.cancelled() => return Err(Failure::fatal(ModelError::Cancelled)),
            next = sse.next() => next,
        };
        match next {
            None if asm.has_finish_reason() => pending.extend(asm.finish()),
            None => {
                return Err(Failure::retry(
                    ModelError::Unavailable("stream closed before the response started".into()),
                    None,
                ));
            }
            Some(Err(e)) => return Err(Failure::retry(ModelError::Unavailable(e), None)),
            Some(Ok(data)) => pending.extend(asm.handle(&data)?),
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
                // Some servers close without `[DONE]`; a finish reason is enough.
                None if st.asm.has_finish_reason() => {
                    let evs = st.asm.finish();
                    st.pending.extend(evs);
                }
                None => {
                    st.done = true;
                    return Some((
                        Err(ModelError::Unavailable(
                            "the response stream ended before it finished".into(),
                        )),
                        st,
                    ));
                }
                Some(Err(e)) => {
                    st.done = true;
                    return Some((Err(ModelError::Unavailable(e)), st));
                }
                Some(Ok(data)) => match st.asm.handle(&data) {
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

    fn sse(chunks: &[Value], done: bool) -> SseStream {
        let mut text: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
        if done {
            text.push_str("data: [DONE]\n\n");
        }
        let bytes: Vec<Result<Vec<u8>, std::io::Error>> =
            text.as_bytes().chunks(5).map(|c| Ok(c.to_vec())).collect();
        sse_data(futures::stream::iter(bytes))
    }

    async fn collect(s: SseStream) -> (Vec<ModelEvent>, Option<ModelError>) {
        let mut stream = match start(s, CancellationToken::new()).await {
            Ok(s) => s,
            Err(f) => return (Vec::new(), Some(f.error)),
        };
        let mut evs = Vec::new();
        while let Some(item) = stream.next().await {
            match item {
                Ok(ev) => evs.push(ev),
                Err(e) => return (evs, Some(e)),
            }
        }
        (evs, None)
    }

    fn completed(evs: &[ModelEvent]) -> &ModelResponse {
        let n = evs
            .iter()
            .filter(|e| matches!(e, ModelEvent::Completed(_)))
            .count();
        assert_eq!(n, 1, "{evs:?}");
        match evs.last() {
            Some(ModelEvent::Completed(r)) => r,
            other => panic!("{other:?}"),
        }
    }

    fn delta(d: Value) -> Value {
        json!({"id":"c","object":"chat.completion.chunk","choices":[{"index":0,"delta":d,"finish_reason":null}]})
    }

    fn finish(reason: &str) -> Value {
        json!({"id":"c","object":"chat.completion.chunk","choices":[{"index":0,"delta":{},"finish_reason":reason}]})
    }

    fn usage() -> Value {
        json!({"id":"c","object":"chat.completion.chunk","choices":[],"usage":{"prompt_tokens":100,"completion_tokens":20,"total_tokens":120,"prompt_tokens_details":{"cached_tokens":64}}})
    }

    #[tokio::test]
    async fn text_and_usage() {
        let s = sse(
            &[
                delta(json!({"role":"assistant","content":""})),
                delta(json!({"content":"Hel"})),
                delta(json!({"content":"lo"})),
                finish("stop"),
                usage(),
            ],
            true,
        );
        let (evs, err) = collect(s).await;
        assert!(err.is_none(), "{err:?}");
        assert_eq!(evs[0], ModelEvent::TextDelta("Hel".into()));
        let r = completed(&evs);
        assert_eq!(r.message.text(), "Hello");
        assert_eq!(r.stop_reason, StopReason::EndTurn);
        assert_eq!(
            r.usage,
            Usage {
                input_tokens: 36,
                output_tokens: 20,
                cache_read_tokens: 64,
                cache_write_tokens: 0
            }
        );
    }

    #[tokio::test]
    async fn tool_calls_assemble_by_index() {
        let s = sse(
            &[
                delta(json!({"role":"assistant","content":null,"tool_calls":[
                    {"index":0,"id":"call_a","type":"function","function":{"name":"read_file","arguments":""}}
                ]})),
                delta(json!({"tool_calls":[{"index":0,"function":{"arguments":"{\"pa"}}]})),
                delta(json!({"tool_calls":[
                    {"index":1,"id":"call_b","type":"function","function":{"name":"grep_text","arguments":"{\"query\""}}
                ]})),
                delta(
                    json!({"tool_calls":[{"index":0,"function":{"arguments":"th\":\"a.rs\"}"}}]}),
                ),
                delta(json!({"tool_calls":[{"index":1,"function":{"arguments":":\"fn\"}"}}]})),
                delta(json!({"tool_calls":[
                    {"index":2,"id":"call_c","type":"function","function":{"name":"list_files","arguments":""}}
                ]})),
                finish("tool_calls"),
                usage(),
            ],
            true,
        );
        let (evs, err) = collect(s).await;
        assert!(err.is_none(), "{err:?}");
        assert_eq!(
            evs[0],
            ModelEvent::ToolUseStart {
                id: "call_a".into(),
                name: "read_file".into()
            }
        );
        let r = completed(&evs);
        assert_eq!(r.stop_reason, StopReason::ToolUse);
        let calls = r.message.tool_calls();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0].input, json!({"path": "a.rs"}));
        assert_eq!(calls[1].id, "call_b");
        assert_eq!(calls[1].input, json!({"query": "fn"}));
        assert_eq!(calls[2].input, json!({}));
    }

    #[tokio::test]
    async fn reasoning_and_length() {
        let s = sse(
            &[
                delta(json!({"reasoning_content":"think"})),
                delta(json!({"content":"ans"})),
                finish("length"),
            ],
            false,
        );
        let (evs, err) = collect(s).await;
        assert!(err.is_none(), "{err:?}");
        assert_eq!(evs[0], ModelEvent::ThinkingDelta("think".into()));
        let r = completed(&evs);
        assert_eq!(r.stop_reason, StopReason::MaxTokens);
        assert!(matches!(
            r.message.content[0],
            ContentBlock::Thinking { .. }
        ));
    }

    #[tokio::test]
    async fn errors_in_stream() {
        let s = sse(
            &[
                json!({"error":{"message":"This model's maximum context length is 8192 tokens","type":"invalid_request_error","code":"context_length_exceeded"}}),
            ],
            false,
        );
        let (_, err) = collect(s).await;
        assert!(
            matches!(err, Some(ModelError::ContextOverflow(_))),
            "{err:?}"
        );

        let s = sse(
            &[
                delta(json!({"content":"partial"})),
                json!({"error":{"message":"upstream overloaded","type":"server_error"}}),
            ],
            false,
        );
        let (evs, err) = collect(s).await;
        assert_eq!(evs, vec![ModelEvent::TextDelta("partial".into())]);
        assert!(matches!(err, Some(ModelError::Unavailable(_))), "{err:?}");

        // Truncated stream with no finish reason.
        let (_, err) = collect(sse(&[delta(json!({"content":"x"}))], false)).await;
        assert!(matches!(err, Some(ModelError::Unavailable(_))));
    }

    #[tokio::test]
    async fn cancellation() {
        let cancel = CancellationToken::new();
        let head: Result<Vec<u8>, std::io::Error> =
            Ok(format!("data: {}\n\n", delta(json!({"content":"hi"}))).into_bytes());
        let bytes = futures::stream::iter(vec![head]).chain(futures::stream::pending());
        let mut s = start(sse_data(bytes), cancel.clone()).await.unwrap();
        assert_eq!(
            s.next().await.unwrap().unwrap(),
            ModelEvent::TextDelta("hi".into())
        );
        cancel.cancel();
        assert!(matches!(s.next().await, Some(Err(ModelError::Cancelled))));
        assert!(s.next().await.is_none());
    }

    #[test]
    fn arguments() {
        assert_eq!(parse_arguments(""), json!({}));
        assert_eq!(parse_arguments("{\"a\":1}"), json!({"a":1}));
        assert_eq!(parse_arguments("{"), json!({"_invalid_json":"{"}));
    }
}
