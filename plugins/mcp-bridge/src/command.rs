//! `/mcp`: list servers, reconnect one, trust a project server.

use std::sync::{Arc, MutexGuard};

use async_trait::async_trait;
use ch_kernel::util::printable;
use ch_kernel::{CmdCx, Command, Outcome, SourceStatus, ToolSource as _};

use crate::launch::{describe, trust_hash};
use crate::server::{McpServer, endpoint};
use crate::{Bridge, ServerConfig};

pub(crate) struct McpCommand {
    bridge: Arc<Bridge>,
}

impl McpCommand {
    pub(crate) fn new(bridge: Arc<Bridge>) -> Self {
        Self { bridge }
    }
}

fn lock<T>(m: &std::sync::Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn status_text(status: Option<&SourceStatus>) -> String {
    match status {
        Some(SourceStatus::Starting) => "starting".into(),
        Some(SourceStatus::Ready { tools: 1 }) => "connected, 1 tool".into(),
        Some(SourceStatus::Ready { tools }) => format!("connected, {tools} tools"),
        Some(SourceStatus::Failed { reason }) => format!("failed: {}", printable(reason)),
        Some(SourceStatus::Exited { reason }) => format!("exited: {}", printable(reason)),
        Some(SourceStatus::Stopped) | None => "stopped".into(),
    }
}

#[async_trait]
impl Command for McpCommand {
    fn name(&self) -> &str {
        "mcp"
    }

    fn description(&self) -> &str {
        "List MCP servers, reconnect one, or trust a project server"
    }

    fn usage(&self) -> &str {
        "[reconnect <server> | trust <server>]"
    }

    async fn run(&self, args: &str, cx: &mut CmdCx<'_>) -> anyhow::Result<Outcome> {
        let words: Vec<&str> = args.split_whitespace().collect();
        let text = match words.as_slice() {
            [] | ["list"] => self.list(cx),
            ["reconnect", name] => self.reconnect(name, cx).await,
            ["trust", name] => self.show_for_trust(name),
            ["trust", name, "yes"] => self.trust(name, cx).await,
            _ => format!("Usage: /mcp {}", self.usage()),
        };
        Ok(Outcome::Message(text))
    }
}

impl McpCommand {
    fn server(&self, name: &str) -> Option<Arc<McpServer>> {
        lock(&self.bridge.servers)
            .iter()
            .find(|s| s.config().name == name)
            .cloned()
    }

    fn list(&self, cx: &CmdCx<'_>) -> String {
        let statuses = cx.session.tool_sources();
        let servers = lock(&self.bridge.servers).clone();
        let untrusted = lock(&self.bridge.untrusted).clone();
        if servers.is_empty() && untrusted.is_empty() {
            return "No MCP servers are configured. Add `[[mcp_servers]]` to \
                    ~/.ch/config.toml, or a project's `.mcp.json`."
                .into();
        }
        let mut lines = vec!["MCP servers:".to_string()];
        for s in &servers {
            let status = statuses
                .iter()
                .find(|(id, _)| id == s.id())
                .map(|(_, st)| st);
            let info = s
                .server_info()
                .map(|i| format!(" ({i})"))
                .unwrap_or_default();
            lines.push(format!(
                "- `{}` {}: {}{info}",
                s.config().name,
                endpoint(s.config()),
                status_text(status)
            ));
        }
        for s in &untrusted {
            lines.push(format!(
                "- `{}` {}: not trusted (from {}); `/mcp trust {}` shows what it runs",
                s.name,
                endpoint(s),
                printable(&s.origin.describe(&self.bridge.places.workspace)),
                s.name
            ));
        }
        lines.join("\n")
    }

    async fn reconnect(&self, name: &str, cx: &mut CmdCx<'_>) -> String {
        let Some(server) = self.server(name) else {
            return format!("No running MCP server is named `{}`.", printable(name));
        };
        let id = server.id().to_string();
        cx.session.remove_tool_source(&id).await;
        if let Err(e) = cx.session.add_tool_source(server.clone()).await {
            return format!(
                "Couldn't restart `{name}`: {}",
                printable(&format!("{e:#}"))
            );
        }
        let status = cx
            .session
            .tool_sources()
            .into_iter()
            .find(|(i, _)| *i == id)
            .map(|(_, s)| s);
        format!("`{name}`: {}", status_text(status.as_ref()))
    }

    fn untrusted(&self, name: &str) -> Option<ServerConfig> {
        lock(&self.bridge.untrusted)
            .iter()
            .find(|s| s.name == name)
            .cloned()
    }

    /// The first step of `/mcp trust`: what the server would run.
    fn show_for_trust(&self, name: &str) -> String {
        let Some(cfg) = self.untrusted(name) else {
            return format!(
                "No project MCP server named `{}` is waiting for trust.",
                printable(name)
            );
        };
        let places = &self.bridge.places;
        let hash = trust_hash(&cfg, places);
        {
            let mut shown = lock(&self.bridge.shown);
            shown.retain(|(n, _)| n != name);
            shown.push((name.to_string(), hash));
        }
        let mut lines = vec![format!("The project's MCP server `{name}`:")];
        for (label, value) in describe(&cfg, places) {
            lines.push(format!("- {label}: {value}"));
        }
        lines.push(format!(
            "It runs with your permissions and its tools go to the model. To trust it for \
             this project and start it, type `/mcp trust {name} yes`."
        ));
        lines.join("\n")
    }

    /// The second step: records trust in what was shown, then starts the server.
    async fn trust(&self, name: &str, cx: &mut CmdCx<'_>) -> String {
        let Some(cfg) = self.untrusted(name) else {
            return format!(
                "No project MCP server named `{}` is waiting for trust.",
                printable(name)
            );
        };
        let places = &self.bridge.places;
        let hash = trust_hash(&cfg, places);
        let shown = lock(&self.bridge.shown)
            .iter()
            .any(|(n, h)| n == name && *h == hash);
        if !shown {
            return format!(
                "`{name}` changed since it was shown, or wasn't shown yet.\n\n{}",
                self.show_for_trust(name)
            );
        }
        let Some(save) = self.bridge.project.as_ref().and_then(|p| p.trust.as_ref()) else {
            return "Trust can't be saved in this run.".into();
        };
        if let Err(e) = save(&cfg, &hash) {
            return format!("Couldn't save trust: {}", printable(&format!("{e:#}")));
        }
        lock(&self.bridge.untrusted).retain(|s| s.name != name);
        lock(&self.bridge.shown).retain(|(n, _)| n != name);
        let server = Arc::new(McpServer::trusted(cfg, places.clone(), hash));
        lock(&self.bridge.servers).push(server.clone());
        let id = server.id().to_string();
        if let Err(e) = cx.session.add_tool_source(server).await {
            return format!(
                "Trusted `{name}`, but couldn't start it: {}",
                printable(&format!("{e:#}"))
            );
        }
        let status = cx
            .session
            .tool_sources()
            .into_iter()
            .find(|(i, _)| *i == id)
            .map(|(_, s)| s);
        format!(
            "Trusted `{name}` for this project; you'll be asked again if its entry changes.\n\
             `{name}`: {}",
            status_text(status.as_ref())
        )
    }
}
