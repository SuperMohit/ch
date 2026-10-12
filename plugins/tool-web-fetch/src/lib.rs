//! Web fetch tool (plugin `tool-web-fetch`, tool `web_fetch`, read-only).
//!
//! GETs an http(s) URL with a timeout, a redirect limit and a download cap. HTML is
//! converted to markdown (or plain text); JSON and text pass through. Every failure (bad
//! scheme, DNS, timeout, HTTP 4xx/5xx, binary content) is a recoverable observation.
//!
//! ```toml
//! [plugins.tool-web-fetch]
//! user_agent = "ch/0.1 (coding agent)"
//! timeout_secs = 30
//! max_bytes = 5242880
//! max_redirects = 10
//! allow_private_hosts = false
//! ```
//!
//! Redirects are followed only within the site the request started on; a redirect to
//! another site is reported so the model fetches it in a new call, which goes through
//! the permission check again. Host names that resolve to private, loopback or
//! link-local addresses are refused unless `allow_private_hosts` is set; URLs that name
//! such an address directly (`http://127.0.0.1:3000`, `http://localhost:8080`) work.

mod convert;
mod fetch;
mod tool;

use std::sync::Arc;

use ch_kernel::{Plugin, PluginManifest, RegisterCx};
use serde::Deserialize;

pub use tool::WebFetchTool;

/// `[plugins.tool-web-fetch]` settings.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct WebFetchConfig {
    /// `User-Agent` header sent with every request.
    pub user_agent: String,
    /// Whole-request timeout, including reading the body.
    pub timeout_secs: u64,
    /// The download stops after this many bytes (the result is then truncated again
    /// by the kernel's tool-output limit).
    pub max_bytes: usize,
    /// Same-site redirects followed before giving up.
    pub max_redirects: usize,
    /// Let host names resolve to private, loopback and link-local addresses (an
    /// intranet). Off by default, so a public name can't lead into the local network.
    pub allow_private_hosts: bool,
}

impl Default for WebFetchConfig {
    fn default() -> Self {
        Self {
            user_agent: format!("ch/{} (coding agent)", env!("CARGO_PKG_VERSION")),
            timeout_secs: 30,
            max_bytes: 5 * 1024 * 1024,
            max_redirects: 10,
            allow_private_hosts: false,
        }
    }
}

/// Registers the `web_fetch` tool.
pub struct ToolWebFetchPlugin;

impl Plugin for ToolWebFetchPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest::new(
            "tool-web-fetch",
            "The `web_fetch` tool: fetches http(s) pages as markdown or text",
        )
        .version(env!("CARGO_PKG_VERSION"))
        .provides(&["tool:web_fetch"])
    }

    fn register(&self, cx: &mut RegisterCx<'_>) -> anyhow::Result<()> {
        let config: WebFetchConfig = cx.config()?;
        cx.add_tool(WebFetchTool::new(config)?);
        Ok(())
    }
}

/// The plugin, for the CLI's plugin list.
pub fn plugin() -> Arc<dyn Plugin> {
    Arc::new(ToolWebFetchPlugin)
}
