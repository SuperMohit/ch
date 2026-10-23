//! OpenAI-compatible chat-completions provider (provider id `openai`).
//!
//! Works with OpenAI, OpenRouter, vLLM, SGLang and other servers that implement
//! `POST /chat/completions` with streaming. Tool-call deltas are assembled by index;
//! transient failures are retried with exponential backoff and jitter.
//!
//! Config (`[plugins.provider-openai]`): see [`OpenAiConfig`].

pub mod errors;
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

const OFFICIAL_HOST: &str = "api.openai.com";

/// `[plugins.provider-openai]`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct OpenAiConfig {
    /// Environment variable holding the API key. Read on each call. May be unset when
    /// `base_url` is not api.openai.com (local vLLM/SGLang servers).
    pub api_key_env: String,
    /// API root including `/v1`.
    pub base_url: String,
    /// Retries for transient failures, after the first attempt.
    pub max_retries: u32,
    /// Fails a call when the server sends nothing for this long (seconds).
    pub timeout_secs: u64,
    /// First backoff delay in milliseconds; doubles per retry.
    pub retry_base_ms: u64,
    /// Context window reported for every model (the provider knows none by itself).
    pub context_window: Option<u64>,
    /// `reasoning_effort` for reasoning models; empty to omit.
    pub reasoning_effort: String,
    /// Output-cap field name. Empty picks `max_completion_tokens` for api.openai.com and
    /// `max_tokens` elsewhere.
    pub max_tokens_field: String,
}

impl Default for OpenAiConfig {
    fn default() -> Self {
        Self {
            api_key_env: "OPENAI_API_KEY".into(),
            base_url: "https://api.openai.com/v1".into(),
            max_retries: 4,
            timeout_secs: 600,
            retry_base_ms: 1000,
            context_window: None,
            reasoning_effort: String::new(),
            max_tokens_field: String::new(),
        }
    }
}

impl OpenAiConfig {
    fn is_official(&self) -> bool {
        self.base_url.contains(OFFICIAL_HOST)
    }

    fn body_options(&self) -> BodyOptions {
        let field = match self.max_tokens_field.trim() {
            "" if self.is_official() => "max_completion_tokens",
            "" => "max_tokens",
            other => other,
        };
        BodyOptions {
            max_tokens_field: field.to_string(),
            reasoning_effort: self.reasoning_effort.trim().to_string(),
        }
    }
}

/// The `openai` model provider.
pub struct OpenAiProvider {
    cfg: OpenAiConfig,
    http: reqwest::Client,
    api_key: Option<String>,
}

impl OpenAiProvider {
    pub fn new(cfg: OpenAiConfig) -> anyhow::Result<Self> {
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

    /// The bearer token, if any. Missing keys are an error only for api.openai.com.
    fn api_key(&self) -> Result<Option<String>, ModelError> {
        if let Some(key) = &self.api_key {
            return Ok(Some(key.clone()));
        }
        let var = &self.cfg.api_key_env;
        match std::env::var(var) {
            Ok(k) if !k.trim().is_empty() => Ok(Some(k.trim().to_string())),
            _ if !self.cfg.is_official() => Ok(None),
            _ => Err(ModelError::Auth(format!(
                "environment variable {var} is not set. Export your OpenAI API key as {var}, \
                 or point plugins.provider-openai.api_key_env at the variable that holds it"
            ))),
        }
    }

    fn url(&self) -> String {
        format!(
            "{}/chat/completions",
            self.cfg.base_url.trim_end_matches('/')
        )
    }

    async fn attempt(
        &self,
        key: Option<&str>,
        body: &Value,
        cancel: &CancellationToken,
    ) -> Result<ModelStream, Failure> {
        let mut req = self
            .http
            .post(self.url())
            .header("accept", "text/event-stream")
            .json(body);
        if let Some(key) = key {
            req = req.bearer_auth(key);
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
        stream::start(stream::sse_data(resp.bytes_stream()), cancel.clone()).await
    }
}

fn transport_failure(e: reqwest::Error) -> Failure {
    if e.is_builder() {
        return Failure::fatal(ModelError::InvalidRequest(format!(
            "could not build the request: {e}. Check plugins.provider-openai.base_url and \
             the API key"
        )));
    }
    Failure::retry(
        ModelError::Unavailable(format!("could not reach the model server: {e}")),
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
impl ModelProvider for OpenAiProvider {
    fn id(&self) -> &str {
        "openai"
    }

    async fn stream(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
    ) -> Result<ModelStream, ModelError> {
        let key = self.api_key()?;
        let body = build_body(&request, &self.cfg.body_options());
        let base = Duration::from_millis(self.cfg.retry_base_ms);
        let mut retries = 0;
        loop {
            if cancel.is_cancelled() {
                return Err(ModelError::Cancelled);
            }
            match self.attempt(key.as_deref(), &body, &cancel).await {
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

    fn context_window(&self, _model: &str) -> Option<u64> {
        self.cfg.context_window
    }
}

/// Registers the `openai` provider.
pub struct OpenAiPlugin;

impl Plugin for OpenAiPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest::new(
            "provider-openai",
            "OpenAI-compatible chat-completions model provider (OpenAI, OpenRouter, vLLM, SGLang)",
        )
        .provides(&["provider:openai"])
        .project_settings(&[
            "max_retries",
            "timeout_secs",
            "retry_base_ms",
            "context_window",
            "reasoning_effort",
            "max_tokens_field",
        ])
    }

    fn register(&self, cx: &mut RegisterCx<'_>) -> anyhow::Result<()> {
        let cfg: OpenAiConfig = cx.config()?;
        cx.add_provider(OpenAiProvider::new(cfg)?);
        Ok(())
    }
}

/// The plugin, for the CLI's plugin list.
pub fn plugin() -> Arc<dyn Plugin> {
    Arc::new(OpenAiPlugin)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_rules_depend_on_host() {
        let official = OpenAiProvider::new(OpenAiConfig {
            api_key_env: "CH_TEST_SURELY_UNSET_OPENAI_VAR".into(),
            ..Default::default()
        })
        .unwrap();
        assert!(matches!(official.api_key(), Err(ModelError::Auth(_))));

        let local = OpenAiProvider::new(OpenAiConfig {
            api_key_env: "CH_TEST_SURELY_UNSET_OPENAI_VAR".into(),
            base_url: "http://localhost:8000/v1".into(),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(local.api_key().unwrap(), None);
    }

    #[test]
    fn max_tokens_field_defaults_by_host() {
        assert_eq!(
            OpenAiConfig::default().body_options().max_tokens_field,
            "max_completion_tokens"
        );
        let local = OpenAiConfig {
            base_url: "http://localhost:8000/v1".into(),
            ..Default::default()
        };
        assert_eq!(local.body_options().max_tokens_field, "max_tokens");
    }
}
