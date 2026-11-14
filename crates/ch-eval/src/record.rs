//! `run.json`: the record of one run. An existing `run.json` means the run is done.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

use crate::events::{Signals, Usage};
use crate::fingerprint::Fingerprint;

/// The record's file name in a run directory.
pub const RUN_JSON: &str = "run.json";

/// Termination reason recorded when the agent run was killed at the task's timeout.
pub const TIMEOUT: &str = "timeout";
/// Termination reason recorded when `ch` printed no result line after reaching the
/// model (it crashed, or was killed for memory); `stderr.log` says why. Without a model
/// call it is an infrastructure failure, and no record is written.
pub const NO_RESULT: &str = "no_result";

/// One run of one task in one cell.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunRecord {
    pub task: String,
    pub cell: String,
    /// Counts from 1.
    pub repeat: u32,
    /// The cell's `--set` assignments.
    pub settings: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    pub image: String,
    /// The agent's command line (no secret values: environment variables are passed by
    /// name).
    pub command: Vec<String>,
    /// RFC 3339.
    pub started_at: String,
    /// The check exited 0.
    pub passed: bool,
    /// `ch`'s termination reason (a short word, `invalid` if it wasn't one), or
    /// [`TIMEOUT`] or [`NO_RESULT`]. Self-reported, like everything from the events.
    pub reason: String,
    pub detail: Option<String>,
    /// Why the run is a failure although the agent finished, such as check files that
    /// couldn't be copied into what the agent left.
    #[serde(default)]
    pub note: Option<String>,
    /// The agent container's exit code; `None` when it was killed at the timeout.
    pub exit_code: Option<i32>,
    /// Tokens, counted by the proxy from the model API's responses. Zero when no call
    /// went through the proxy (the scripted provider).
    pub usage: Usage,
    /// USD: `usage` priced with the run's `[model.pricing]`; 0 without one.
    pub cost: f64,
    /// Requests the proxy forwarded, retries included.
    pub model_calls: u32,
    /// What `ch` itself reported in its event stream. The agent can write to that
    /// stream, so these numbers are not trusted; they are clamped to sane values.
    pub reported: Reported,
    /// Wall time of the agent run, container start included.
    pub wall_ms: u64,
    pub verify: VerifyRecord,
    /// From the event stream, so also self-reported.
    pub signals: Signals,
    /// Lines of `events.jsonl` that weren't JSON (a killed run's last line, or output
    /// the agent wrote there).
    pub unreadable_event_lines: usize,
    /// `.git` entries removed from `work/` after the run, so git on the host never
    /// reads agent-written config there. `baseline.git` holds the starting commit.
    pub git_entries_removed: usize,
    pub fingerprint: Fingerprint,
}

/// `ch`'s own account of a run.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Reported {
    /// Agent model calls.
    pub steps: u32,
    pub usage: Usage,
    pub cost: f64,
}

/// How the check went.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct VerifyRecord {
    /// `None` when it was killed at its timeout.
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub wall_ms: u64,
}

impl VerifyRecord {
    /// The check couldn't run its command: the shell's 126 (not executable) or 127
    /// (not found).
    pub fn could_not_run(&self) -> bool {
        matches!(self.exit_code, Some(126 | 127))
    }
}

impl RunRecord {
    /// Writes `run.json` in `dir` through a temporary file, so an interrupted write
    /// can't leave a record that marks the run done.
    pub fn save(&self, dir: &Path) -> anyhow::Result<()> {
        let tmp = dir.join(format!("{RUN_JSON}.tmp"));
        let text = serde_json::to_string_pretty(self)? + "\n";
        std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, dir.join(RUN_JSON))
            .with_context(|| format!("saving {}", dir.join(RUN_JSON).display()))
    }
}

/// `<out>/runs/<task>/<cell>/<repeat>`.
pub fn run_dir(out: &Path, task: &str, cell: &str, repeat: u32) -> PathBuf {
    out.join("runs")
        .join(task)
        .join(cell)
        .join(repeat.to_string())
}

/// Every `run.json` under `<out>/runs`, sorted by task, cell and repeat.
pub fn load_all(out: &Path) -> anyhow::Result<Vec<RunRecord>> {
    let runs = out.join("runs");
    let mut records = Vec::new();
    if !runs.is_dir() {
        return Ok(records);
    }
    for task in subdirs(&runs)? {
        for cell in subdirs(&task)? {
            for repeat in subdirs(&cell)? {
                let path = repeat.join(RUN_JSON);
                if !path.is_file() {
                    continue;
                }
                let text = std::fs::read_to_string(&path)
                    .with_context(|| format!("reading {}", path.display()))?;
                let record: RunRecord = serde_json::from_str(&text)
                    .with_context(|| format!("parsing {}", path.display()))?;
                records.push(record);
            }
        }
    }
    records.sort_by(|a, b| (&a.task, &a.cell, a.repeat).cmp(&(&b.task, &b.cell, b.repeat)));
    Ok(records)
}

fn subdirs(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        if path.is_dir() {
            out.push(path);
        }
    }
    Ok(out)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A record with the given outcome, for tests here and in the report.
    pub(crate) fn record(task: &str, cell: &str, repeat: u32, passed: bool) -> RunRecord {
        RunRecord {
            task: task.into(),
            cell: cell.into(),
            repeat,
            settings: vec![],
            tags: vec![],
            image: "ch-eval-base".into(),
            command: vec!["ch".into(), "-p".into(), "-".into()],
            started_at: "2026-11-13T00:00:00Z".into(),
            passed,
            reason: "done".into(),
            detail: None,
            note: None,
            exit_code: Some(0),
            usage: Usage::default(),
            cost: 0.0,
            model_calls: 4,
            reported: Reported {
                steps: 4,
                ..Default::default()
            },
            wall_ms: 1000,
            verify: VerifyRecord {
                exit_code: Some(if passed { 0 } else { 1 }),
                timed_out: false,
                wall_ms: 10,
            },
            signals: Signals::default(),
            unreadable_event_lines: 0,
            git_entries_removed: 1,
            fingerprint: Fingerprint::default(),
        }
    }

    #[test]
    fn saves_and_loads_every_record() {
        let out = tempfile::tempdir().unwrap();
        let a = record("t1", "base", 1, true);
        let b = record("t1", "base", 2, false);
        let c = record("t0", "planning=off", 1, true);
        for r in [&a, &b, &c] {
            let dir = run_dir(out.path(), &r.task, &r.cell, r.repeat);
            std::fs::create_dir_all(&dir).unwrap();
            r.save(&dir).unwrap();
            assert!(!dir.join("run.json.tmp").exists());
        }
        // A run that was interrupted has a directory but no record.
        std::fs::create_dir_all(run_dir(out.path(), "t1", "base", 3)).unwrap();
        assert_eq!(load_all(out.path()).unwrap(), [c, a, b]);
        assert!(load_all(&out.path().join("missing")).unwrap().is_empty());
    }

    #[test]
    fn saving_replaces_the_file_instead_of_writing_into_it() {
        // A reader holding the old run.json (here, a hard link to it) keeps a whole
        // record: the new one is renamed into place, never written over the old.
        let dir = tempfile::tempdir().unwrap();
        let r = record("t", "c", 1, true);
        r.save(dir.path()).unwrap();
        let held = dir.path().join("held.json");
        std::fs::hard_link(dir.path().join(RUN_JSON), &held).unwrap();
        let mut changed = r.clone();
        changed.passed = false;
        changed.save(dir.path()).unwrap();
        let old: RunRecord =
            serde_json::from_str(&std::fs::read_to_string(&held).unwrap()).unwrap();
        assert!(old.passed);
        let new: RunRecord =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join(RUN_JSON)).unwrap())
                .unwrap();
        assert!(!new.passed);
    }
}
