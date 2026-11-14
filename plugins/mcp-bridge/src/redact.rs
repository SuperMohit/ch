//! Stripping secrets from server text before it reaches the model, the terminal, logs
//! or session files (ADR 0006 §8).

use std::sync::LazyLock;

use regex::Regex;

/// Configured values shorter than this aren't redacted: they would match ordinary text.
pub const MIN_SECRET_LEN: usize = 8;

const MASK: &str = "[redacted]";

/// `Authorization: <anything to the end of the value>`, in a header line, JSON or text.
static AUTHORIZATION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(\b(?:proxy-)?authorization\b["']?\s*[:=]\s*["']?)[^\r\n"']+"#).unwrap()
});

/// `Bearer <token>` and `Basic <credentials>`.
static SCHEME_TOKEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(bearer|basic)\s+[A-Za-z0-9._~+/=-]{8,}").unwrap());

/// `scheme://user:password@host`, and `scheme://token@host` (a token as the user name).
static URL_CREDENTIALS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b([a-z][a-z0-9+.-]*://)[^\s/?#@]+@").unwrap());

/// Query parameters that usually carry a credential (`?api_key=...`).
static QUERY_SECRET: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)([?&](?:api[_-]?key|key|token|access[_-]?token|auth|password|secret|sig|signature|code)=)[^&\s#]+",
    )
    .unwrap()
});

/// Redacts one server's secrets: its configured env and header values, plus anything
/// that looks like a credential.
#[derive(Debug, Clone, Default)]
pub struct Redactor {
    /// Longest first, so a value containing another is replaced whole.
    values: Vec<String>,
}

impl Redactor {
    pub fn new(values: impl IntoIterator<Item = String>) -> Self {
        let mut values: Vec<String> = values
            .into_iter()
            .filter(|v| v.chars().count() >= MIN_SECRET_LEN)
            .collect();
        values.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        values.dedup();
        Self { values }
    }

    pub fn redact(&self, text: &str) -> String {
        let mut out = text.to_string();
        for v in &self.values {
            if out.contains(v.as_str()) {
                out = out.replace(v.as_str(), MASK);
            }
        }
        redact_patterns(&out)
    }
}

/// The credential patterns alone, for text with no server attached.
pub fn redact_patterns(text: &str) -> String {
    let out = AUTHORIZATION.replace_all(text, format!("${{1}}{MASK}"));
    let out = SCHEME_TOKEN.replace_all(&out, format!("${{1}} {MASK}"));
    let out = URL_CREDENTIALS.replace_all(&out, format!("${{1}}{MASK}@"));
    QUERY_SECRET
        .replace_all(&out, format!("${{1}}{MASK}"))
        .into_owned()
}

/// `url` with credentials (user info, secret-looking query parameters) replaced, for
/// showing.
pub fn url_without_credentials(url: &str) -> String {
    redact_patterns(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_values_and_patterns() {
        let r = Redactor::new(["ghp_abcdefgh123".to_string(), "short".to_string()]);
        let text = "token ghp_abcdefgh123 failed; short stays\n\
                    Authorization: Bearer xyz.abc.def\n\
                    {\"authorization\": \"Basic dXNlcjpwYXNz\"}\n\
                    curl -H 'Bearer abcdefghijkl' https://bob:hunter22@example.com/x";
        let out = r.redact(text);
        for secret in [
            "ghp_abcdefgh123",
            "xyz.abc.def",
            "dXNlcjpwYXNz",
            "abcdefghijkl",
            "hunter22",
            "bob:",
        ] {
            assert!(!out.contains(secret), "{secret} in {out}");
        }
        assert!(out.contains("short stays"), "{out}");
        assert!(out.contains("https://[redacted]@example.com/x"), "{out}");
        let out = r.redact("see https://ghp_abc123@github.com/x?api_key=k123&page=2");
        assert_eq!(
            out,
            "see https://[redacted]@github.com/x?api_key=[redacted]&page=2"
        );
    }

    #[test]
    fn longer_values_go_first() {
        let r = Redactor::new(["abcdefgh".to_string(), "abcdefgh-longer".to_string()]);
        assert_eq!(r.redact("x abcdefgh-longer y"), "x [redacted] y");
    }

    #[test]
    fn urls() {
        assert_eq!(
            url_without_credentials("https://u:p@x.example/mcp"),
            "https://[redacted]@x.example/mcp"
        );
        assert_eq!(
            url_without_credentials("https://x.example/mcp"),
            "https://x.example/mcp"
        );
    }
}
