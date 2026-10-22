//! Layered TOML configuration.
//!
//! Layers merge in order: built-in defaults → `~/.ch/config.toml` → `.ch/config.toml`
//! → `--profile <name>` (the `[profiles.<name>]` table) → `--set key=value` overrides.
//! Tables merge recursively; any other value (including arrays) is replaced.

use std::path::Path;

use anyhow::{Context as _, bail};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use toml::{Table, Value};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Config {
    root: Table,
}

impl Config {
    pub fn from_table(root: Table) -> Self {
        Self { root }
    }

    pub fn parse(text: &str) -> anyhow::Result<Self> {
        Ok(Self {
            root: text.parse::<Table>()?,
        })
    }

    pub fn as_table(&self) -> &Table {
        &self.root
    }

    /// Merges `overlay` on top of this config.
    pub fn merge(&mut self, overlay: Table) {
        merge_tables(&mut self.root, overlay);
    }

    /// Merges a TOML file if it exists. Returns whether it was found.
    pub fn merge_file(&mut self, path: &Path) -> anyhow::Result<bool> {
        if !path.exists() {
            return Ok(false);
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let table: Table = text
            .parse()
            .with_context(|| format!("parsing config {}", path.display()))?;
        self.merge(table);
        Ok(true)
    }

    /// Merges `[profiles.<name>]` over the root.
    pub fn apply_profile(&mut self, name: &str) -> anyhow::Result<()> {
        let Some(Value::Table(profile)) = self.get_value(&format!("profiles.{name}")).cloned()
        else {
            let known = match self.root.get("profiles") {
                Some(Value::Table(t)) => t.keys().cloned().collect::<Vec<_>>().join(", "),
                _ => String::new(),
            };
            bail!("unknown profile `{name}` (known profiles: {known})");
        };
        self.merge(profile);
        Ok(())
    }

    /// Applies a `--set a.b.c=value` override. The value is parsed as TOML (so `true`,
    /// `3`, `["x"]` work); anything that doesn't parse is taken as a string.
    pub fn set_override(&mut self, assignment: &str) -> anyhow::Result<()> {
        let Some((path, raw)) = assignment.split_once('=') else {
            bail!("override `{assignment}` must look like key.path=value");
        };
        let path = path.trim();
        let raw = raw.trim();
        let value = format!("v = {raw}")
            .parse::<Table>()
            .ok()
            .and_then(|mut t| t.remove("v"))
            .unwrap_or_else(|| Value::String(raw.to_string()));
        self.set_value(path, value)
    }

    pub fn set_value(&mut self, path: &str, value: Value) -> anyhow::Result<()> {
        let parts: Vec<&str> = path.split('.').collect();
        if parts.iter().any(|p| p.is_empty()) {
            bail!("invalid config path `{path}`");
        }
        let mut table = &mut self.root;
        for part in &parts[..parts.len() - 1] {
            let entry = table
                .entry(part.to_string())
                .or_insert_with(|| Value::Table(Table::new()));
            table = match entry {
                Value::Table(t) => t,
                _ => bail!("config path `{path}`: `{part}` is not a table"),
            };
        }
        table.insert(parts[parts.len() - 1].to_string(), value);
        Ok(())
    }

    pub fn get_value(&self, path: &str) -> Option<&Value> {
        let mut parts = path.split('.');
        let mut current = self.root.get(parts.next()?)?;
        for part in parts {
            current = current.as_table()?.get(part)?;
        }
        Some(current)
    }

    /// Deserializes the value at `path`, if present.
    pub fn get<T: DeserializeOwned>(&self, path: &str) -> anyhow::Result<Option<T>> {
        match self.get_value(path) {
            None => Ok(None),
            Some(v) => Ok(Some(
                v.clone()
                    .try_into()
                    .with_context(|| format!("invalid config at `{path}`"))?,
            )),
        }
    }

    /// Deserializes the table at `path`, or `T::default()` if it is missing.
    pub fn section<T: DeserializeOwned + Default>(&self, path: &str) -> anyhow::Result<T> {
        Ok(self.get(path)?.unwrap_or_default())
    }

    /// The `[plugins.<id>]` section.
    pub fn plugin_section<T: DeserializeOwned + Default>(&self, id: &str) -> anyhow::Result<T> {
        self.section(&format!("plugins.{id}"))
    }

    /// The kernel's own settings.
    pub fn kernel(&self) -> anyhow::Result<KernelConfig> {
        Value::Table(self.root.clone())
            .try_into()
            .context("invalid kernel config")
    }
}

fn merge_tables(base: &mut Table, overlay: Table) {
    for (key, value) in overlay {
        match (base.get_mut(&key), value) {
            (Some(Value::Table(b)), Value::Table(o)) => merge_tables(b, o),
            (_, v) => {
                base.insert(key, v);
            }
        }
    }
}

/// Settings the kernel itself reads. Unknown keys are ignored (they belong to plugins).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct KernelConfig {
    pub model: ModelConfig,
    pub agent: AgentConfig,
    pub plugins: PluginsConfig,
    pub select: SelectConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelConfig {
    /// Id of the `ModelProvider` plugin.
    pub provider: String,
    /// Model id passed to the provider.
    pub id: String,
    pub max_output_tokens: u32,
    pub temperature: Option<f32>,
    pub thinking_budget: Option<u32>,
    /// Overrides the provider's known window.
    pub context_window: Option<u64>,
    pub pricing: Option<Pricing>,
    pub summarizer: Option<SummarizerConfig>,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            provider: "anthropic".into(),
            id: "claude-opus-5-5".into(),
            max_output_tokens: 16_384,
            temperature: None,
            thinking_budget: None,
            context_window: None,
            pricing: None,
            summarizer: None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SummarizerConfig {
    /// Defaults to the agent's provider.
    pub provider: Option<String>,
    /// Defaults to the agent's model.
    pub id: Option<String>,
}

/// USD per million tokens.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Pricing {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

impl Pricing {
    pub fn cost(&self, usage: &crate::types::Usage) -> f64 {
        (usage.input_tokens as f64 * self.input
            + usage.output_tokens as f64 * self.output
            + usage.cache_read_tokens as f64 * self.cache_read
            + usage.cache_write_tokens as f64 * self.cache_write)
            / 1_000_000.0
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentConfig {
    /// Maximum model calls per user turn.
    pub max_steps: u32,
    /// Tool results longer than this are truncated (head and tail kept).
    pub tool_result_max_chars: usize,
    /// Read-only tool calls in one step run concurrently, up to this many at once.
    pub max_parallel_readonly: usize,
    /// Used when neither config nor the provider knows the model's window.
    pub default_context_window: u64,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_steps: 300,
            tool_result_max_chars: 24_000,
            max_parallel_readonly: 8,
            default_context_window: 200_000,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginsConfig {
    /// Plugins to enable in addition to those enabled by default.
    pub enable: Vec<String>,
    /// Plugins to disable (wins over `enable`).
    pub disable: Vec<String>,
}

/// Picks among several registered implementations of a single-slot extension point.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SelectConfig {
    pub context: Option<String>,
    pub executor: Option<String>,
    pub approver: Option<String>,
    pub frontend: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layers_merge_recursively() {
        let mut c = Config::parse(
            r#"
            [model]
            provider = "anthropic"
            id = "a"
            [plugins.context]
            strategy = "staged"
            soft = 0.6
            "#,
        )
        .unwrap();
        c.merge(
            r#"
            [model]
            id = "b"
            [plugins.context]
            strategy = "none"
            "#
            .parse()
            .unwrap(),
        );
        let k = c.kernel().unwrap();
        assert_eq!(k.model.provider, "anthropic");
        assert_eq!(k.model.id, "b");
        assert_eq!(
            c.get::<String>("plugins.context.strategy")
                .unwrap()
                .unwrap(),
            "none"
        );
        assert_eq!(c.get::<f64>("plugins.context.soft").unwrap().unwrap(), 0.6);
    }

    #[test]
    fn overrides_parse_toml_or_fall_back_to_string() {
        let mut c = Config::default();
        c.set_override("agent.max_steps=12").unwrap();
        c.set_override("plugins.context.strategy=elide").unwrap();
        c.set_override("plugins.disable=[\"tools-fs\"]").unwrap();
        let k = c.kernel().unwrap();
        assert_eq!(k.agent.max_steps, 12);
        assert_eq!(k.plugins.disable, vec!["tools-fs"]);
        assert_eq!(
            c.get::<String>("plugins.context.strategy")
                .unwrap()
                .unwrap(),
            "elide"
        );
        assert!(c.set_override("novalue").is_err());
    }

    #[test]
    fn profiles_apply_over_root() {
        let mut c = Config::parse(
            r#"
            [agent]
            max_steps = 10
            [profiles.fast.agent]
            max_steps = 3
            "#,
        )
        .unwrap();
        c.apply_profile("fast").unwrap();
        assert_eq!(c.kernel().unwrap().agent.max_steps, 3);
        assert!(c.apply_profile("missing").is_err());
    }

    #[test]
    fn defaults_when_empty() {
        let k = Config::default().kernel().unwrap();
        assert_eq!(k.agent.max_steps, 300);
        assert_eq!(k.agent.tool_result_max_chars, 24_000);
        assert_eq!(k.model.max_output_tokens, 16_384);
    }

    #[test]
    fn pricing_cost() {
        let p = Pricing {
            input: 3.0,
            output: 15.0,
            ..Default::default()
        };
        let u = crate::types::Usage {
            input_tokens: 1_000_000,
            output_tokens: 100_000,
            ..Default::default()
        };
        assert!((p.cost(&u) - 4.5).abs() < 1e-9);
    }
}
