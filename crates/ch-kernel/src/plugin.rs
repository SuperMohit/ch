//! The plugin trait and the registration context.

use std::any::Any;
use std::path::Path;
use std::sync::Arc;

use serde::de::DeserializeOwned;

use crate::approver::Approver;
use crate::command::Command;
use crate::config::Config;
use crate::context::ContextStrategy;
use crate::events::EventSink;
use crate::executor::Executor;
use crate::frontend::Frontend;
use crate::hook::Hook;
use crate::model::ModelProvider;
use crate::prompt::PromptContributor;
use crate::registry::RegistryBuilder;
use crate::tool::Tool;

/// Bumped on breaking changes to the plugin traits.
pub const API_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq)]
pub struct PluginManifest {
    /// Unique id, also the config section name (`[plugins.<id>]`).
    pub id: String,
    pub version: String,
    pub api_version: u32,
    pub description: String,
    /// Plugins that must be enabled (and are registered first).
    pub requires: Vec<String>,
    /// Free-form capability names, for `ch --list-plugins`.
    pub provides: Vec<String>,
    /// Enabled unless disabled in config. Plugins that need credentials (telemetry)
    /// default to off and are turned on with `plugins.enable`.
    pub default_enabled: bool,
    /// Keys of `[plugins.<id>]` a project's `.ch/config.toml` may set without the
    /// user's trust. List only settings that can't loosen a check, run a command, read
    /// a path, or choose an endpoint or a credential; every other key needs trust.
    pub project_settings: Vec<String>,
}

impl PluginManifest {
    pub fn new(id: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            version: "0.1.0".into(),
            api_version: API_VERSION,
            description: description.into(),
            requires: Vec::new(),
            provides: Vec::new(),
            default_enabled: true,
            project_settings: Vec::new(),
        }
    }

    pub fn version(mut self, version: impl Into<String>) -> Self {
        self.version = version.into();
        self
    }

    pub fn requires(mut self, ids: &[&str]) -> Self {
        self.requires = ids.iter().map(|s| s.to_string()).collect();
        self
    }

    pub fn provides(mut self, caps: &[&str]) -> Self {
        self.provides = caps.iter().map(|s| s.to_string()).collect();
        self
    }

    pub fn default_enabled(mut self, on: bool) -> Self {
        self.default_enabled = on;
        self
    }

    pub fn project_settings(mut self, keys: &[&str]) -> Self {
        self.project_settings = keys.iter().map(|s| s.to_string()).collect();
        self
    }
}

pub trait Plugin: Send + Sync {
    fn manifest(&self) -> PluginManifest;
    fn register(&self, cx: &mut RegisterCx<'_>) -> anyhow::Result<()>;
}

/// Handed to [`Plugin::register`]. Everything a plugin adds is tagged with its id.
pub struct RegisterCx<'a> {
    pub(crate) plugin_id: String,
    pub(crate) config: &'a Config,
    pub(crate) builder: &'a mut RegistryBuilder,
    pub(crate) workspace: &'a Path,
}

impl RegisterCx<'_> {
    pub fn plugin_id(&self) -> &str {
        &self.plugin_id
    }

    /// This plugin's `[plugins.<id>]` section, or `T::default()`.
    pub fn config<T: DeserializeOwned + Default>(&self) -> anyhow::Result<T> {
        self.config.plugin_section(&self.plugin_id)
    }

    /// The whole merged config.
    pub fn full_config(&self) -> &Config {
        self.config
    }

    pub fn workspace(&self) -> &Path {
        self.workspace
    }

    /// Whether another plugin is enabled in this run.
    pub fn plugin_enabled(&self, id: &str) -> bool {
        self.builder.enabled.iter().any(|e| e == id)
    }

    pub fn add_provider(&mut self, p: impl ModelProvider + 'static) {
        self.builder
            .providers
            .push((self.plugin_id.clone(), Arc::new(p)));
    }

    pub fn add_tool(&mut self, t: impl Tool + 'static) {
        self.builder
            .tools
            .push((self.plugin_id.clone(), Arc::new(t)));
    }

    pub fn add_tool_arc(&mut self, t: Arc<dyn Tool>) {
        self.builder.tools.push((self.plugin_id.clone(), t));
    }

    pub fn add_hook(&mut self, h: impl Hook + 'static) {
        self.builder
            .hooks
            .push((self.plugin_id.clone(), Arc::new(h)));
    }

    pub fn add_prompt(&mut self, p: impl PromptContributor + 'static) {
        self.builder
            .prompts
            .push((self.plugin_id.clone(), Arc::new(p)));
    }

    pub fn add_context(&mut self, c: impl ContextStrategy + 'static) {
        self.builder
            .contexts
            .push((self.plugin_id.clone(), Arc::new(c)));
    }

    pub fn add_command(&mut self, c: impl Command + 'static) {
        self.builder
            .commands
            .push((self.plugin_id.clone(), Arc::new(c)));
    }

    pub fn add_sink(&mut self, s: impl EventSink + 'static) {
        self.builder
            .sinks
            .push((self.plugin_id.clone(), Arc::new(s)));
    }

    pub fn add_executor(&mut self, e: impl Executor + 'static) {
        self.builder
            .executors
            .push((self.plugin_id.clone(), Arc::new(e)));
    }

    pub fn add_approver(&mut self, a: impl Approver + 'static) {
        self.builder
            .approvers
            .push((self.plugin_id.clone(), Arc::new(a)));
    }

    pub fn add_frontend(&mut self, f: impl Frontend + 'static) {
        self.builder
            .frontends
            .push((self.plugin_id.clone(), Arc::new(f)));
    }

    /// Shares typed state with other plugins. Errors if the type is already provided.
    pub fn provide_service<T: Any + Send + Sync>(&mut self, service: Arc<T>) -> anyhow::Result<()> {
        if !self.builder.services.provide(service) {
            anyhow::bail!(
                "plugin `{}`: service {} is already provided by another plugin",
                self.plugin_id,
                std::any::type_name::<T>()
            );
        }
        Ok(())
    }

    /// A service provided by a plugin registered earlier (list it in `requires`).
    pub fn service<T: Any + Send + Sync>(&self) -> Option<Arc<T>> {
        self.builder.services.get::<T>()
    }
}
