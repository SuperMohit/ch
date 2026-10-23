//! Anthropic Messages API provider (provider id `anthropic`).
//!
//! Streams `POST /v1/messages` over SSE and assembles text, thinking and tool_use blocks
//! into kernel types. Transient failures (429, 5xx, 529 overloaded, connection errors and
//! stream errors before any output) are retried with exponential backoff and jitter.
//!
//! Config (`[plugins.provider-anthropic]`): see [`AnthropicConfig`].

pub mod errors;
pub mod models;
pub mod request;
pub mod retry;
pub mod stream;

use std::sync::Arc;
use std::time::Duration;

use ch_kernel::reexports::{CancellationToken, async_trait};
use ch_kernel::{
    ModelError, ModelProvider, ModelRequest, ModelStream, Plugin, PluginManifest, RegisterCx,
};
use serde::Deserialize;
use serde_json::Value;

use crate::errors::{Failure, from_status};
use crate::request::{BodyOptions, build_body};

/// Messages API version header value.
pub const API_VERSION_HEADER: &str = "2023-06-01";
/// Beta header for `fallbacks: "default"`.
pub const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
const OFFICIAL_HOST: &str = "api.anthropic.com";

/// `[plugins.provider-anthropic]`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct AnthropicConfig {
    /// Environment variable holding the API key. Read on each call, not at startup.
    pub api_key_env: String,
    /// API root, without `/v1`.
    pub base_url: String,
    /// Retries for transient failures, after the first attempt.
    pub max_retries: u32,
    /// Fails a call when the server sends nothing for this long (seconds).
    pub timeout_secs: u64,
    /// First backoff delay in milliseconds; doubles per retry.
    pub retry_base_ms: u64,
    /// `output_config.effort` for adaptive-thinking models (`low` … `max`); empty to let
    /// the API pick its default.
    pub effort: String,
    /// `thinking.display` on models that hide thinking text by default; empty to omit.
    pub thinking_display: String,
    /// Server-side refusal fallback (`"default"`) on models that support it; empty or
    /// `"off"` to disable. Only sent to the official API.
    pub fallbacks: String,
    /// Stream tool inputs without server-side buffering. Defaults to on for the official
    /// API and off for other base URLs (proxies may reject the field).
    pub eager_input_streaming: Option<bool>,
}

impl Default for AnthropicConfig {
    fn default() -> Self {
        Self {
            api_key_env: "ANTHROPIC_API_KEY".into(),
            base_url: "https://api.anthropic.com".into(),
            max_retries: 4,
            timeout_secs: 600,
            retry_base_ms: 1000,
            effort: "high".into(),
            thinking_display: "summarized".into(),
            fallbacks: "default".into(),
            eager_input_streaming: None,
        }
    }
}

impl AnthropicConfig {
    fn is_official(&self) -> bool {
        self.base_url.contains(OFFICIAL_HOST)
    }

    fn body_options(&self) -> BodyOptions {
        let fallbacks = match self.fallbacks.trim() {
            "off" | "none" | "false" => String::new(),
            _ if !self.is_official() => String::new(),
            other => other.to_string(),
        };
        BodyOptions {
            eager_input_streaming: self.eager_input_streaming.unwrap_or(self.is_official()),
            thinking_display: self.thinking_display.trim().to_string(),
            effort: self.effort.trim().to_string(),
            fallbacks,
        }
    }
}

/// The `anthropic` model provider.
pub struct AnthropicProvider {
    cfg: AnthropicConfig,
    http: reqwest::Client,
    api_key: Option<String>,
}

impl AnthropicProvider {
    pub fn new(cfg: AnthropicConfig) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .read_timeout(Duration::from_secs(cfg.timeout_secs.max(1)))
            .build()?;
        Ok(Self {
            cfg,
            http,
            api_key: None,
        })
    }

    /// Uses `key` instead of reading `api_key_env` (for embedding and tests).
    pub fn with_api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self
    }

    fn api_key(&self) -> Result<String, ModelError> {
        if let Some(key) = &self.api_key {
            return Ok(key.clone());
        }
        let var = &self.cfg.api_key_env;
        match std::env::var(var) {
            Ok(k) if !k.trim().is_empty() => Ok(k.trim().to_string()),
            _ => Err(ModelError::Auth(format!(
                "environment variable {var} is not set. Export your Anthropic API key as \
                 {var}, or point plugins.provider-anthropic.api_key_env at the variable \
                 that holds it"
            ))),
        }
    }

    fn url(&self) -> String {
        format!("{}/v1/messages", self.cfg.base_url.trim_end_matches('/'))
    }

    async fn attempt(
        &self,
        key: &str,
        body: &Value,
        beta: bool,
        cancel: &CancellationToken,
    ) -> Result<ModelStream, Failure> {
        let mut req = self
            .http
            .post(self.url())
            .header("x-api-key", key)
            .header("anthropic-version", API_VERSION_HEADER)
            .header("accept", "text/event-stream")
            .json(body);
        if beta {
            req = req.header("anthropic-beta", FALLBACK_BETA);
        }
        let sent = tokio::select! {
            _ = cancel.cancelled() => return Err(Failure::fatal(ModelError::Cancelled)),
            r = req.send() => r,
        };
        let resp = sent.map_err(transport_failure)?;
        let status = resp.status();
        if !status.is_success() {
            let wait = retry::retry_after(resp.headers());
            let text = tokio::select! {
                _ = cancel.cancelled() => return Err(Failure::fatal(ModelError::Cancelled)),
                t = resp.text() => t.unwrap_or_default(),
            };
            return Err(from_status(status.as_u16(), &text, wait));
        }
        stream::start(stream::sse_events(resp.bytes_stream()), cancel.clone()).await
    }
}

fn transport_failure(e: reqwest::Error) -> Failure {
    if e.is_builder() {
        return Failure::fatal(ModelError::InvalidRequest(format!(
            "could not build the request: {e}. Check plugins.provider-anthropic.base_url \
             and the API key"
        )));
    }
    Failure::retry(
        ModelError::Unavailable(format!("could not reach the Anthropic API: {e}")),
        None,
    )
}

/// Adds how many retries were spent to a final transient error.
fn exhausted(error: ModelError, retries: u32) -> ModelError {
    if retries == 0 {
        return error;
    }
    let note = |m: String| format!("{m} (gave up after {retries} retries)");
    match error {
        ModelError::RateLimited(m) => ModelError::RateLimited(note(m)),
        ModelError::Unavailable(m) => ModelError::Unavailable(note(m)),
        other => other,
    }
}

#[async_trait]
impl ModelProvider for AnthropicProvider {
    fn id(&self) -> &str {
        "anthropic"
    }

    async fn stream(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
    ) -> Result<ModelStream, ModelError> {
        let key = self.api_key()?;
        let opts = self.cfg.body_options();
        let beta = opts.uses_fallbacks(&request.model);
        let body = build_body(&request, &opts);
        let base = Duration::from_millis(self.cfg.retry_base_ms);
        let mut retries = 0;
        loop {
            if cancel.is_cancelled() {
                return Err(ModelError::Cancelled);
            }
            match self.attempt(&key, &body, beta, &cancel).await {
                Ok(stream) => return Ok(stream),
                Err(f) if f.retryable && retries < self.cfg.max_retries => {
                    let delay = f
                        .retry_after
                        .unwrap_or_else(|| retry::backoff(base, retries));
                    retry::sleep(delay, &cancel).await?;
                    retries += 1;
                }
                Err(f) => return Err(exhausted(f.error, retries)),
            }
        }
    }

    fn context_window(&self, model: &str) -> Option<u64> {
        models::context_window(model)
    }

    fn default_model(&self) -> Option<&str> {
        Some(models::DEFAULT_MODEL)
    }
}

/// Registers the `anthropic` provider.
pub struct AnthropicPlugin;

impl Plugin for AnthropicPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest::new(
            "provider-anthropic",
            "Anthropic Messages API model provider (streaming, retries, prompt caching)",
        )
        .provides(&["provider:anthropic"])
        .project_settings(&[
            "max_retries",
            "timeout_secs",
            "retry_base_ms",
            "effort",
            "thinking_display",
            "fallbacks",
            "eager_input_streaming",
        ])
    }

    fn register(&self, cx: &mut RegisterCx<'_>) -> anyhow::Result<()> {
        let cfg: AnthropicConfig = cx.config()?;
        cx.add_provider(AnthropicProvider::new(cfg)?);
        Ok(())
    }
}

/// The plugin, for the CLI's plugin list.
pub fn plugin() -> Arc<dyn Plugin> {
    Arc::new(AnthropicPlugin)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_options_depend_on_base_url() {
        let official = AnthropicConfig::default().body_options();
        assert!(official.eager_input_streaming);
        assert_eq!(official.fallbacks, "default");

        let proxy = AnthropicConfig {
            base_url: "http://localhost:8080".into(),
            ..Default::default()
        }
        .body_options();
        assert!(!proxy.eager_input_streaming);
        assert!(proxy.fallbacks.is_empty());

        let off = AnthropicConfig {
            fallbacks: "off".into(),
            ..Default::default()
        }
        .body_options();
        assert!(off.fallbacks.is_empty());
    }

    #[test]
    fn missing_key_is_an_auth_error() {
        let p = AnthropicProvider::new(AnthropicConfig {
            api_key_env: "CH_TEST_SURELY_UNSET_KEY_VAR".into(),
            ..Default::default()
        })
        .unwrap();
        match p.api_key() {
            Err(ModelError::Auth(m)) => assert!(m.contains("CH_TEST_SURELY_UNSET_KEY_VAR")),
            other => panic!("{other:?}"),
        }
    }
}
