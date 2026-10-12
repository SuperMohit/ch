//! Mapping OpenAI-compatible API errors to [`ModelError`].

use std::time::Duration;

use ch_kernel::ModelError;
use serde_json::Value;

/// A failed attempt, with whether retrying it may help.
#[derive(Debug)]
pub struct Failure {
    pub error: ModelError,
    pub retryable: bool,
    /// Server-requested delay (`retry-after`).
    pub retry_after: Option<Duration>,
}

impl Failure {
    pub fn fatal(error: ModelError) -> Self {
        Self {
            error,
            retryable: false,
            retry_after: None,
        }
    }

    pub fn retry(error: ModelError, retry_after: Option<Duration>) -> Self {
        Self {
            error,
            retryable: true,
            retry_after,
        }
    }
}

/// Whether an error is about the prompt not fitting the model's context window.
pub fn is_context_overflow(code: &str, message: &str) -> bool {
    if code == "context_length_exceeded" {
        return true;
    }
    let m = message.to_ascii_lowercase();
    [
        "maximum context length",
        "context length",
        "context_length_exceeded",
        "context window",
        "prompt is too long",
        "input is too long",
    ]
    .iter()
    .any(|needle| m.contains(needle))
}

/// `(code, type, message)` from an error object. Accepts `{"error": {...}}`,
/// `{"error": "..."}` and vLLM's flat `{"object": "error", "message": ...}`.
pub fn parse_error(v: &Value) -> (String, String, String) {
    let err = v.get("error").unwrap_or(v);
    if let Some(s) = err.as_str() {
        return (String::new(), String::new(), s.to_string());
    }
    let s = |k: &str| match &err[k] {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    };
    (s("code"), s("type"), s("message"))
}

fn parse_error_body(body: &str) -> (String, String, String) {
    match serde_json::from_str::<Value>(body) {
        Ok(v) => {
            let (code, kind, msg) = parse_error(&v);
            let msg = if msg.is_empty() {
                body.trim().to_string()
            } else {
                msg
            };
            (code, kind, msg)
        }
        Err(_) => (String::new(), String::new(), body.trim().to_string()),
    }
}

/// Maps an error object that arrived inside the stream.
pub fn from_stream_error(v: &Value) -> Failure {
    let (code, kind, message) = parse_error(v);
    let message = if message.is_empty() {
        v.to_string()
    } else {
        message
    };
    if is_context_overflow(&code, &message) {
        return Failure::fatal(ModelError::ContextOverflow(message));
    }
    match (code.as_str(), kind.as_str()) {
        ("rate_limit_exceeded", _) | (_, "rate_limit_error") => {
            Failure::retry(ModelError::RateLimited(message), None)
        }
        (_, "server_error") | (_, "overloaded_error") | ("server_error", _) => {
            Failure::retry(ModelError::Unavailable(message), None)
        }
        _ => Failure::fatal(ModelError::InvalidRequest(message)),
    }
}

/// Maps a non-success HTTP response.
pub fn from_status(status: u16, body: &str, retry_after: Option<Duration>) -> Failure {
    let (code, _kind, message) = parse_error_body(body);
    match status {
        401 | 403 => Failure::fatal(ModelError::Auth(format!(
            "HTTP {status}: {message}. Check the API key and that it can use this model"
        ))),
        // Quota exhaustion is a 429 that waiting won't fix.
        429 if code == "insufficient_quota" => Failure::fatal(ModelError::RateLimited(message)),
        429 => Failure::retry(ModelError::RateLimited(message), retry_after),
        408 | 409 | 500..=599 => Failure::retry(
            ModelError::Unavailable(format!("HTTP {status}: {message}")),
            retry_after,
        ),
        413 => Failure::fatal(ModelError::ContextOverflow(message)),
        _ if is_context_overflow(&code, &message) => {
            Failure::fatal(ModelError::ContextOverflow(message))
        }
        404 => Failure::fatal(ModelError::InvalidRequest(format!(
            "HTTP 404: {message}. Check model.id and plugins.provider-openai.base_url"
        ))),
        _ => Failure::fatal(ModelError::InvalidRequest(format!(
            "HTTP {status}: {message}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn maps_statuses() {
        let openai = json!({"error": {"message": "This model's maximum context length is 128000 tokens. However, your messages resulted in 130000 tokens.", "type": "invalid_request_error", "param": "messages", "code": "context_length_exceeded"}}).to_string();
        assert!(matches!(
            from_status(400, &openai, None).error,
            ModelError::ContextOverflow(_)
        ));
        let vllm = json!({"object": "error", "message": "This model's maximum context length is 4096 tokens. However, you requested 5000 tokens.", "type": "BadRequestError", "code": 400}).to_string();
        assert!(matches!(
            from_status(400, &vllm, None).error,
            ModelError::ContextOverflow(_)
        ));
        assert!(matches!(
            from_status(401, "{}", None).error,
            ModelError::Auth(_)
        ));
        assert!(from_status(429, "{}", None).retryable);
        let quota =
            json!({"error": {"message": "quota", "code": "insufficient_quota"}}).to_string();
        assert!(!from_status(429, &quota, None).retryable);
        assert!(from_status(503, "busy", None).retryable);
        match from_status(400, r#"{"error":{"message":"bad tool schema"}}"#, None).error {
            ModelError::InvalidRequest(m) => assert!(m.contains("bad tool schema")),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn maps_stream_errors() {
        let f =
            from_stream_error(&json!({"error": {"message": "overloaded", "type": "server_error"}}));
        assert!(f.retryable);
        let f = from_stream_error(&json!({"error": "context length exceeded"}));
        assert!(matches!(f.error, ModelError::ContextOverflow(_)));
    }
}
