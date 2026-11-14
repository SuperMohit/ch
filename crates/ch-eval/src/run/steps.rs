//! One run, step by step: workspace, agent, diff, check and `run.json`.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context as _, bail};

use crate::backend::{Exit, Io, Job, Sink, Step};
use crate::events;
use crate::ledger::{self, RunCalls, TOKEN_ENV};
use crate::matrix::Planned;
use crate::record::{Reported, RunRecord, VerifyRecord};
use crate::workspace::{Scrub, commit_baseline, copy_dir, copy_repo, overlay, remove_dir};

use super::schedule::Ctx;
use super::verdict::{Verdict, classify, code_text, reason_of};

/// Wall time for the diff.
const DIFF_LIMIT: Duration = Duration::from_secs(120);
/// How much of each output is kept. Past it, the rest is dropped with a note.
const EVENTS_CAP: u64 = events::MAX_TOTAL;
const LOG_CAP: u64 = 16 * 1024 * 1024;
const DIFF_CAP: u64 = 64 * 1024 * 1024;

/// How a run ended, for the runner.
#[derive(Debug)]
pub(super) enum Outcome {
    Recorded(Box<RunRecord>, RunCalls),
    CutByMaxCost,
    Transient(String),
}

/// Does one run and, if it measured the agent, saves its `run.json`.
pub(super) async fn run_one(ctx: &Ctx, p: &Planned, seq: usize) -> anyhow::Result<Outcome> {
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

fn sink(path: &Path, cap: u64) -> anyhow::Result<Sink> {
    let file =
        std::fs::File::create(path).with_context(|| format!("creating {}", path.display()))?;
    Ok(Sink { file, cap })
}

/// One log for both of a step's outputs.
pub(super) fn shared_log(path: &Path) -> anyhow::Result<Io<'static>> {
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
