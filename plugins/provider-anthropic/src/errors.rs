//! Mapping API errors (HTTP status or stream `error` events) to [`ModelError`].

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

/// Whether an API error message is about the prompt not fitting the context window.
pub fn is_context_overflow(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    [
        "prompt is too long",
        "context window",
        "context length",
        "context_length",
        "maximum context",
        "input is too long",
    ]
    .iter()
    .any(|needle| m.contains(needle))
}

/// Extracts `(error.type, error.message)` from an API error body, falling back to the raw
/// text.
pub fn parse_error_body(body: &str) -> (String, String) {
    match serde_json::from_str::<Value>(body) {
        Ok(v) => {
            let err = v.get("error").unwrap_or(&v);
            let kind = err["type"].as_str().unwrap_or("").to_string();
            let msg = err["message"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| body.trim().to_string());
            (kind, msg)
        }
        Err(_) => (String::new(), body.trim().to_string()),
    }
}

/// Maps an error by its API `error.type` (used for stream `error` events, where there is
/// no HTTP status).
pub fn from_error_type(kind: &str, message: &str) -> Failure {
    match kind {
        "overloaded_error" => Failure::retry(
            ModelError::Unavailable(format!("Anthropic API overloaded: {message}")),
            None,
        ),
        "api_error" | "timeout_error" => Failure::retry(
            ModelError::Unavailable(format!("Anthropic API error: {message}")),
            None,
        ),
        "rate_limit_error" => Failure::retry(ModelError::RateLimited(message.to_string()), None),
        "authentication_error" | "permission_error" => {
            Failure::fatal(ModelError::Auth(message.to_string()))
        }
        "request_too_large" => Failure::fatal(ModelError::ContextOverflow(message.to_string())),
        _ if is_context_overflow(message) => {
            Failure::fatal(ModelError::ContextOverflow(message.to_string()))
        }
        _ => Failure::fatal(ModelError::InvalidRequest(if kind.is_empty() {
            message.to_string()
        } else {
            format!("{kind}: {message}")
        })),
    }
}

/// Maps a non-success HTTP response.
pub fn from_status(status: u16, body: &str, retry_after: Option<Duration>) -> Failure {
    let (kind, message) = parse_error_body(body);
    match status {
        401 | 403 => Failure::fatal(ModelError::Auth(format!(
            "HTTP {status}: {message}. Check that the API key is valid and has access to \
             this model"
        ))),
        429 => Failure::retry(ModelError::RateLimited(message), retry_after),
        408 | 409 | 500..=599 => Failure::retry(
            ModelError::Unavailable(format!("HTTP {status}: {message}")),
            retry_after,
        ),
        413 => Failure::fatal(ModelError::ContextOverflow(message)),
        400 if is_context_overflow(&message) => {
            Failure::fatal(ModelError::ContextOverflow(message))
        }
        404 => Failure::fatal(ModelError::InvalidRequest(format!(
            "HTTP 404: {message}. Check model.id"
        ))),
        _ => {
            let mut f = from_error_type(&kind, &message);
            if !f.retryable && matches!(f.error, ModelError::InvalidRequest(_)) {
                f.error = ModelError::InvalidRequest(format!("HTTP {status}: {message}"));
            }
            f
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_mapping() {
        let overflow = r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 1000001 tokens > 1000000 maximum"}}"#;
        assert!(matches!(
            from_status(400, overflow, None).error,
            ModelError::ContextOverflow(_)
        ));
        let bad = r#"{"type":"error","error":{"type":"invalid_request_error","message":"messages: roles must alternate"}}"#;
        let f = from_status(400, bad, None);
        assert!(!f.retryable);
        match f.error {
            ModelError::InvalidRequest(m) => assert!(m.contains("roles must alternate")),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            from_status(401, "{}", None).error,
            ModelError::Auth(_)
        ));
        assert!(matches!(
            from_status(403, "nope", None).error,
            ModelError::Auth(_)
        ));
        let f = from_status(429, "{}", Some(Duration::from_secs(2)));
        assert!(f.retryable && matches!(f.error, ModelError::RateLimited(_)));
        assert_eq!(f.retry_after, Some(Duration::from_secs(2)));
        let f = from_status(
            529,
            r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
            None,
        );
        assert!(f.retryable && matches!(f.error, ModelError::Unavailable(_)));
        assert!(from_status(500, "", None).retryable);
    }

    #[test]
    fn stream_error_types() {
        assert!(from_error_type("overloaded_error", "Overloaded").retryable);
        assert!(matches!(
            from_error_type("invalid_request_error", "prompt is too long").error,
            ModelError::ContextOverflow(_)
        ));
        assert!(matches!(
            from_error_type("authentication_error", "bad key").error,
            ModelError::Auth(_)
        ));
    }
}
