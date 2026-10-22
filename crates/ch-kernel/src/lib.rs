//! The `ch` kernel.
//!
//! The kernel owns the agent loop, the event bus and the plugin registry. It has no
//! concrete model providers, tools, prompts or UI: those are all plugins that implement
//! the traits in this crate and register themselves through [`RegisterCx`].

pub mod approver;
pub mod command;
pub mod config;
pub mod context;
pub mod cx;
pub mod events;
pub mod executor;
pub mod frontend;
pub mod history;
pub mod hook;
pub mod model;
pub mod plugin;
pub mod prompt;
pub mod registry;
pub mod services;
pub mod session;
pub mod store;
pub mod tokens;
pub mod tool;
pub mod types;
pub mod util;

pub use approver::{Approval, ApprovalRequest, Approver};
pub use command::{CmdCx, Command, Outcome};
pub use config::{Config, KernelConfig, ModelConfig, Pricing};
pub use context::{Budget, ContextCx, ContextStrategy, PassthroughContext, View};
pub use cx::Cx;
pub use events::{AgentEvent, CompactionKind, EventBus, EventSink};
pub use executor::{ExecRequest, ExecResult, Executor};
pub use frontend::Frontend;
pub use history::{Event, EventId, History};
pub use hook::{Hook, HookDecision, HookEvent, HookOutcome, HookPoint, Verdict, run_hooks};
pub use model::{
    ModelError, ModelEvent, ModelProvider, ModelRequest, ModelResponse, ModelStream, complete,
};
pub use plugin::{API_VERSION, Plugin, PluginManifest, RegisterCx};
pub use prompt::PromptContributor;
pub use registry::{PluginStatus, Registry, RegistryBuilder};
pub use services::Services;
pub use session::{CancelHandle, Session, SessionBuilder, TurnResult};
pub use store::{SessionMeta, SessionSnapshot, SessionStore};
pub use tokens::{TokenCalibrator, estimate_tokens};
pub use tool::{Tool, ToolOutput, ToolSpec};
pub use types::{ContentBlock, Message, Role, StopReason, TerminationReason, ToolCall, Usage};

/// Re-exports plugin crates commonly need, so they agree on versions.
pub mod reexports {
    pub use async_trait::async_trait;
    pub use futures;
    pub use serde_json;
    pub use tokio_util::sync::CancellationToken;
}
