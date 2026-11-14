//! Tools from MCP servers (plugin `mcp-bridge`, roadmap 7.4, ADR 0006 §8–§11).
//!
//! Each configured server becomes a kernel tool source: it starts with the session,
//! over stdio or streamable HTTP, and its tools are offered as `mcp__<server>__<tool>`.
//! A server that changes its tools (`notifications/tools/list_changed`) or exits changes
//! the session's tools at the next model call; nothing needs a restart.
//!
//! ```toml
//! # ~/.ch/config.toml (trusted), or a project's .ch/config.toml (asks first)
//! [[mcp_servers]]
//! name = "github"                    # [a-z0-9-]{1,24}
//! command = "github-mcp-server"      # found on PATH, or a path
//! args = ["stdio"]
//! env = { GITHUB_TOKEN = "${GITHUB_TOKEN}" }   # ${VAR} from ch's environment
//! cwd = "workspace"                  # user-level servers start in $HOME otherwise
//! read_only_tools = ["list_issues"]  # exact names; the server's own hints are ignored
//! startup_timeout_secs = 10
//! call_timeout_secs = 120
//!
//! [[mcp_servers]]
//! name = "docs"
//! url = "https://docs.example.com/mcp"
//! headers = { Authorization = "Bearer ${DOCS_TOKEN}" }
//! ```
//!
//! A project's `.mcp.json` (`{"mcpServers": {...}}`) is read too. Project servers start
//! only once the user trusts each one (the CLI asks at startup, or `/mcp trust <name>`),
//! and a user-level server with the same name wins.
//!
//! What a server gets: a cleared environment (`PATH`, `HOME`, `USER`, `LANG`, `TERM`,
//! `TMPDIR`, `SHELL`, `CH_WORKSPACE` and its own `env`), its own process group, the
//! workspace as its only MCP root, and no sampling or elicitation. Its configured env
//! and header values, `Authorization` values, bearer tokens and URL credentials are
//! redacted from everything it sends back.

pub mod config;
pub mod launch;
pub mod procs;
pub mod redact;

mod command;
mod server;
mod tool;
mod transport;

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::bail;
use ch_kernel::util::printable;
use ch_kernel::{Config, Plugin, PluginManifest, RegisterCx};

pub use config::{Origin, OriginKind, Secret, ServerConfig};
pub use launch::Places;
pub use server::McpServer;
pub use tool::convert_result;

/// Records the user's trust in a project server: called with the entry and its
/// [`launch::trust_hash`].
pub type TrustFn = dyn Fn(&ServerConfig, &str) -> anyhow::Result<()> + Send + Sync;

/// The project's servers, as the CLI found them, with whether each is trusted. Provided
/// as a service when the registry is built.
pub struct ProjectServers {
    pub servers: Vec<(ServerConfig, bool)>,
    /// Saves trust given with `/mcp trust`. `None` makes that command refuse.
    pub trust: Option<Box<TrustFn>>,
}

/// Project servers and what was wrong with the rest.
#[derive(Debug, Default)]
pub struct Discovered {
    pub servers: Vec<ServerConfig>,
    pub warnings: Vec<String>,
}

/// The names of the user-level servers in the merged `config`, ignoring invalid entries
/// (registration reports those).
pub fn user_server_names(config: &Config) -> Vec<String> {
    config
        .get_value("mcp_servers")
        .map(|v| config::parse_toml(v, &Origin::user()))
        .unwrap_or_default()
        .into_iter()
        .filter_map(Result::ok)
        .map(|s| s.name)
        .collect()
}

/// Finds a project's servers: `mcp_servers` from its `.ch/config.toml` (passed in, as the
/// CLI already parsed that file) and then its `.mcp.json`. An entry that is invalid,
/// repeats a name, or has the name of a user-level server is left out with a warning.
pub fn discover_project(
    workspace: &Path,
    project_servers: Option<&toml::Value>,
    user_names: &[String],
) -> Discovered {
    let mut out = Discovered::default();
    let config_file = workspace.join(".ch").join("config.toml");
    let mut found: Vec<anyhow::Result<ServerConfig>> = Vec::new();
    if let Some(v) = project_servers {
        let origin = Origin {
            kind: OriginKind::ProjectConfig,
            file: config_file,
        };
        found.extend(config::parse_toml(v, &origin));
    }
    let json_file = workspace.join(".mcp.json");
    match read_inside(workspace, &json_file) {
        Ok(Some(text)) => {
            let origin = Origin {
                kind: OriginKind::McpJson,
                file: json_file,
            };
            match config::parse_mcp_json(&text, &origin) {
                Ok(list) => found.extend(list),
                Err(e) => out.warnings.push(format!("ignoring .mcp.json: {e:#}")),
            }
        }
        Ok(None) => {}
        Err(e) => out.warnings.push(format!("ignoring .mcp.json: {e:#}")),
    }
    let mut seen: HashSet<String> = HashSet::new();
    for entry in found {
        match entry {
            Err(e) => out.warnings.push(format!(
                "ignoring a project MCP server: {}",
                printable(&format!("{e:#}"))
            )),
            Ok(s) if user_names.contains(&s.name) => out.warnings.push(format!(
                "ignoring the project's MCP server `{}` from {}: your own config has a \
                 server with that name, and yours wins",
                s.name,
                printable(&s.origin.describe(workspace))
            )),
            Ok(s) if !seen.insert(s.name.clone()) => out.warnings.push(format!(
                "ignoring the second project MCP server named `{}` (from {})",
                s.name,
                printable(&s.origin.describe(workspace))
            )),
            Ok(s) => out.servers.push(s),
        }
    }
    out
}

/// Reads `file` if it exists, refusing one that resolves outside `workspace`.
fn read_inside(workspace: &Path, file: &Path) -> anyhow::Result<Option<String>> {
    let real = match file.canonicalize() {
        Ok(p) => p,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let root = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    if !real.starts_with(&root) {
        bail!("it points outside the workspace");
    }
    Ok(Some(std::fs::read_to_string(&real)?))
}

/// Everything the `/mcp` command needs.
pub(crate) struct Bridge {
    places: Places,
    /// Started with the session or trusted since, in precedence order.
    servers: Mutex<Vec<Arc<McpServer>>>,
    /// Project servers waiting for `/mcp trust`.
    untrusted: Mutex<Vec<ServerConfig>>,
    project: Option<Arc<ProjectServers>>,
    /// `/mcp trust <name>` shows the entry first; this is the hash that was shown.
    shown: Mutex<Vec<(String, String)>>,
}

/// The `mcp-bridge` plugin.
pub struct McpBridgePlugin;

impl Plugin for McpBridgePlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest::new(
            "mcp-bridge",
            "Tools from MCP servers (stdio and streamable HTTP), named mcp__<server>__<tool>",
        )
        .version(env!("CARGO_PKG_VERSION"))
        .provides(&["tool-source:mcp", "command:mcp"])
    }

    fn register(&self, cx: &mut RegisterCx<'_>) -> anyhow::Result<()> {
        let places = Places::new(cx.workspace());
        let mut started: Vec<ServerConfig> = Vec::new();
        if let Some(v) = cx.full_config().get_value("mcp_servers") {
            for entry in config::parse_toml(v, &Origin::user()) {
                let entry = entry?;
                if started.iter().any(|s| s.name == entry.name) {
                    bail!("two entries in mcp_servers are named `{}`", entry.name);
                }
                started.push(entry);
            }
        }
        let mut untrusted = Vec::new();
        let project = cx.service::<ProjectServers>();
        for (entry, trusted) in project.iter().flat_map(|p| &p.servers) {
            if !entry.origin.is_project() || started.iter().any(|s| s.name == entry.name) {
                continue;
            }
            if *trusted {
                started.push(entry.clone());
            } else {
                untrusted.push(entry.clone());
            }
        }
        let servers: Vec<Arc<McpServer>> = started
            .into_iter()
            .map(|s| {
                Arc::new(if s.origin.is_project() {
                    let hash = launch::trust_hash(&s, &places);
                    McpServer::trusted(s, places.clone(), hash)
                } else {
                    McpServer::new(s, places.clone())
                })
            })
            .collect();
        for s in &servers {
            cx.add_tool_source(s.clone());
        }
        let bridge = Arc::new(Bridge {
            places,
            servers: Mutex::new(servers),
            untrusted: Mutex::new(untrusted),
            project,
            shown: Mutex::default(),
        });
        cx.add_command(command::McpCommand::new(bridge));
        Ok(())
    }
}

/// The plugin, for the CLI's plugin list.
pub fn plugin() -> Arc<dyn Plugin> {
    Arc::new(McpBridgePlugin)
}
