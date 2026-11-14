//! The scheduler: the preflight, the queue of runs, `-j` workers and retries with backoff.

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context as _, anyhow, bail};
use futures::StreamExt;
use tokio::sync::Notify;

use crate::backend::{Backend, Job, Step, Stopping};
use crate::fingerprint::{Fingerprint, hash, hash_file, hash_tree};
use crate::ledger::{self};
use crate::matrix::Planned;
use crate::record::{RUN_JSON, RunRecord};
use crate::report::count;
use crate::workspace::{Scrub, copy_repo, overlay, remove_dir};

use super::steps::{Outcome, run_one, shared_log};
use super::verdict::preflight_problem;
use super::{CellModels, Matrix, Options, Summary, normalized, plumbing};

/// The longest wait before a run whose model API failed transiently is tried again.
pub(super) const MAX_BACKOFF: Duration = Duration::from_secs(15 * 60);

/// The task's own hashes, for fingerprints.
struct TaskHashes {
    task: String,
    repo: String,
    verify: String,
}

/// What every run of an invocation shares.
pub(super) struct Ctx {
    pub(super) m: Arc<Matrix>,
    pub(super) backend: Arc<dyn Backend>,
    pub(super) out: PathBuf,
    /// Names this invocation's runs in the ledger and in container names.
    pub(super) invocation: String,
    pub(super) plumbing: Vec<String>,
    /// What each cell's runs may call.
    pub(super) cells: Vec<CellModels>,
    config_hash: String,
    tasks: Vec<TaskHashes>,
    /// The backend's identity for each task image, worked out once.
    identities: BTreeMap<String, String>,
}

impl Ctx {
    pub(super) fn fingerprint(&self, p: &Planned) -> Fingerprint {
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
