//! Collects what plugins register and validates it.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context as _, bail};

use crate::approver::Approver;
use crate::command::Command;
use crate::config::Config;
use crate::context::{ContextStrategy, PassthroughContext};
use crate::events::EventSink;
use crate::executor::Executor;
use crate::frontend::Frontend;
use crate::hook::Hook;
use crate::model::ModelProvider;
use crate::plugin::{API_VERSION, Plugin, PluginManifest, RegisterCx};
use crate::prompt::PromptContributor;
use crate::services::Services;
use crate::tool::{Tool, ToolSpec};

type Tagged<T> = Vec<(String, Arc<T>)>;

/// Mutable state during registration. Use [`RegistryBuilder::build`].
#[derive(Default)]
pub struct RegistryBuilder {
    pub(crate) enabled: Vec<String>,
    pub(crate) providers: Tagged<dyn ModelProvider>,
    pub(crate) tools: Tagged<dyn Tool>,
    pub(crate) hooks: Tagged<dyn Hook>,
    pub(crate) prompts: Tagged<dyn PromptContributor>,
    pub(crate) contexts: Tagged<dyn ContextStrategy>,
    pub(crate) commands: Tagged<dyn Command>,
    pub(crate) sinks: Tagged<dyn EventSink>,
    pub(crate) executors: Tagged<dyn Executor>,
    pub(crate) approvers: Tagged<dyn Approver>,
    pub(crate) frontends: Tagged<dyn Frontend>,
    pub(crate) services: Services,
}

/// Why a plugin is or isn't active, for `ch --list-plugins`.
#[derive(Debug, Clone, PartialEq)]
pub struct PluginStatus {
    pub manifest: PluginManifest,
    pub enabled: bool,
    pub reason: String,
}

/// Everything registered by the enabled plugins. Immutable once built.
pub struct Registry {
    pub plugins: Vec<PluginStatus>,
    pub providers: Tagged<dyn ModelProvider>,
    pub tools: Tagged<dyn Tool>,
    /// Sorted by priority (stable).
    pub hooks: Tagged<dyn Hook>,
    /// Sorted by priority (stable).
    pub prompts: Tagged<dyn PromptContributor>,
    pub contexts: Tagged<dyn ContextStrategy>,
    pub commands: Tagged<dyn Command>,
    pub sinks: Tagged<dyn EventSink>,
    pub executors: Tagged<dyn Executor>,
    pub approvers: Tagged<dyn Approver>,
    pub frontends: Tagged<dyn Frontend>,
    pub services: Arc<Services>,
}

impl RegistryBuilder {
    /// Registers the enabled subset of `plugins` and validates the result.
    ///
    /// A plugin is enabled if it is `default_enabled` or listed in `plugins.enable`,
    /// and not listed in `plugins.disable`.
    pub fn build(
        plugins: Vec<Arc<dyn Plugin>>,
        config: &Config,
        workspace: &Path,
    ) -> anyhow::Result<Registry> {
        Self::build_with_services(plugins, config, workspace, Services::default())
    }

    /// Like [`RegistryBuilder::build`], with services the host provides before any plugin
    /// registers (for example the CLI handing options to a frontend). Plugins see them
    /// through `RegisterCx::service` and `Cx::services`.
    pub fn build_with_services(
        plugins: Vec<Arc<dyn Plugin>>,
        config: &Config,
        workspace: &Path,
        services: Services,
    ) -> anyhow::Result<Registry> {
        let kcfg = config.kernel()?;
        let manifests: Vec<PluginManifest> = plugins.iter().map(|p| p.manifest()).collect();

        let mut seen = HashSet::new();
        for m in &manifests {
            if !seen.insert(m.id.clone()) {
                bail!("two plugins have the id `{}`", m.id);
            }
        }
        for id in kcfg.plugins.enable.iter().chain(&kcfg.plugins.disable) {
            if !seen.contains(id) {
                bail!(
                    "config mentions unknown plugin `{id}` (known: {})",
                    manifests
                        .iter()
                        .map(|m| m.id.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
        }

        let mut statuses: Vec<PluginStatus> = manifests
            .iter()
            .map(|m| {
                let (enabled, reason) = if kcfg.plugins.disable.contains(&m.id) {
                    (false, "disabled in config".to_string())
                } else if kcfg.plugins.enable.contains(&m.id) {
                    (true, "enabled in config".to_string())
                } else if m.default_enabled {
                    (true, "enabled by default".to_string())
                } else {
                    (false, "off by default (add to plugins.enable)".to_string())
                };
                PluginStatus {
                    manifest: m.clone(),
                    enabled,
                    reason,
                }
            })
            .collect();

        for s in statuses.iter().filter(|s| s.enabled) {
            if s.manifest.api_version != API_VERSION {
                bail!(
                    "plugin `{}` targets plugin API v{}, but this ch supports v{API_VERSION}",
                    s.manifest.id,
                    s.manifest.api_version
                );
            }
        }
        let enabled: HashSet<String> = statuses
            .iter()
            .filter(|s| s.enabled)
            .map(|s| s.manifest.id.clone())
            .collect();
        for s in statuses.iter().filter(|s| s.enabled) {
            for req in &s.manifest.requires {
                if !enabled.contains(req) {
                    let why = if seen.contains(req) {
                        "which is disabled"
                    } else {
                        "which is not installed"
                    };
                    bail!(
                        "plugin `{}` requires `{req}`, {why}. Enable `{req}` or disable `{}`",
                        s.manifest.id,
                        s.manifest.id
                    );
                }
            }
        }

        let order = topo_order(&statuses)?;
        let by_id: HashMap<String, Arc<dyn Plugin>> = manifests
            .iter()
            .zip(plugins)
            .map(|(m, p)| (m.id.clone(), p))
            .collect();

        let mut builder = RegistryBuilder {
            enabled: order.clone(),
            services,
            ..Default::default()
        };
        for id in &order {
            let plugin = &by_id[id];
            let mut cx = RegisterCx {
                plugin_id: id.clone(),
                config,
                builder: &mut builder,
                workspace,
            };
            plugin
                .register(&mut cx)
                .with_context(|| format!("registering plugin `{id}`"))?;
        }

        // Keep the status list in registration order for display.
        statuses.sort_by_key(|s| {
            order
                .iter()
                .position(|o| o == &s.manifest.id)
                .unwrap_or(usize::MAX)
        });

        builder.finish(statuses)
    }

    fn finish(mut self, plugins: Vec<PluginStatus>) -> anyhow::Result<Registry> {
        // Commands that yield give way to any other command with the same name.
        let names: Vec<(String, bool)> = self
            .commands
            .iter()
            .map(|(_, c)| (c.name().to_string(), c.yields_on_conflict()))
            .collect();
        let mut i = 0;
        self.commands.retain(|(plugin, c)| {
            let (name, yields) = &names[i];
            i += 1;
            let clash = names
                .iter()
                .enumerate()
                .any(|(j, (n, y))| j != i - 1 && n == name && !*y);
            if *yields && clash {
                tracing::warn!(plugin = %plugin, command = %c.name(), "command hidden by another plugin's command of the same name");
                false
            } else {
                true
            }
        });
        check_unique("tool", self.tools.iter().map(|(p, t)| (p, t.spec().name)))?;
        check_unique(
            "command",
            self.commands.iter().map(|(p, c)| (p, c.name().to_string())),
        )?;
        check_unique(
            "model provider",
            self.providers.iter().map(|(p, m)| (p, m.id().to_string())),
        )?;
        check_unique(
            "context strategy",
            self.contexts.iter().map(|(p, c)| (p, c.id().to_string())),
        )?;
        check_unique(
            "executor",
            self.executors.iter().map(|(p, e)| (p, e.id().to_string())),
        )?;
        check_unique(
            "approver",
            self.approvers.iter().map(|(p, a)| (p, a.id().to_string())),
        )?;
        check_unique(
            "frontend",
            self.frontends.iter().map(|(p, f)| (p, f.id().to_string())),
        )?;
        for (plugin, tool) in &self.tools {
            let spec = tool.spec();
            if !spec.input_schema.is_object() {
                bail!(
                    "tool `{}` from `{plugin}` has a non-object input schema",
                    spec.name
                );
            }
        }
        // Stable sorts keep registration order for equal priorities.
        self.hooks.sort_by_key(|(_, h)| h.priority());
        self.prompts.sort_by_key(|(_, p)| p.priority());
        Ok(Registry {
            plugins,
            providers: self.providers,
            tools: self.tools,
            hooks: self.hooks,
            prompts: self.prompts,
            contexts: self.contexts,
            commands: self.commands,
            sinks: self.sinks,
            executors: self.executors,
            approvers: self.approvers,
            frontends: self.frontends,
            services: Arc::new(self.services),
        })
    }
}

fn check_unique<'a>(
    kind: &str,
    items: impl Iterator<Item = (&'a String, String)>,
) -> anyhow::Result<()> {
    let mut seen: HashMap<String, &String> = HashMap::new();
    for (plugin, name) in items {
        if let Some(other) = seen.insert(name.clone(), plugin) {
            bail!("{kind} `{name}` is registered by both `{other}` and `{plugin}`");
        }
    }
    Ok(())
}

/// Orders enabled plugins so that each comes after everything it requires, keeping the
/// given order otherwise.
fn topo_order(statuses: &[PluginStatus]) -> anyhow::Result<Vec<String>> {
    let enabled: Vec<&PluginManifest> = statuses
        .iter()
        .filter(|s| s.enabled)
        .map(|s| &s.manifest)
        .collect();
    let mut done: Vec<String> = Vec::new();
    let mut visiting: HashSet<String> = HashSet::new();

    fn visit(
        m: &PluginManifest,
        all: &[&PluginManifest],
        done: &mut Vec<String>,
        visiting: &mut HashSet<String>,
    ) -> anyhow::Result<()> {
        if done.contains(&m.id) {
            return Ok(());
        }
        if !visiting.insert(m.id.clone()) {
            bail!("plugin dependency cycle involving `{}`", m.id);
        }
        for req in &m.requires {
            if let Some(dep) = all.iter().find(|d| &d.id == req) {
                visit(dep, all, done, visiting)?;
            }
        }
        visiting.remove(&m.id);
        done.push(m.id.clone());
        Ok(())
    }

    for m in &enabled {
        visit(m, &enabled, &mut done, &mut visiting)?;
    }
    Ok(done)
}

fn pick<T: ?Sized>(
    kind: &str,
    items: &Tagged<T>,
    id_of: impl Fn(&T) -> &str,
    wanted: Option<&str>,
) -> anyhow::Result<Option<Arc<T>>> {
    match wanted {
        Some(w) => items
            .iter()
            .find(|(_, x)| id_of(x) == w)
            .map(|(_, x)| Some(x.clone()))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no {kind} named `{w}` (available: {})",
                    items
                        .iter()
                        .map(|(_, x)| id_of(x))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }),
        None => match items.len() {
            0 => Ok(None),
            1 => Ok(Some(items[0].1.clone())),
            _ => bail!(
                "several {kind}s are registered ({}); choose one with `select.{}`",
                items
                    .iter()
                    .map(|(_, x)| id_of(x))
                    .collect::<Vec<_>>()
                    .join(", "),
                kind.replace(' ', "_")
            ),
        },
    }
}

impl Registry {
    pub fn tool_specs(&self) -> Vec<ToolSpec> {
        self.tools.iter().map(|(_, t)| t.spec()).collect()
    }

    pub fn tool(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools
            .iter()
            .find(|(_, t)| t.spec().name == name)
            .map(|(_, t)| t.clone())
    }

    pub fn command(&self, name: &str) -> Option<Arc<dyn Command>> {
        self.commands
            .iter()
            .find(|(_, c)| c.name() == name)
            .map(|(_, c)| c.clone())
    }

    pub fn provider(&self, id: &str) -> anyhow::Result<Arc<dyn ModelProvider>> {
        self.providers
            .iter()
            .find(|(_, p)| p.id() == id)
            .map(|(_, p)| p.clone())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no model provider `{id}` (available: {}). Set model.provider",
                    self.providers
                        .iter()
                        .map(|(_, p)| p.id())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })
    }

    /// The context strategy; the passthrough strategy when no plugin provides one.
    pub fn context(&self, wanted: Option<&str>) -> anyhow::Result<Arc<dyn ContextStrategy>> {
        Ok(
            pick("context strategy", &self.contexts, |c| c.id(), wanted)?
                .unwrap_or_else(|| Arc::new(PassthroughContext)),
        )
    }

    pub fn executor(&self, wanted: Option<&str>) -> anyhow::Result<Option<Arc<dyn Executor>>> {
        pick("executor", &self.executors, |e| e.id(), wanted)
    }

    pub fn approver(&self, wanted: Option<&str>) -> anyhow::Result<Option<Arc<dyn Approver>>> {
        pick("approver", &self.approvers, |a| a.id(), wanted)
    }

    pub fn frontend(&self, wanted: Option<&str>) -> anyhow::Result<Arc<dyn Frontend>> {
        pick("frontend", &self.frontends, |f| f.id(), wanted)?
            .context("no frontend plugin is enabled")
    }

    pub fn enabled_plugin_ids(&self) -> Vec<String> {
        self.plugins
            .iter()
            .filter(|s| s.enabled)
            .map(|s| s.manifest.id.clone())
            .collect()
    }

    pub fn is_enabled(&self, id: &str) -> bool {
        self.plugins
            .iter()
            .any(|s| s.enabled && s.manifest.id == id)
    }
}
