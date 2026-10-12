//! The runtime context passed to tools, hooks, prompt contributors and context strategies.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::events::{AgentEvent, EventBus};
use crate::executor::Executor;
use crate::services::Services;
use crate::tool::ToolSpec;

/// Cheap to clone; everything is behind `Arc`.
#[derive(Clone)]
pub struct Cx {
    pub session_id: Arc<str>,
    /// Workspace root. Tools must not touch anything outside it.
    pub workspace: Arc<PathBuf>,
    pub services: Arc<Services>,
    pub events: EventBus,
    pub config: Arc<Config>,
    pub executor: Option<Arc<dyn Executor>>,
    /// Specs of every registered tool (what the model sees).
    pub tools: Arc<Vec<ToolSpec>>,
    /// Current user turn (1-based; 0 before the first turn).
    pub turn: u32,
    /// Model call index within the turn (0-based).
    pub step: u32,
    /// Cancelled when the user interrupts the current turn.
    pub cancel: CancellationToken,
    /// Set while a tool call is being handled.
    pub tool_use_id: Option<String>,
    /// Directory for this session's files (elided outputs, etc.), if persistence is on.
    pub session_dir: Option<Arc<PathBuf>>,
}

impl Cx {
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    pub fn emit(&self, event: AgentEvent) {
        self.events.emit(event);
    }

    /// Emits a plugin-specific event (plan updates, diagnostics, ...).
    pub fn emit_custom(&self, source: &str, name: &str, data: serde_json::Value) {
        self.events.emit(AgentEvent::Custom {
            source: source.to_string(),
            name: name.to_string(),
            data,
        });
    }

    pub fn has_tool(&self, name: &str) -> bool {
        self.tools.iter().any(|t| t.name == name)
    }

    /// A context for tests and offline use: temp-free, no executor, no tools.
    pub fn detached(workspace: impl Into<PathBuf>) -> Self {
        Self {
            session_id: Arc::from("detached"),
            workspace: Arc::new(workspace.into()),
            services: Arc::new(Services::default()),
            events: EventBus::new(),
            config: Arc::new(Config::default()),
            executor: None,
            tools: Arc::new(Vec::new()),
            turn: 0,
            step: 0,
            cancel: CancellationToken::new(),
            tool_use_id: None,
            session_dir: None,
        }
    }
}
