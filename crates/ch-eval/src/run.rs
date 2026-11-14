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

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context as _, anyhow, bail};
use ch_kernel::{Config, Pricing};
use futures::StreamExt;
use tokio::sync::Notify;

use crate::backend::{Backend, Exit, Io, Job, Sink, Step, Stopping};
use crate::events;
use crate::fingerprint::{Fingerprint, hash, hash_file, hash_tree};
use crate::ledger::{self, RunCalls, TOKEN_ENV};
use crate::matrix::{Cell, Planned, ch_args, plan};
use crate::proxy::Provider;
use crate::record::{NO_RESULT, RUN_JSON, Reported, RunRecord, TIMEOUT, VerifyRecord, run_dir};
use crate::report::count;
use crate::task::Task;
use crate::workspace::{Scrub, commit_baseline, copy_dir, copy_repo, overlay, remove_dir};

/// Wall time for the diff.
const DIFF_LIMIT: Duration = Duration::from_secs(120);
/// How much of a self-reported detail is kept.
const MAX_DETAIL: usize = 500;
/// How much of each output is kept. Past it, the rest is dropped with a note.
const EVENTS_CAP: u64 = events::MAX_TOTAL;
const LOG_CAP: u64 = 16 * 1024 * 1024;
const DIFF_CAP: u64 = 64 * 1024 * 1024;
/// The longest wait before a run whose model API failed transiently is tried again.
const MAX_BACKOFF: Duration = Duration::from_secs(15 * 60);

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

/// The task's own hashes, for fingerprints.
struct TaskHashes {
    task: String,
    repo: String,
    verify: String,
}

/// What every run of an invocation shares.
struct Ctx {
    m: Arc<Matrix>,
    backend: Arc<dyn Backend>,
    out: PathBuf,
    /// Names this invocation's runs in the ledger and in container names.
    invocation: String,
    plumbing: Vec<String>,
    /// What each cell's runs may call.
    cells: Vec<CellModels>,
    config_hash: String,
    tasks: Vec<TaskHashes>,
    /// The backend's identity for each task image, worked out once.
    identities: BTreeMap<String, String>,
}

impl Ctx {
    fn fingerprint(&self, p: &Planned) -> Fingerprint {
        let task = &self.tasks[p.task];
        Fingerprint {
            ch: self
                .identities
                .get(&self.m.tasks[p.task].image)
                .cloned()
                .unwrap_or_default(),
            config: self.config_hash.clone(),
            settings: hash(self.m.ch_args(p).join("\n").as_bytes()),
            task: task.task.clone(),
            repo: task.repo.clone(),
            verify: task.verify.clone(),
        }
    }
}

/// A run waiting to start, maybe not before some time, and how often it was tried.
#[derive(Debug, Clone, Copy)]
struct Queued {
    planned: Planned,
    not_before: Option<Instant>,
    attempts: u32,
}

/// The wait before a run's next try, after `attempts` transient failures: `base`,
/// doubling with each, at most [`MAX_BACKOFF`].
pub fn backoff(base: Duration, attempts: u32) -> Duration {
    let factor = 2u32.saturating_pow(attempts.saturating_sub(1));
    base.saturating_mul(factor).min(MAX_BACKOFF)
}

/// Shared by the workers.
struct State {
    queue: VecDeque<Queued>,
    to_run: usize,
    started: usize,
    finished: usize,
    spent: f64,
    capped: bool,
    cut: usize,
    retried: usize,
    usage_missing: usize,
    error: Option<anyhow::Error>,
}

/// Runs every planned run that isn't done, `opts.jobs` at a time, after checking the
/// done runs' fingerprints and pre-flighting the tasks. A run that fails for a reason
/// other than the agent stops the matrix: no new runs start, the ones going finish, and
/// the error is returned.
pub async fn run_matrix(
    m: Arc<Matrix>,
    backend: Arc<dyn Backend>,
    opts: &Options,
) -> anyhow::Result<Summary> {
    let planned = m.planned();
    let mut tasks = Vec::new();
    for task in &m.tasks {
        tasks.push(TaskHashes {
            task: hash_file(&task.dir.join("task.toml"))?,
            repo: hash_tree(&task.repo())?,
            verify: hash_tree(&task.verify_dir())?,
        });
    }
    let cells = (0..m.cells.len())
        .map(|c| m.cell_models(c))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let identities = m
        .tasks
        .iter()
        .map(|t| (t.image.clone(), backend.identity(&t.image)))
        .collect();
    let ctx = Arc::new(Ctx {
        plumbing: plumbing(&m, backend.as_ref())?,
        m: m.clone(),
        backend: backend.clone(),
        out: opts.out.clone(),
        invocation: uuid::Uuid::new_v4().simple().to_string()[..12].to_string(),
        cells,
        config_hash: hash(normalized(m.config.as_deref().unwrap_or(""))?.as_bytes()),
        tasks,
        identities,
    });

    check_fingerprints(&ctx, &planned)?;
    // A run without a record may have been cut off by a killed ch-eval, before its
    // `.git` entries were removed.
    for p in planned.iter().filter(|p| !m.is_done(&opts.out, p)) {
        let dir = m.run_dir(&opts.out, p);
        if dir.exists() {
            Scrub::new(vec![dir.join("work"), dir.join("ch-home")]).finish()?;
        }
    }
    let queue: VecDeque<Queued> = planned
        .iter()
        .filter(|p| !m.is_done(&opts.out, p))
        .map(|&planned| Queued {
            planned,
            not_before: None,
            attempts: 0,
        })
        .collect();
    let to_run = queue.len();
    let mut pending_tasks: Vec<usize> = queue.iter().map(|q| q.planned.task).collect();
    pending_tasks.sort_unstable();
    pending_tasks.dedup();
    preflight(&ctx, &pending_tasks, opts.jobs).await?;

    let recorded = match backend.proxy() {
        Some(route) => ledger::total_cost(&route.state),
        None => 0.0,
    };
    if to_run == 0 {
        eprintln!(
            "ch-eval: all {} planned are done already",
            count(planned.len(), "run")
        );
    } else {
        eprintln!(
            "ch-eval: {} planned, {} done already, {to_run} to run ({} at a time)",
            count(planned.len(), "run"),
            planned.len() - to_run,
            opts.jobs.clamp(1, to_run)
        );
    }
    let state = Arc::new(Mutex::new(State {
        queue,
        to_run,
        started: 0,
        finished: 0,
        spent: recorded,
        capped: false,
        cut: 0,
        retried: 0,
        usage_missing: 0,
        error: None,
    }));
    let wake = Arc::new(Notify::new());
    let mut workers = Vec::new();
    for _ in 0..opts.jobs.clamp(1, to_run.max(1)) {
        let (ctx, state, opts, wake) = (ctx.clone(), state.clone(), opts.clone(), wake.clone());
        workers.push(tokio::spawn(async move {
            worker(&ctx, &state, &opts, &wake).await
        }));
    }
    // Every worker is awaited, so no run is left going when this returns.
    let panicked = futures::future::join_all(workers)
        .await
        .iter()
        .any(Result::is_err);
    let mut state = lock(&state);
    if let Some(e) = state.error.take() {
        return Err(e);
    }
    if panicked {
        bail!("a run panicked; its containers are removed with the rest");
    }
    let stopped = if state.capped {
        state.queue.len() + state.cut
    } else {
        0
    };
    Ok(Summary {
        planned: planned.len(),
        done_before: planned.len() - to_run,
        ran: state.finished,
        stopped_by_max_cost: stopped,
        retried: state.retried,
        usage_missing: state.usage_missing,
        recorded_cost: match backend.proxy() {
            Some(route) => ledger::total_cost(&route.state),
            None => state.spent,
        },
    })
}

fn lock(state: &Mutex<State>) -> std::sync::MutexGuard<'_, State> {
    state.lock().unwrap_or_else(|e| e.into_inner())
}

/// Refuses to add runs to a matrix whose recorded runs were made from something else.
fn check_fingerprints(ctx: &Ctx, planned: &[Planned]) -> anyhow::Result<()> {
    let mut mismatches = Vec::new();
    for p in planned.iter().filter(|p| ctx.m.is_done(&ctx.out, p)) {
        let dir = ctx.m.run_dir(&ctx.out, p);
        let text = std::fs::read_to_string(dir.join(RUN_JSON))
            .with_context(|| format!("reading {}", dir.join(RUN_JSON).display()))?;
        let record: RunRecord = serde_json::from_str(&text)
            .with_context(|| format!("parsing {}", dir.join(RUN_JSON).display()))?;
        let differs = ctx.fingerprint(p).differences(&record.fingerprint);
        if !differs.is_empty() {
            let rel = dir.strip_prefix(&ctx.out).unwrap_or(&dir);
            mismatches.push(format!("{} ({})", rel.display(), differs.join(", ")));
        }
    }
    if !mismatches.is_empty() {
        let shown: Vec<&str> = mismatches.iter().take(5).map(String::as_str).collect();
        bail!(
            "{} in {} were made from something else than this run would be: {}{}. Use \
             another --out, or remove those runs to make them again.",
            count(mismatches.len(), "recorded run"),
            ctx.out.display(),
            shown.join("; "),
            if mismatches.len() > 5 { "; ..." } else { "" }
        );
    }
    Ok(())
}

/// Runs each task's check on its untouched repo, `jobs` at a time. It must fail, and
/// not because it couldn't run (124–127, a timeout). This happens before any model
/// call, so a broken task costs nothing.
async fn preflight(ctx: &Ctx, tasks: &[usize], jobs: usize) -> anyhow::Result<()> {
    if tasks.is_empty() {
        return Ok(());
    }
    let results: Vec<anyhow::Result<()>> = futures::stream::iter(tasks.iter().copied())
        .map(|t| preflight_one(ctx, t))
        .buffer_unordered(jobs.max(1))
        .collect()
        .await;
    results.into_iter().collect::<anyhow::Result<Vec<()>>>()?;
    let dir = ctx.out.join("preflight");
    let _ = std::fs::remove_dir(&dir);
    Ok(())
}

/// Why a pre-flight's exit disqualifies the task's check, if it does.
pub fn preflight_problem(exit: &Exit) -> Option<String> {
    if let Some(e) = &exit.start_error {
        return Some(format!("its check's container didn't start: {e}"));
    }
    if exit.timed_out {
        return Some("its check timed out on the starting repo".into());
    }
    match exit.code {
        Some(0) => {
            Some("its check passes on the starting repo, so it can't tell a fix from no fix".into())
        }
        Some(c @ 124..=127) => Some(format!(
            "its check couldn't run on the starting repo (exit {c})"
        )),
        None => Some("its check was killed by a signal".into()),
        _ => None,
    }
}

async fn preflight_one(ctx: &Ctx, t: usize) -> anyhow::Result<()> {
    let task = &ctx.m.tasks[t];
    let dir = ctx.out.join("preflight").join(&task.id);
    if dir.exists() {
        remove_dir(&dir)?;
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let work = dir.join("work");
    let scrub = Scrub::new(vec![work.clone()]);
    copy_repo(&task.repo(), &work)?;
    if task.verify_dir().is_dir() {
        overlay(&task.verify_dir(), &work)?;
    }
    let id = format!("ch-eval-{}-pre{t}", ctx.invocation);
    let job = Job {
        id: &id,
        image: &task.image,
        work: &work,
        ch_home: &dir,
        git_dir: &dir,
        ch_args: &[],
        env: &[],
        verify: &task.verify,
    };
    let log_path = dir.join("verify.log");
    let exit = ctx
        .backend
        .run(
            Step::Verify,
            &job,
            shared_log(&log_path)?,
            task.verify_timeout,
        )
        .await?;
    scrub.finish()?;
    if let Some(problem) = preflight_problem(&exit) {
        bail!(
            "task {}: {problem}. Nothing was run. See {}",
            task.id,
            log_path.display()
        );
    }
    remove_dir(&dir)
}

/// What a worker does next.
enum Next {
    Run(Queued, usize),
    Wait(Instant),
    Stop,
}

/// Takes runs from the queue until it is empty or the matrix stops. A worker waiting
/// out a run's backoff wakes when another worker changes the state.
async fn worker(ctx: &Ctx, state: &Mutex<State>, opts: &Options, wake: &Notify) {
    loop {
        let next = {
            let mut s = lock(state);
            if s.error.is_some() || s.capped {
                Next::Stop
            } else if opts.max_cost.is_some_and(|max| s.spent >= max) {
                s.capped = true;
                Next::Stop
            } else {
                let now = Instant::now();
                match s
                    .queue
                    .iter()
                    .position(|q| q.not_before.is_none_or(|t| t <= now))
                {
                    Some(i) => match s.queue.remove(i) {
                        Some(q) => {
                            s.started += 1;
                            Next::Run(q, s.started)
                        }
                        None => Next::Stop,
                    },
                    None => match s.queue.iter().filter_map(|q| q.not_before).min() {
                        Some(t) => Next::Wait(t),
                        None => Next::Stop,
                    },
                }
            }
        };
        let (queued, seq) = match next {
            Next::Stop => {
                wake.notify_waiters();
                return;
            }
            Next::Wait(t) => {
                tokio::select! {
                    () = tokio::time::sleep_until(t.into()) => {}
                    () = wake.notified() => {}
                }
                continue;
            }
            Next::Run(q, seq) => (q, seq),
        };
        let planned = queued.planned;
        let result = run_one(ctx, &planned, seq).await;
        // Whatever happened, waiting workers look again.
        let _notify = NotifyOnDrop(wake);
        let mut s = lock(state);
        let label = format!(
            "{} {} #{}",
            ctx.m.tasks[planned.task].id, ctx.m.cells[planned.cell].name, planned.repeat
        );
        match result {
            Ok(Outcome::Recorded(record, calls)) => {
                s.spent = (s.spent + record.cost).min(f64::MAX);
                s.finished += 1;
                if calls.usage_missing {
                    s.usage_missing += 1;
                }
                if let Some(model) = &calls.unlisted_model {
                    eprintln!(
                        "ch-eval: {label}: model `{model}` answered, which the cell doesn't \
                         use; its calls are priced as the model asked for"
                    );
                }
                if let Some(model) = &calls.refused_model {
                    eprintln!(
                        "ch-eval: {label}: the proxy refused a call for model `{model}`, \
                         which the cell doesn't use; the agent made it itself"
                    );
                }
                eprintln!(
                    "[{}/{}] {label}: {} ({}, {} steps, ${:.4}, {:.0}s)",
                    s.finished,
                    s.to_run,
                    if record.passed { "pass" } else { "FAIL" },
                    record.reason,
                    record.reported.steps,
                    record.cost,
                    record.wall_ms as f64 / 1000.0
                );
            }
            Ok(Outcome::CutByMaxCost) => {
                eprintln!("ch-eval: {label}: cut off at --max-cost; it will run again on resume");
                s.capped = true;
                s.cut += 1;
            }
            Ok(Outcome::Transient(why)) => {
                let attempts = queued.attempts + 1;
                if attempts >= opts.max_attempts {
                    s.error.get_or_insert(anyhow!(
                        "run {label}: {why}, on all {} of it, so the matrix stops. Runs \
                         without a run.json are made again on resume.",
                        count(attempts as usize, "attempt")
                    ));
                } else {
                    let wait = backoff(opts.backoff, attempts);
                    eprintln!(
                        "ch-eval: {label}: {why}; trying it again in {:.0}s",
                        wait.as_secs_f64()
                    );
                    s.retried += 1;
                    s.queue.push_front(Queued {
                        planned,
                        not_before: Some(Instant::now() + wait),
                        attempts,
                    });
                }
            }
            Err(e) if e.is::<Stopping>() => {
                if s.error.is_none() {
                    s.error = Some(e);
                }
            }
            Err(e) => {
                eprintln!("ch-eval: {label}: {e:#}");
                if s.error.is_none() {
                    s.error = Some(e.context(format!("run {label}")));
                }
            }
        }
    }
}

/// Wakes the waiting workers when dropped.
struct NotifyOnDrop<'a>(&'a Notify);

impl Drop for NotifyOnDrop<'_> {
    fn drop(&mut self) {
        self.0.notify_waiters();
    }
}

/// How a run ended, for the runner.
#[derive(Debug)]
enum Outcome {
    Recorded(Box<RunRecord>, RunCalls),
    CutByMaxCost,
    Transient(String),
}

/// What to do with a run once the agent is done.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// It measured the agent: check and record it.
    Record,
    /// `--max-cost` cut it off: no record, so it runs again on resume.
    CutByMaxCost,
    /// The model API failed in a way that may pass: no record; try it again later.
    Transient(String),
    /// It says nothing about the agent, and trying again won't help: stop the matrix.
    Infra(String),
}

/// Decides what a finished agent step means, from what the runner and the proxy saw:
/// the container's exit, and the ledger's `calls` (none without a proxy). `proxied`
/// says whether the cell's model goes through the proxy. `plugins` is the first
/// `session_start` event's list, written before the agent's first command.
///
/// `ch` ended normally when it exited with a code of its own other than 1 (done, step
/// limit, context overflow, a hook stop, a denied prompt, cancelled), or was killed at
/// the timeout. A run through the proxy with no successful model call says nothing about
/// the agent, however it ended.
pub fn classify(
    exit: &Exit,
    calls: Option<&RunCalls>,
    proxied: bool,
    plugins: Option<&[String]>,
    cell: &Cell,
) -> Verdict {
    if let Some(e) = &exit.start_error {
        return Verdict::Infra(format!("the agent's container didn't start: {e}"));
    }
    let ended_normally = exit.timed_out || matches!(exit.code, Some(0 | 2 | 3 | 4 | 5 | 130));
    if let Some(calls) = calls {
        let last = || {
            let status = match calls.last_status {
                Some(0) | None => "no answer".to_string(),
                Some(s) => format!("HTTP {s}"),
            };
            match &calls.last_error {
                Some(message) => format!("{status}: {message}"),
                None => status,
            }
        };
        if calls.cut_by_max_cost {
            return Verdict::CutByMaxCost;
        }
        if calls.auth_failed {
            return Verdict::Infra(format!(
                "the model API refused the key ({}); check the key the proxy was given",
                last()
            ));
        }
        if !ended_normally && calls.last_transient {
            return Verdict::Transient(format!("the model API failed (last answer: {})", last()));
        }
        if proxied && calls.succeeded == 0 {
            return Verdict::Infra(if calls.forwarded == 0 {
                format!(
                    "the run ended ({}) before its first model call",
                    code_text(exit.code)
                )
            } else {
                format!(
                    "no model call of the run succeeded (last answer: {}); check the \
                     model, the account and the request size",
                    last()
                )
            });
        }
    }
    match plugins.map(|p| cell.check_plugins(p)) {
        Some(Err(e)) => Verdict::Infra(e),
        _ => Verdict::Record,
    }
}

/// The termination reason and its detail: `timeout` when the agent was killed (even if
/// it printed a result), else the result line's, else `no_result`. Self-reported, so
/// only recorded.
pub fn reason_of(
    exit: &Exit,
    stream: &events::Stream,
    timeout: Duration,
) -> (String, Option<String>) {
    if exit.timed_out {
        return (
            TIMEOUT.to_string(),
            Some(format!("killed after {}s", timeout.as_secs())),
        );
    }
    match stream.reason() {
        Some(reason) => {
            let detail = stream
                .result
                .as_ref()
                .and_then(|r| r.detail.as_deref())
                .map(|d| d.chars().take(MAX_DETAIL).collect());
            (reason.to_string(), detail)
        }
        None => (
            NO_RESULT.to_string(),
            Some(format!(
                "ch exited with {} and no result line; see stderr.log",
                code_text(exit.code)
            )),
        ),
    }
}

/// Does one run and, if it measured the agent, saves its `run.json`.
async fn run_one(ctx: &Ctx, p: &Planned, seq: usize) -> anyhow::Result<Outcome> {
    let m = &ctx.m;
    let task = &m.tasks[p.task];
    let cell = &m.cells[p.cell];
    let dir = m.run_dir(&ctx.out, p);
    if dir.exists() {
        // An earlier attempt that stopped before its record: start it over.
        remove_dir(&dir)?;
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let work = dir.join("work");
    let ch_home = dir.join("ch-home");
    // From here on, however the run ends, no `.git` is left where the agent wrote.
    let scrub = Scrub::new(vec![work.clone(), ch_home.clone()]);
    copy_repo(&task.repo(), &work)?;
    let baseline = dir.join("baseline.git");
    commit_baseline(&work, &baseline)?;
    std::fs::create_dir(&ch_home).with_context(|| format!("creating {}", ch_home.display()))?;
    if let Some(config) = &m.config {
        write(&ch_home.join("config.toml"), config)?;
    }

    let attempt = format!("{}-{seq}", ctx.invocation);
    let id = format!("ch-eval-{attempt}");
    let proxy = ctx.backend.proxy();
    let models = &ctx.cells[p.cell];
    let token = match proxy {
        Some(route) => Some(ledger::issue_token(
            &route.state,
            &attempt,
            models.models.clone(),
        )?),
        None => None,
    };
    let env: Vec<(String, String)> = token
        .iter()
        .map(|t| (TOKEN_ENV.to_string(), t.clone()))
        .collect();
    let mut args = ctx.plumbing.clone();
    args.extend(m.ch_args(p));
    let git_dir = dir.join("diff.git");
    let job = Job {
        id: &id,
        image: &task.image,
        work: &work,
        ch_home: &ch_home,
        git_dir: &git_dir,
        ch_args: &args,
        env: &env,
        verify: &task.verify,
    };
    let command = ctx.backend.command(Step::Agent, &job);
    let started_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let io = Io {
        stdin: Some(&task.instruction),
        stdout: sink(&dir.join("events.jsonl"), EVENTS_CAP)?,
        stderr: sink(&dir.join("stderr.log"), LOG_CAP)?,
    };
    let agent = ctx.backend.run(Step::Agent, &job, io, task.timeout).await;
    if let (Some(route), Some(token)) = (proxy, &token) {
        ledger::revoke_token(&route.state, token);
    }
    let exit = agent?;
    // The agent is done: its `.git` goes now, before anything else reads the workspace.
    let removed_early = scrub.now()?;
    let stream = events::parse_file(&dir.join("events.jsonl"));
    let calls = proxy.map(|route| ledger::summarize(&ledger::read_run(&route.state, &attempt)));
    let proxied = !models.providers.is_empty() && proxy.is_some();
    match classify(
        &exit,
        calls.as_ref(),
        proxied,
        stream.plugins.as_deref(),
        cell,
    ) {
        Verdict::Record => {}
        Verdict::CutByMaxCost => return Ok(Outcome::CutByMaxCost),
        Verdict::Transient(why) => return Ok(Outcome::Transient(why)),
        Verdict::Infra(message) => bail!(
            "{message}. No record was written. The end of {}:\n{}",
            dir.join("stderr.log").display(),
            tail(&dir.join("stderr.log"))
        ),
    }

    take_diff(ctx, &job, &dir, &baseline, &git_dir).await;
    // What the agent left can stop the check files from going in; that is the run's
    // failure, not the matrix's.
    let copied = match task.verify_dir().is_dir() {
        true => overlay(&task.verify_dir(), &work),
        false => Ok(()),
    };
    let (check, note) = match copied {
        Ok(()) => {
            let check = ctx
                .backend
                .run(
                    Step::Verify,
                    &job,
                    shared_log(&dir.join("verify.log"))?,
                    task.verify_timeout,
                )
                .await?;
            if let Some(e) = &check.start_error {
                bail!("the check's container didn't start: {e}");
            }
            (check, None)
        }
        Err(e) => {
            let note =
                format!("the check didn't run: what the agent left kept its files out: {e:#}");
            write(&dir.join("verify.log"), &format!("ch-eval: {note}\n"))?;
            (Exit::default(), Some(note))
        }
    };
    let removed = removed_early + scrub.finish()?;

    let (reason, detail) = reason_of(&exit, &stream, task.timeout);
    let (steps, usage, cost) = stream.totals();
    let trusted = calls.unwrap_or_default();
    let record = RunRecord {
        task: task.id.clone(),
        cell: cell.name.clone(),
        repeat: p.repeat,
        settings: cell.settings.clone(),
        tags: task.tags.clone(),
        image: task.image.clone(),
        command,
        started_at,
        passed: note.is_none() && check.code == Some(0) && !check.timed_out,
        reason,
        detail,
        note,
        exit_code: exit.code,
        usage: trusted.usage,
        cost: trusted.cost,
        model_calls: trusted.forwarded,
        reported: Reported { steps, usage, cost },
        wall_ms: millis(&exit),
        verify: VerifyRecord {
            exit_code: check.code,
            timed_out: check.timed_out,
            wall_ms: millis(&check),
        },
        signals: stream.signals.clone(),
        unreadable_event_lines: stream.unreadable_lines,
        git_entries_removed: removed,
        fingerprint: ctx.fingerprint(p),
    };
    record.save(&dir)?;
    Ok(Outcome::Recorded(Box::new(record), trusted))
}

/// Takes the diff in a container against a scratch copy of the baseline. Best effort:
/// whatever goes wrong is noted at the top of `diff.patch`, and the run goes on.
async fn take_diff(ctx: &Ctx, job: &Job<'_>, dir: &Path, baseline: &Path, git_dir: &Path) {
    let patch = dir.join("diff.patch");
    let part = dir.join("diff.patch.part");
    let errors = dir.join("diff.err");
    let result = async {
        copy_dir(baseline, git_dir)?;
        let io = Io {
            stdin: None,
            stdout: sink(&part, DIFF_CAP)?,
            stderr: sink(&errors, 64 * 1024)?,
        };
        ctx.backend.run(Step::Diff, job, io, DIFF_LIMIT).await
    }
    .await;
    let problem = match result {
        Err(e) => Some(format!("{e:#}")),
        Ok(exit) => match (&exit.start_error, exit.timed_out, exit.code) {
            (Some(e), _, _) => Some(format!("the diff's container didn't start: {e}")),
            (None, true, _) => Some(format!("git took longer than {}s", DIFF_LIMIT.as_secs())),
            (None, false, Some(0)) => None,
            (None, false, code) => Some(format!("git exited with {}", code_text(code))),
        },
    };
    let written = match problem {
        None => std::fs::rename(&part, &patch).map_err(anyhow::Error::from),
        Some(problem) => prepend_note(&patch, &part, &problem, &errors),
    };
    if let Err(e) = written {
        let _ = std::fs::write(
            &patch,
            format!("# ch-eval: the diff couldn't be saved: {e:#}\n"),
        );
    }
    let _ = std::fs::remove_file(&part);
    let _ = std::fs::remove_file(&errors);
    if git_dir.exists() {
        let _ = remove_dir(git_dir);
    }
}

/// Writes `patch` as `#` lines saying what went wrong (with the start of `errors`),
/// then `part`, streamed rather than read whole.
fn prepend_note(patch: &Path, part: &Path, problem: &str, errors: &Path) -> anyhow::Result<()> {
    use std::io::{Read as _, Write as _};
    let mut stderr = String::new();
    if let Ok(f) = std::fs::File::open(errors) {
        let _ = f.take(16 * 1024).read_to_string(&mut stderr);
    }
    let note: String = format!("ch-eval: this diff is incomplete: {problem}\n{stderr}")
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| format!("# {l}\n"))
        .collect();
    let mut out = std::fs::File::create(patch)?;
    out.write_all(note.as_bytes())?;
    if let Ok(mut body) = std::fs::File::open(part) {
        std::io::copy(&mut body, &mut out)?;
    }
    Ok(())
}

/// The last lines of a log, for an error message; at most the last 64 KB are read.
fn tail(path: &Path) -> String {
    use std::io::{Read as _, Seek as _, SeekFrom};
    let mut text = String::new();
    if let Ok(mut f) = std::fs::File::open(path) {
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        let _ = f.seek(SeekFrom::Start(len.saturating_sub(64 * 1024)));
        let mut bytes = Vec::new();
        let _ = f.read_to_end(&mut bytes);
        text = String::from_utf8_lossy(&bytes).into_owned();
    }
    let lines: Vec<&str> = text.lines().collect();
    let tail = lines[lines.len().saturating_sub(10)..].join("\n");
    if tail.trim().is_empty() {
        "(nothing)".into()
    } else {
        tail
    }
}

fn millis(exit: &Exit) -> u64 {
    u64::try_from(exit.wall.as_millis()).unwrap_or(u64::MAX)
}

fn code_text(code: Option<i32>) -> String {
    code.map_or("a signal".into(), |c| format!("code {c}"))
}

fn sink(path: &Path, cap: u64) -> anyhow::Result<Sink> {
    let file =
        std::fs::File::create(path).with_context(|| format!("creating {}", path.display()))?;
    Ok(Sink { file, cap })
}

/// One log for both of a step's outputs.
fn shared_log(path: &Path) -> anyhow::Result<Io<'static>> {
    let out = sink(path, LOG_CAP)?;
    let err = Sink {
        file: out.file.try_clone().context("opening a log")?,
        cap: LOG_CAP,
    };
    Ok(Io {
        stdin: None,
        stdout: out,
        stderr: err,
    })
}

fn write(path: &Path, text: &str) -> anyhow::Result<()> {
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use serde_json::json;

    use super::*;
    use crate::backend::ProxyRoute;
    use crate::events::Stream;
    use crate::ledger::{Call, StateDir};
    use crate::matrix::{PluginLists, cells, parse_vary};

    fn stream(lines: &[serde_json::Value]) -> Stream {
        let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
        events::parse(&text)
    }

    fn exit(code: Option<i32>) -> Exit {
        Exit {
            code,
            ..Default::default()
        }
    }

    fn base_cell() -> Cell {
        Cell {
            name: "base".into(),
            ..Default::default()
        }
    }

    #[test]
    fn timeout_wins_over_a_result_line() {
        let s = stream(&[json!({"type": "result", "reason": "done", "steps": 3})]);
        let killed = Exit {
            timed_out: true,
            ..Default::default()
        };
        let (reason, detail) = reason_of(&killed, &s, Duration::from_secs(9));
        assert_eq!(reason, TIMEOUT);
        assert_eq!(detail.as_deref(), Some("killed after 9s"));
        assert_eq!(reason_of(&exit(Some(0)), &s, Duration::ZERO).0, "done");
        assert_eq!(
            reason_of(&exit(Some(1)), &stream(&[]), Duration::ZERO).0,
            NO_RESULT
        );
    }

    #[test]
    fn classifies_from_what_the_runner_and_proxy_saw() {
        let cell = base_cell();
        let calls = |forwarded, succeeded, last_status: Option<u16>, transient| RunCalls {
            forwarded,
            succeeded,
            last_status,
            last_transient: transient,
            ..Default::default()
        };
        let ok = calls(3, 3, Some(200), false);
        assert_eq!(
            classify(&exit(Some(0)), Some(&ok), true, None, &cell),
            Verdict::Record
        );
        // A container's own exit code 125 is not Docker failing to start it.
        assert_eq!(
            classify(&exit(Some(125)), Some(&ok), true, None, &cell),
            Verdict::Record
        );
        let not_started = Exit {
            start_error: Some("exec format error".into()),
            ..Default::default()
        };
        assert!(matches!(
            classify(&not_started, Some(&ok), true, None, &cell),
            Verdict::Infra(m) if m.contains("exec format")
        ));
        // No model call made through the proxy: the run never reached the model.
        let none = calls(0, 0, None, false);
        assert!(matches!(
            classify(&exit(Some(1)), Some(&none), true, None, &cell),
            Verdict::Infra(m) if m.contains("before its first model call")
        ));
        // Calls made, none succeeded: a misspelled model, no credit, a request too big.
        let rejected = RunCalls {
            last_error: Some("model: claude-typo".into()),
            ..calls(1, 0, Some(404), false)
        };
        assert!(matches!(
            classify(&exit(Some(1)), Some(&rejected), true, None, &cell),
            Verdict::Infra(m) if m.contains("HTTP 404: model: claude-typo")
        ));
        // Without a proxy in the path (the scripted provider), that's the agent's.
        assert_eq!(
            classify(&exit(Some(1)), Some(&none), false, None, &cell),
            Verdict::Record
        );
        assert_eq!(
            classify(&exit(Some(1)), None, true, None, &cell),
            Verdict::Record
        );
        // An error after a transient failure is tried again, after real work too.
        let failing = calls(6, 5, Some(529), true);
        assert!(matches!(
            classify(&exit(Some(1)), Some(&failing), true, None, &cell),
            Verdict::Transient(m) if m.contains("529")
        ));
        // A run that ended normally is the agent's, whatever the API did last.
        for code in [Some(0), Some(2), Some(3)] {
            assert_eq!(
                classify(&exit(code), Some(&failing), true, None, &cell),
                Verdict::Record
            );
        }
        let killed = Exit {
            timed_out: true,
            ..Default::default()
        };
        assert_eq!(
            classify(&killed, Some(&failing), true, None, &cell),
            Verdict::Record
        );
        let refused = RunCalls {
            auth_failed: true,
            ..calls(1, 0, Some(401), false)
        };
        assert!(matches!(
            classify(&exit(Some(1)), Some(&refused), true, None, &cell),
            Verdict::Infra(m) if m.contains("refused the key")
        ));
        let cut = RunCalls {
            cut_by_max_cost: true,
            ..calls(2, 2, Some(200), false)
        };
        assert_eq!(
            classify(&exit(Some(1)), Some(&cut), true, None, &cell),
            Verdict::CutByMaxCost
        );
    }

    #[test]
    fn the_backoff_doubles_per_attempt() {
        let base = Duration::from_secs(30);
        assert_eq!(backoff(base, 1), Duration::from_secs(30));
        assert_eq!(backoff(base, 2), Duration::from_secs(60));
        assert_eq!(backoff(base, 3), Duration::from_secs(120));
        assert_eq!(backoff(base, 40), MAX_BACKOFF);
    }

    #[test]
    fn a_run_whose_plugins_differ_from_its_cell_is_not_recorded() {
        let dims = vec![parse_vary("planning=off").unwrap()];
        let cell = cells(&dims, &PluginLists::default()).unwrap().remove(0);
        let ok = ["tools-fs".to_string()];
        assert_eq!(
            classify(&exit(Some(0)), None, false, Some(&ok), &cell),
            Verdict::Record
        );
        let wrong = ["tools-fs".to_string(), "planning".to_string()];
        assert!(matches!(
            classify(&exit(Some(0)), None, false, Some(&wrong), &cell),
            Verdict::Infra(m) if m.contains("planning")
        ));
    }

    #[test]
    fn the_preflight_refuses_checks_that_pass_or_cannot_run() {
        for code in [0, 124, 125, 126, 127] {
            assert!(preflight_problem(&exit(Some(code))).is_some(), "{code}");
        }
        assert!(preflight_problem(&exit(None)).is_some());
        for code in [1, 2, 123, 128] {
            assert_eq!(preflight_problem(&exit(Some(code))), None, "{code}");
        }
    }

    #[test]
    fn proxy_settings_route_both_providers_and_keep_official_behaviour() {
        let base = Config::parse("").unwrap();
        let s = proxy_settings("http://ch-eval-proxy:8080", &base);
        assert_eq!(
            s,
            [
                r#"plugins.provider-anthropic.base_url="http://ch-eval-proxy:8080/anthropic""#,
                r#"plugins.provider-anthropic.api_key_env="CH_EVAL_API_KEY""#,
                r#"plugins.provider-openai.base_url="http://ch-eval-proxy:8080/openai""#,
                r#"plugins.provider-openai.api_key_env="CH_EVAL_API_KEY""#,
                "plugins.provider-anthropic.official_api=true",
                r#"plugins.provider-openai.max_tokens_field="max_completion_tokens""#,
            ]
        );
        let custom = Config::parse(
            "[plugins.provider-anthropic]\nbase_url = \"http://gateway:9\"\n\
             [plugins.provider-openai]\nbase_url = \"http://vllm:8000/v1\"\n",
        )
        .unwrap();
        let s = proxy_settings("http://p:1", &custom);
        assert!(s.contains(&"plugins.provider-anthropic.official_api=false".to_string()));
        assert!(
            s.contains(&r#"plugins.provider-openai.max_tokens_field="max_tokens""#.to_string())
        );
        let explicit = Config::parse(
            "[plugins.provider-anthropic]\nofficial_api = true\n[plugins.provider-openai]\nmax_tokens_field = \"x\"\n",
        )
        .unwrap();
        assert_eq!(proxy_settings("http://p:1", &explicit).len(), 4);
        assert_eq!(
            upstream_url(&custom, Provider::OpenAi),
            "http://vllm:8000/v1"
        );
        assert_eq!(key_env(&base, Provider::OpenAi), "OPENAI_API_KEY");
    }

    fn matrix(config: &str, settings: &[&str]) -> Matrix {
        Matrix {
            tasks: vec![],
            cells: vec![Cell {
                name: "c".into(),
                settings: settings.iter().map(|s| s.to_string()).collect(),
                ..Default::default()
            }],
            repeats: 1,
            profile: None,
            config: Some(config.into()),
        }
    }

    #[test]
    fn cells_list_their_models_with_each_ones_pricing() {
        let config = r#"
            [model]
            provider = "anthropic"
            id = "claude-a"
            [model.pricing]
            input = 3.0
            [model.summarizer]
            provider = "openai"
            id = "gpt-small"
            [eval.pricing."gpt-small"]
            input = 0.5
        "#;
        let c = matrix(config, &[]).cell_models(0).unwrap();
        assert_eq!(
            c.providers,
            BTreeSet::from([Provider::Anthropic, Provider::OpenAi])
        );
        assert_eq!(c.models["claude-a"].unwrap().input, 3.0);
        assert_eq!(c.models["gpt-small"].unwrap().input, 0.5);
        // [model.pricing] is the configured model's, as in ch: a cell that changes the
        // model takes it along unless [eval.pricing] lists the new one.
        let c = matrix(config, &[r#"model.id="claude-b""#])
            .cell_models(0)
            .unwrap();
        assert_eq!(c.models["claude-b"].unwrap().input, 3.0);
        let listed = format!("{config}\n[eval.pricing.\"claude-b\"]\ninput = 15.0\n");
        let c = matrix(&listed, &[r#"model.id="claude-b""#])
            .cell_models(0)
            .unwrap();
        assert_eq!(c.models["claude-b"].unwrap().input, 15.0);
        // The proxy needs to know the model.
        assert!(
            matrix("[model]\nprovider = \"anthropic\"\n", &[])
                .cell_models(0)
                .is_err()
        );
        let scripted = matrix("[model]\nprovider = \"scripted\"\n", &[])
            .cell_models(0)
            .unwrap();
        assert!(scripted.providers.is_empty() && scripted.models.is_empty());
    }

    #[test]
    fn unpriced_model_changes_warn_and_malformed_prices_fail() {
        let config =
            "[model]\nprovider = \"anthropic\"\nid = \"a\"\n[model.pricing]\ninput = 3.0\n";
        let c = matrix(config, &[r#"model.id="b""#]).cell_models(0).unwrap();
        assert!(c.warnings[0].contains("model b"), "{:?}", c.warnings);
        assert!(
            matrix(config, &[])
                .cell_models(0)
                .unwrap()
                .warnings
                .is_empty()
        );
        let bad = format!("{config}[eval.pricing.a]\ninput = \"lots\"\n");
        let err = matrix(&bad, &[]).cell_models(0).unwrap_err();
        assert!(format!("{err:#}").contains("eval.pricing"), "{err:#}");
    }

    #[test]
    fn the_config_fingerprint_ignores_comments_and_order() {
        let a = normalized("# a comment\n[model]\nid = \"x\"\nprovider = \"p\"\n").unwrap();
        let b = normalized("[model]\nprovider = \"p\" # here\nid = \"x\"\n").unwrap();
        assert_eq!(a, b);
        assert_ne!(
            a,
            normalized("[model]\nid = \"y\"\nprovider = \"p\"\n").unwrap()
        );
    }

    /// What the fake agent does in one task.
    #[derive(Clone)]
    struct Act {
        agent: Exit,
        /// Calls the "proxy" records for the run.
        calls: Vec<Call>,
        check: Exit,
    }

    /// A backend whose agent plants `.git` entries and finishes as its task's `Act`
    /// says, so every way a run can end is reachable without Docker or a model.
    struct Fake {
        state: Option<ProxyRoute>,
        acts: BTreeMap<String, Act>,
        /// Agent runs, per task.
        tries: Mutex<BTreeMap<String, usize>>,
    }

    impl Fake {
        fn new(dir: &Path, acts: &[(&str, Act)]) -> Self {
            let state = StateDir(dir.join("out/proxy"));
            state.prepare().unwrap();
            Self {
                state: Some(ProxyRoute {
                    url: "http://proxy".into(),
                    state,
                }),
                acts: acts
                    .iter()
                    .map(|(t, a)| (t.to_string(), a.clone()))
                    .collect(),
                tries: Mutex::new(BTreeMap::new()),
            }
        }

        fn tries(&self, task: &str) -> usize {
            self.tries.lock().unwrap().get(task).copied().unwrap_or(0)
        }
    }

    /// `<out>/runs/<task>/<cell>/<repeat>/work`: the task.
    fn task_of(job: &Job<'_>) -> String {
        let runs = job.work.ancestors().nth(3).unwrap();
        runs.file_name().unwrap().to_string_lossy().into_owned()
    }

    #[async_trait]
    impl Backend for Fake {
        fn command(&self, _: Step, _: &Job<'_>) -> Vec<String> {
            vec!["fake".into()]
        }

        async fn run(
            &self,
            step: Step,
            job: &Job<'_>,
            _io: Io<'_>,
            _: Duration,
        ) -> anyhow::Result<Exit> {
            if step == Step::Verify && job.ch_args.is_empty() {
                // The pre-flight: the check fails on the untouched repo.
                return Ok(exit(Some(1)));
            }
            let task = task_of(job);
            let act = self.acts[&task].clone();
            match step {
                Step::Agent => {
                    *self.tries.lock().unwrap().entry(task).or_default() += 1;
                    std::fs::create_dir_all(job.work.join("sub/.Git")).unwrap();
                    std::fs::create_dir_all(job.ch_home.join(".git")).unwrap();
                    if let (Some(route), Some((_, token))) = (&self.state, job.env.first()) {
                        let run = ledger::lookup_token(&route.state, token).unwrap().run;
                        for call in &act.calls {
                            ledger::append(&route.state, &run, call).unwrap();
                        }
                    }
                    Ok(act.agent)
                }
                Step::Diff => {
                    // The agent's `.git` is gone before the diff and the check.
                    assert!(!job.work.join("sub/.Git").exists());
                    assert!(!job.ch_home.join(".git").exists());
                    Ok(exit(Some(0)))
                }
                Step::Verify => Ok(act.check),
            }
        }

        fn proxy(&self) -> Option<&ProxyRoute> {
            self.state.as_ref()
        }

        fn identity(&self, _: &str) -> String {
            "fake".into()
        }
    }

    fn suite(dir: &Path, tasks: &[&str]) -> Matrix {
        for id in tasks {
            let task = dir.join("suite").join(id);
            std::fs::create_dir_all(task.join("repo")).unwrap();
            std::fs::write(task.join("repo/a.txt"), "a").unwrap();
            std::fs::write(
                task.join("task.toml"),
                "instruction = \"x\"\nverify = \"false\"\n",
            )
            .unwrap();
        }
        Matrix {
            tasks: crate::task::load_suite(&dir.join("suite")).unwrap(),
            cells: vec![Cell {
                name: "base".into(),
                ..Default::default()
            }],
            repeats: 1,
            profile: None,
            config: Some("[model]\nprovider = \"anthropic\"\nid = \"m\"\n".into()),
        }
    }

    async fn run_fake(
        dir: &Path,
        fake: Arc<Fake>,
        jobs: usize,
        backoff: Duration,
    ) -> anyhow::Result<Summary> {
        let tasks: Vec<&str> = fake.acts.keys().map(String::as_str).collect();
        let opts = Options {
            out: dir.join("out"),
            jobs,
            max_cost: None,
            max_attempts: 3,
            backoff,
        };
        run_matrix(Arc::new(suite(dir, &tasks)), fake, &opts).await
    }

    fn call(status: u16, refused: Option<&str>) -> Call {
        Call {
            status,
            refused: refused.map(String::from),
            ..Default::default()
        }
    }

    fn act(agent: Option<i32>, calls: Vec<Call>) -> Act {
        Act {
            agent: exit(agent),
            calls,
            check: exit(Some(1)),
        }
    }

    fn no_git_left(dir: &Path, task: &str) {
        let run = dir.join("out/runs").join(task).join("base/1");
        assert!(
            run.join("work").is_dir(),
            "the run got as far as its workspace"
        );
        assert!(
            !run.join("work/sub/.Git").exists(),
            "work/ still has a .git"
        );
        assert!(
            !run.join("ch-home/.git").exists(),
            "ch-home/ still has a .git"
        );
        assert!(run.join("baseline.git/HEAD").is_file());
    }

    const QUICK: Duration = Duration::from_millis(10);

    #[tokio::test]
    async fn every_way_a_run_ends_leaves_no_git_behind() {
        // Recorded.
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::new(
            dir.path(),
            &[("t", act(Some(0), vec![call(200, None)]))],
        ));
        run_fake(dir.path(), fake, 1, QUICK).await.unwrap();
        no_git_left(dir.path(), "t");
        assert!(dir.path().join("out/runs/t/base/1/run.json").is_file());

        // Infrastructure: the run never reached the model.
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::new(dir.path(), &[("t", act(Some(1), vec![]))]));
        assert!(run_fake(dir.path(), fake, 1, QUICK).await.is_err());
        no_git_left(dir.path(), "t");

        // Cut off by --max-cost.
        let dir = tempfile::tempdir().unwrap();
        let calls = vec![call(200, None), call(0, Some("max_cost"))];
        let fake = Arc::new(Fake::new(dir.path(), &[("t", act(Some(1), calls))]));
        let summary = run_fake(dir.path(), fake, 1, QUICK).await.unwrap();
        assert_eq!(summary.stopped_by_max_cost, 1);
        no_git_left(dir.path(), "t");

        // The check's container didn't start.
        let dir = tempfile::tempdir().unwrap();
        let mut broken = act(Some(0), vec![call(200, None)]);
        broken.check.start_error = Some("no such image".into());
        let fake = Arc::new(Fake::new(dir.path(), &[("t", broken)]));
        assert!(run_fake(dir.path(), fake, 1, QUICK).await.is_err());
        no_git_left(dir.path(), "t");
    }

    #[tokio::test]
    async fn a_run_is_tried_at_most_max_attempts_times() {
        // "t" fails transiently every time; "u" is recorded meanwhile, which must not
        // give "t" more tries.
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::new(
            dir.path(),
            &[
                ("t", act(Some(1), vec![call(200, None), call(529, None)])),
                ("u", act(Some(0), vec![call(200, None)])),
            ],
        ));
        let err = run_fake(dir.path(), fake.clone(), 1, QUICK)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("on all 3 attempts"), "{err:#}");
        assert_eq!(fake.tries("t"), 3);
        assert_eq!(fake.tries("u"), 1);
        assert!(dir.path().join("out/runs/u/base/1/run.json").is_file());
        no_git_left(dir.path(), "t");
    }

    #[tokio::test]
    async fn a_waiting_worker_wakes_when_the_matrix_stops() {
        // "t" fails transiently and waits an hour for its retry; "u" stops the matrix.
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::new(
            dir.path(),
            &[
                ("t", act(Some(1), vec![call(200, None), call(529, None)])),
                ("u", act(Some(1), vec![])),
            ],
        ));
        let started = Instant::now();
        let err = run_fake(dir.path(), fake, 2, Duration::from_secs(3600))
            .await
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("before its first model call"),
            "{err:#}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "{:?}",
            started.elapsed()
        );
    }
}
