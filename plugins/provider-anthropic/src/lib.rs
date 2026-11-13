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
    ToolChanges,
};
use serde::Deserialize;
use serde_json::Value;

use crate::errors::{Failure, from_status};
use crate::request::{BodyOptions, build_body};

/// Messages API version header value.
pub const API_VERSION_HEADER: &str = "2023-06-01";
/// Beta header for `fallbacks: "default"`.
pub const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
/// Beta header for `thinking.block_binding`.
pub const BINDING_BETA: &str = "thinking-binding-controls-2026-08-01";
/// Beta header for `defer_loading` with `tool_addition`/`tool_removal` (ADR 0006).
pub const TOOL_CHANGES_BETA: &str = "mid-conversation-tool-changes-2026-07-01";
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
    /// `thinking.block_binding.prefix_mismatch_behavior` (such as `"error"`), sent with
    /// the binding-controls beta when thinking is on; empty to leave it to the account.
    pub prefix_mismatch_behavior: String,
    /// Tools added or withdrawn mid-conversation: `"auto"` announces them with the
    /// mid-conversation tool-change beta on the official API for models that accept it;
    /// `"on"` also does through another base URL (a proxy that passes the beta through);
    /// `"off"` never does.
    /// Without it, a model that checks its earlier reasoning keeps its first tool list
    /// until `/clear`, and other models get the new list and a note.
    pub tool_changes: String,
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
            prefix_mismatch_behavior: String::new(),
            tool_changes: "auto".into(),
        }
    }
}

impl AnthropicConfig {
    fn is_official(&self) -> bool {
        self.base_url.contains(OFFICIAL_HOST)
    }

    /// How requests for `model` handle tools that change mid-conversation.
    pub fn tool_changes(&self, model: &str) -> ToolChanges {
        let caps = crate::models::caps(model);
        let native = caps.tool_changes
            && match self.tool_changes.trim() {
                "on" => true,
                "off" => false,
                _ => self.is_official(),
            };
        if native {
            ToolChanges::Native
        } else if caps.checks_thinking {
            ToolChanges::Frozen
        } else {
            ToolChanges::Replace
        }
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
            prefix_mismatch_behavior: self.prefix_mismatch_behavior.trim().to_string(),
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
        betas: &[&str],
        cancel: &CancellationToken,
    ) -> Result<ModelStream, Failure> {
        let mut req = self
            .http
            .post(self.url())
            .header("x-api-key", key)
            .header("anthropic-version", API_VERSION_HEADER)
            .header("accept", "text/event-stream")
            .json(body);
        if !betas.is_empty() {
            req = req.header("anthropic-beta", betas.join(","));
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
        let body = build_body(&request, &opts);
        let betas = opts.betas(&body);
        let base = Duration::from_millis(self.cfg.retry_base_ms);
        let mut retries = 0;
        loop {
            if cancel.is_cancelled() {
                return Err(ModelError::Cancelled);
            }
            match self.attempt(&key, &body, &betas, &cancel).await {
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

    fn tool_changes(&self, model: &str) -> ToolChanges {
        self.cfg.tool_changes(model)
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
    #[test]
    fn tool_changes_are_native_only_where_the_api_takes_them() {
        use ch_kernel::ToolChanges::*;
        let official = AnthropicConfig::default();
        assert_eq!(official.tool_changes("claude-opus-5-5"), Native);
        assert_eq!(official.tool_changes("claude-sonnet-5"), Replace);
        assert_eq!(official.tool_changes("claude-opus-4-7"), Replace);
        let proxy = AnthropicConfig {
            base_url: "https://gateway.example.com".into(),
            ..AnthropicConfig::default()
        };
        // A model that checks its reasoning can't have its tool list changed there.
        assert_eq!(proxy.tool_changes("claude-opus-5-5"), Frozen);
        assert_eq!(proxy.tool_changes("claude-opus-4-8"), Replace);
        let on = AnthropicConfig {
            tool_changes: "on".into(),
            ..proxy
        };
        assert_eq!(on.tool_changes("claude-opus-5-5"), Native);
        // Not for a model that doesn't accept the beta.
        assert_eq!(on.tool_changes("claude-sonnet-5"), Replace);
        let off = AnthropicConfig {
            tool_changes: "off".into(),
            ..AnthropicConfig::default()
        };
        assert_eq!(off.tool_changes("claude-opus-5-5"), Frozen);
    }

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
