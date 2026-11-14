//! A project's MCP servers: finding them, asking the user to trust each one, and
//! handing them to the `mcp-bridge` plugin (ADR 0006 §8).
//!
//! Servers from `.mcp.json` or the project's `.ch/config.toml` start only once trusted.
//! Trust is recorded per server, keyed by workspace, file and name, for the hash of the
//! whole entry, so any change asks again. `--trust-project-servers` starts them for one
//! run without asking; `-p` never asks.

use std::io::Write as _;
use std::sync::Arc;

use anyhow::Context as _;
use ch_kernel::Services;
use ch_mcp_bridge::launch::{describe, trust_hash};
use ch_mcp_bridge::{Places, ProjectServers, ServerConfig, TrustFn};

use crate::config::Paths;
use crate::trust::TrustStore;

/// Finds the project's servers, settles which are trusted (asking when `interactive`),
/// and provides them to the plugin as a service.
pub fn provide_project_servers(
    paths: &Paths,
    config: &ch_kernel::Config,
    project_servers: Option<&toml::Value>,
    interactive: bool,
    trust_all: bool,
    services: &mut Services,
) -> anyhow::Result<()> {
    let user_names = ch_mcp_bridge::user_server_names(config);
    let found = ch_mcp_bridge::discover_project(&paths.workspace, project_servers, &user_names);
    for warning in &found.warnings {
        eprintln!("ch: {warning}");
    }
    if found.servers.is_empty() {
        return Ok(());
    }
    let places = Places::new(&paths.workspace);
    let store = TrustStore::new(&paths.home);
    let mut servers = Vec::new();
    let mut waiting = Vec::new();
    for server in found.servers {
        let hash = trust_hash(&server, &places);
        let saved =
            store.is_server_trusted(&paths.workspace, &server.origin.file, &server.name, &hash);
        let mut trusted = saved || trust_all;
        if !trusted && interactive && confirm(&server, &places)? {
            store.trust_server(&paths.workspace, &server.origin.file, &server.name, &hash)?;
            trusted = true;
        }
        if !trusted {
            waiting.push(server.name.clone());
        }
        servers.push((server, trusted));
    }
    if !waiting.is_empty() && !interactive {
        eprintln!(
            "ch: not starting the project's MCP servers {}: each needs your trust. Run `ch` \
             in a terminal to review them, type `/mcp trust <name>` in a session, or pass \
             --trust-project-servers.",
            waiting
                .iter()
                .map(|n| format!("`{n}`"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    let workspace = paths.workspace.clone();
    let save: Box<TrustFn> = Box::new(move |s: &ServerConfig, hash: &str| {
        store.trust_server(&workspace, &s.origin.file, &s.name, hash)
    });
    services.provide(Arc::new(ProjectServers {
        servers,
        trust: Some(save),
    }));
    Ok(())
}

/// Shows what `server` runs on stderr and asks whether to trust it.
fn confirm(server: &ServerConfig, places: &Places) -> anyhow::Result<bool> {
    let mut err = std::io::stderr();
    let _ = writeln!(
        err,
        "ch: this project wants to start the MCP server `{}`:",
        server.name
    );
    for (label, value) in describe(server, places) {
        let _ = writeln!(err, "  {label}: {value}");
    }
    let _ = write!(
        err,
        "It runs with your permissions, and its tools go to the model.\n\
         Trust it for this project? [y/N]: "
    );
    let _ = err.flush();
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .context("reading the answer")?;
    let yes = matches!(answer.trim(), "y" | "Y" | "yes" | "Yes");
    if yes {
        let _ = writeln!(
            err,
            "ch: trusted `{}`. You'll be asked again if its entry changes.",
            server.name
        );
    }
    Ok(yes)
}
