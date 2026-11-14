//! Running the matrix: every task, cell and repeat, `-j` at a time, each in a fresh
//! workspace. A run with a `run.json` is done, so running again with the same output
//! directory picks up where an interrupted matrix stopped, as long as nothing the runs
//! are made from changed (see [`crate::fingerprint`]).
//!
//! Before any run, each task's check runs once on its untouched repo: it must fail
//! there, and not because it couldn't run. Then one run, in
//! `<out>/runs/<task>/<cell>/<repeat>/`:
//! 1. `repo/` is copied to `work/` and committed as the baseline (`baseline.git`).
//! 2. `ch-home/config.toml` is the base config.
//! 3. The agent runs, with a token for the proxy; stdout is `events.jsonl`, stderr
//!    `stderr.log`. Usage and cost come from the proxy's ledger.
//! 4. The diff against the baseline is taken in a container, into `diff.patch`.
//! 5. `verify/` is copied over `work/` and the check runs; its output is `verify.log`.
//! 6. `run.json` records the run.
//!
//! However a run ends, every `.git` under `work/` and `ch-home/` is removed.
//!
//! What happens to a run is decided from what the runner and the proxy saw (container
//! exit codes, the ledger), never from the event stream, which the agent can write to;
//! the one exception is the plugin list in the stream's first event, written before the
//! agent's first command. A run whose model API failed transiently is tried again
//! later; one that says nothing about the agent stops the matrix (see [`classify`]).
//!
//! This module holds the matrix's setup (models, pricing, the proxy's settings) and
//! `describe`. The rest is split by what it decides:
//! - `schedule`: the preflight, the queue, `-j` workers and retries with backoff.
//! - `steps`: one run, from copying the repo to writing `run.json`.
//! - `verdict`: what a finished run means ([`classify`], [`reason_of`]).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, bail};
use ch_kernel::{Config, Pricing};

use crate::backend::{Backend, Job, Step};
use crate::ledger::TOKEN_ENV;
use crate::matrix::{Cell, Planned, ch_args, plan};
use crate::proxy::Provider;
use crate::record::{RUN_JSON, run_dir};
use crate::report::count;
use crate::task::Task;

mod schedule;
mod steps;
#[cfg(test)]
mod tests;
mod verdict;

pub use schedule::{backoff, run_matrix};
pub use verdict::{Verdict, classify, preflight_problem, reason_of};

/// What to run.
#[derive(Debug, Clone)]
pub struct Matrix {
    pub tasks: Vec<Task>,
    pub cells: Vec<Cell>,
    pub repeats: u32,
    /// `--profile`, passed to every run.
    pub profile: Option<String>,
    /// The base config's text, written to every run's `ch-home/config.toml`.
    pub config: Option<String>,
}

impl Matrix {
    /// Every run, in the order they start.
    pub fn planned(&self) -> Vec<Planned> {
        plan(self.tasks.len(), self.cells.len(), self.repeats)
    }

    /// `ch`'s arguments for a run, without the proxy's settings.
    pub fn ch_args(&self, p: &Planned) -> Vec<String> {
        ch_args(
            &self.cells[p.cell],
            self.tasks[p.task].max_steps,
            self.profile.as_deref(),
        )
    }

    /// Where a run's files go.
    pub fn run_dir(&self, out: &Path, p: &Planned) -> PathBuf {
        run_dir(
            out,
            &self.tasks[p.task].id,
            &self.cells[p.cell].name,
            p.repeat,
        )
    }

    fn is_done(&self, out: &Path, p: &Planned) -> bool {
        self.run_dir(out, p).join(RUN_JSON).is_file()
    }

    /// The base config with `--profile` applied, as `ch` reads it (a task's project
    /// config aside).
    pub fn base(&self) -> anyhow::Result<Config> {
        let mut config = Config::parse(self.config.as_deref().unwrap_or(""))?;
        if let Some(profile) = &self.profile {
            config.apply_profile(profile)?;
        }
        Ok(config)
    }

    /// A cell's config: the base with its settings applied.
    pub fn cell_config(&self, cell: usize) -> anyhow::Result<Config> {
        let mut config = self.base()?;
        for s in &self.cells[cell].settings {
            config
                .set_override(s)
                .with_context(|| format!("cell {}: --set {s}", self.cells[cell].name))?;
        }
        Ok(config)
    }

    /// What a cell's runs call through the proxy (see [`CellModels`]).
    pub fn cell_models(&self, cell: usize) -> anyhow::Result<CellModels> {
        let name = &self.cells[cell].name;
        let config = self.cell_config(cell)?;
        let kernel = config
            .kernel()
            .with_context(|| format!("cell {name}: reading the config"))?;
        let proxied = |p: &str| match p {
            "anthropic" => Some(Provider::Anthropic),
            "openai" => Some(Provider::OpenAi),
            _ => None,
        };
        let model = &kernel.model;
        // Unset, ch uses its first provider, Anthropic.
        let agent = model.provider.clone().unwrap_or_else(|| "anthropic".into());
        let mut out = CellModels::default();
        let base_model = self.base()?.kernel()?.model.id;
        let mut add = |provider: Provider, id: &str| -> anyhow::Result<()> {
            out.providers.insert(provider);
            let listed = listed_pricing(&config, id)
                .with_context(|| format!("cell {name}: [eval.pricing.\"{id}\"]"))?;
            // [model.pricing] is the configured model's, as ch prices it.
            let pricing = listed.or(model.pricing.filter(|_| model.id.as_deref() == Some(id)));
            if listed.is_none()
                && pricing.is_some()
                && model.id.as_deref() == Some(id)
                && base_model.as_deref() != Some(id)
            {
                out.warnings.push(format!(
                    "cell {name} uses model {id}, which has no [eval.pricing.\"{id}\"], so the \
                     base config's [model.pricing] prices it"
                ));
            }
            out.models.insert(id.to_string(), pricing);
            Ok(())
        };
        if let Some(p) = proxied(&agent) {
            let id = model.id.as_deref().with_context(|| {
                format!(
                    "cell {name}: set model.id; the proxy forwards only the models a cell \
                     configures, and doesn't know the provider's default"
                )
            })?;
            add(p, id)?;
        }
        if let Some(s) = &model.summarizer {
            let provider = s.provider.clone().unwrap_or_else(|| agent.clone());
            if let Some(p) = proxied(&provider) {
                // Without its own provider or id, the summarizer uses the agent's model.
                let id = match (&s.id, &s.provider) {
                    (Some(id), _) => id.clone(),
                    (None, None) => model.id.clone().unwrap_or_default(),
                    (None, Some(_)) => bail!(
                        "cell {name}: set model.summarizer.id; the proxy forwards only the \
                         models a cell configures"
                    ),
                };
                add(p, &id)?;
            }
        }
        Ok(out)
    }
}

/// What one cell's runs may call through the proxy.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CellModels {
    /// The model APIs its agent and summarizer use (none for the scripted provider).
    pub providers: BTreeSet<Provider>,
    /// Its agent and summarizer models, each with its pricing, if any.
    pub models: BTreeMap<String, Option<Pricing>>,
    /// Pricing that is likely not what was meant.
    pub warnings: Vec<String>,
}

/// `[eval.pricing."<model>"]` from the config (a top-level table `ch` ignores), if
/// there is one; a malformed one is an error.
fn listed_pricing(config: &Config, id: &str) -> anyhow::Result<Option<Pricing>> {
    let Some(table) = config.get_value("eval.pricing") else {
        return Ok(None);
    };
    let entry = table
        .as_table()
        .context("[eval.pricing] must be a table of models")?
        .get(id);
    match entry {
        None => Ok(None),
        Some(v) => Ok(Some(v.clone().try_into::<Pricing>().context(
            "expected input, output, cache_read and cache_write prices in USD per million tokens",
        )?)),
    }
}

/// The URL `ch` uses for a provider when the config doesn't set one (the provider
/// plugins' defaults).
pub fn default_base_url(provider: Provider) -> &'static str {
    match provider {
        Provider::Anthropic => "https://api.anthropic.com",
        Provider::OpenAi => "https://api.openai.com/v1",
    }
}

/// The variable `ch` reads a provider's key from when the config doesn't say.
pub fn default_key_env(provider: Provider) -> &'static str {
    match provider {
        Provider::Anthropic => "ANTHROPIC_API_KEY",
        Provider::OpenAi => "OPENAI_API_KEY",
    }
}

fn plugin_setting(config: &Config, provider: Provider, key: &str) -> Option<String> {
    let path = format!("plugins.provider-{}.{key}", provider.name());
    config.get::<String>(&path).ok().flatten()
}

/// The provider's upstream in `config`: its `base_url`, or the default.
pub fn upstream_url(config: &Config, provider: Provider) -> String {
    plugin_setting(config, provider, "base_url")
        .unwrap_or_else(|| default_base_url(provider).to_string())
}

/// The variable holding the provider's key in `config`.
pub fn key_env(config: &Config, provider: Provider) -> String {
    plugin_setting(config, provider, "api_key_env")
        .unwrap_or_else(|| default_key_env(provider).to_string())
}

/// `--set` flags that point `ch`'s providers at the proxy at `url`, with the run's token
/// as the key. Whether the upstream is the official API is passed on explicitly
/// (unless the config says), so requests carry what they would without the proxy.
pub fn proxy_settings(url: &str, base: &Config) -> Vec<String> {
    let quoted = |s: &str| toml::Value::String(s.to_string()).to_string();
    let mut out = Vec::new();
    for provider in [Provider::Anthropic, Provider::OpenAi] {
        let prefix = format!("plugins.provider-{}", provider.name());
        out.push(format!(
            "{prefix}.base_url={}",
            quoted(&format!("{url}/{}", provider.name()))
        ));
        out.push(format!("{prefix}.api_key_env={}", quoted(TOKEN_ENV)));
    }
    let anthropic = upstream_url(base, Provider::Anthropic);
    if base
        .get_value("plugins.provider-anthropic.official_api")
        .is_none()
    {
        let official = anthropic.contains("api.anthropic.com");
        out.push(format!(
            "plugins.provider-anthropic.official_api={official}"
        ));
    }
    let openai = upstream_url(base, Provider::OpenAi);
    if base
        .get_value("plugins.provider-openai.max_tokens_field")
        .is_none()
    {
        let field = if openai.contains("api.openai.com") {
            "max_completion_tokens"
        } else {
            "max_tokens"
        };
        out.push(format!(
            "plugins.provider-openai.max_tokens_field={}",
            quoted(field)
        ));
    }
    out
}

/// The config as data, keys sorted and comments gone, so a reformatted or commented
/// config fingerprints the same.
pub fn normalized(text: &str) -> anyhow::Result<String> {
    fn canonical(value: &toml::Value, out: &mut String) {
        match value {
            toml::Value::Table(t) => {
                let mut keys: Vec<&String> = t.keys().collect();
                keys.sort();
                out.push('{');
                for k in keys {
                    out.push_str(&format!("{k:?}:"));
                    canonical(&t[k], out);
                    out.push(',');
                }
                out.push('}');
            }
            toml::Value::Array(items) => {
                out.push('[');
                for item in items {
                    canonical(item, out);
                    out.push(',');
                }
                out.push(']');
            }
            other => out.push_str(&other.to_string()),
        }
    }
    let table: toml::Table = text.parse().context("parsing the base config")?;
    let mut out = String::new();
    canonical(&toml::Value::Table(table), &mut out);
    Ok(out)
}

/// How to run it.
#[derive(Debug, Clone)]
pub struct Options {
    /// The output directory, absolute.
    pub out: PathBuf,
    /// Runs at once.
    pub jobs: usize,
    /// Stop starting runs once the ledger's total (USD) reaches this. The proxy also
    /// refuses model calls past it.
    pub max_cost: Option<f64>,
    /// How many times a run is tried when the model API fails transiently; past that
    /// the matrix stops.
    pub max_attempts: u32,
    /// The wait before a run's first retry; it doubles with each of its attempts.
    pub backoff: Duration,
}

/// What a call to [`run_matrix`] did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Summary {
    pub planned: usize,
    /// Runs that had a `run.json` already.
    pub done_before: usize,
    pub ran: usize,
    /// Runs cut off or not started because `--max-cost` was reached.
    pub stopped_by_max_cost: usize,
    /// Runs tried again after a transient model API failure.
    pub retried: usize,
    /// Runs with a successful model answer that carried no usage.
    pub usage_missing: usize,
    /// Everything the ledger holds for this output directory, in USD.
    pub recorded_cost: f64,
}

/// The planned runs and the command lines of one, for `--dry-run`.
pub fn describe(m: &Matrix, backend: &dyn Backend, out: &Path) -> anyhow::Result<String> {
    let planned = m.planned();
    let done = planned.iter().filter(|p| m.is_done(out, p)).count();
    let mut text = format!(
        "{} × {} × {} = {}; {done} done already in {}.\n\n",
        count(m.tasks.len(), "task"),
        count(m.cells.len(), "cell"),
        count(m.repeats as usize, "repeat"),
        count(planned.len(), "run"),
        out.display()
    );
    let ids: Vec<&str> = m.tasks.iter().map(|t| t.id.as_str()).collect();
    text.push_str(&format!("Tasks: {}\n\nCells:\n", ids.join(", ")));
    let width = m.cells.iter().map(|c| c.name.len()).max().unwrap_or(0);
    for cell in &m.cells {
        let flags: Vec<String> = cell
            .settings
            .iter()
            .map(|s| shell_words::join(["--set", s]))
            .collect();
        text.push_str(&format!("  {:width$}  {}\n", cell.name, flags.join(" ")));
    }
    let first = planned
        .iter()
        .find(|p| !m.is_done(out, p))
        .or(planned.first());
    if let Some(p) = first {
        let task = &m.tasks[p.task];
        let dir = m.run_dir(out, p);
        let (work, ch_home, git_dir) =
            (dir.join("work"), dir.join("ch-home"), dir.join("diff.git"));
        let mut args = plumbing(m, backend)?;
        args.extend(m.ch_args(p));
        let env = [(TOKEN_ENV.to_string(), String::new())];
        let job = Job {
            id: "ch-eval-<id>-1",
            image: &task.image,
            work: &work,
            ch_home: &ch_home,
            git_dir: &git_dir,
            ch_args: &args,
            env: &env,
            verify: &task.verify,
        };
        text.push_str(&format!(
            "\nRun {} (task {}, cell {}, repeat {}):\n  {}\n\nIts check:\n  {}\n",
            planned.iter().position(|q| q == p).unwrap_or(0) + 1,
            task.id,
            m.cells[p.cell].name,
            p.repeat,
            shell_words::join(backend.command(Step::Agent, &job)),
            shell_words::join(backend.command(Step::Verify, &job)),
        ));
    }
    Ok(text)
}

/// `--set` flags with the proxy's settings, for every run, if the backend has a proxy.
fn plumbing(m: &Matrix, backend: &dyn Backend) -> anyhow::Result<Vec<String>> {
    Ok(match backend.proxy() {
        Some(route) => proxy_settings(&route.url, &m.base()?)
            .into_iter()
            .flat_map(|s| ["--set".to_string(), s])
            .collect(),
        None => Vec::new(),
    })
}
