//! The report: `report.md` for people and `results.json` for tools, both built from the
//! `run.json` files alone, so `ch-eval report` can write them again at any time.
//!
//! - Per cell: the pass rate with a 95% Wilson interval over runs; mean and median cost,
//!   tokens (both counted by the proxy), steps (reported by `ch`) and wall time; how
//!   runs and checks ended; the harness signals per run; and a note when its runs were
//!   made from different binaries, configs or tasks.
//! - Per task and cell: passes out of runs.
//! - For each pair of cells: per task, which passed more often over the repeats both
//!   have, and an exact two-sided sign test over the tasks where they differ. Tasks are
//!   the unit, since repeats of one task aren't independent evidence about the harness.
//!
//! Strings that came from a run (reasons, tool names, failure reasons) are escaped, so
//! an agent can't write markup into the report.

use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context as _, bail};
use serde::Serialize;

use crate::record::{RunRecord, load_all};
use crate::stats::{self, Paired, mean, median, sign_test, wilson};

/// With fewer differing tasks than this, a two-sided sign test can't reach p < 0.05.
const MIN_TASKS_FOR_SIGNIFICANCE: u64 = 6;

/// The report's numbers, also written as `results.json`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Results {
    pub runs: usize,
    pub tasks: usize,
    /// Caveats that apply to the whole report.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    pub cells: Vec<CellStats>,
    pub per_task: Vec<TaskRow>,
    pub pairs: Vec<PairStats>,
}

/// Mean and median of a per-run number.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct Spread {
    pub mean: f64,
    pub median: f64,
}

impl Spread {
    fn of(values: &[f64]) -> Self {
        Self {
            mean: mean(values).unwrap_or(0.0),
            median: median(values).unwrap_or(0.0),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CellStats {
    pub name: String,
    /// The cell's `--set` assignments.
    pub settings: Vec<String>,
    pub runs: u64,
    pub passed: u64,
    pub pass_rate: f64,
    /// 95% Wilson interval over runs, `[low, high]`. It treats runs as independent.
    pub pass_rate_ci95: [f64; 2],
    /// USD, from the proxy.
    pub cost: Spread,
    /// From the proxy.
    pub model_calls: Spread,
    pub input_tokens: Spread,
    pub output_tokens: Spread,
    pub cache_read_tokens: Spread,
    pub cache_write_tokens: Spread,
    /// Agent steps, as `ch` reported them.
    pub steps: Spread,
    pub wall_secs: Spread,
    /// Runs by termination reason.
    pub reasons: BTreeMap<String, u64>,
    pub checks: CheckCounts,
    pub signals: SignalStats,
    /// How many different fingerprints the cell's runs have; more than one means they
    /// were made from different binaries, configs or tasks.
    pub fingerprints: usize,
}

/// How the checks ended.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct CheckCounts {
    pub passed: u64,
    pub failed: u64,
    pub timed_out: u64,
    /// Exit 126 or 127: the check's command couldn't be run.
    pub could_not_run: u64,
}

/// Harness signals, as means per run.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct SignalStats {
    pub tool_calls: f64,
    pub tool_calls_by_tool: BTreeMap<String, f64>,
    pub tool_errors: f64,
    /// By `<tool>:<failure reason>`.
    pub tool_errors_by_reason: BTreeMap<String, f64>,
    /// Calls that didn't run (denied or stopped).
    pub tool_denials: f64,
    pub compactions: f64,
    pub plan_refreshes: f64,
    pub hook_denials: f64,
    pub warnings: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TaskRow {
    pub task: String,
    /// By cell name; cells without runs of the task are absent.
    pub cells: BTreeMap<String, PassCount>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct PassCount {
    pub passed: u64,
    pub runs: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PairStats {
    pub a: String,
    pub b: String,
    /// Tasks where `a` passed more often, over the repeats both cells have.
    pub a_wins: u64,
    /// Tasks where `b` passed more often.
    pub b_wins: u64,
    pub ties: u64,
    /// Exact two-sided sign test over `a_wins + b_wins` tasks.
    pub p_value: f64,
}

/// Builds the results from run records. Cells are sorted by name.
pub fn build(records: &[RunRecord]) -> Results {
    let mut by_cell: BTreeMap<&str, Vec<&RunRecord>> = BTreeMap::new();
    let mut by_task: BTreeMap<&str, BTreeMap<String, PassCount>> = BTreeMap::new();
    // task -> cell -> repeat -> passed, for pairing.
    let mut outcomes: BTreeMap<&str, BTreeMap<&str, BTreeMap<u32, bool>>> = BTreeMap::new();
    for r in records {
        by_cell.entry(&r.cell).or_default().push(r);
        let count = by_task
            .entry(&r.task)
            .or_default()
            .entry(r.cell.clone())
            .or_default();
        count.runs += 1;
        count.passed += u64::from(r.passed);
        outcomes
            .entry(&r.task)
            .or_default()
            .entry(&r.cell)
            .or_default()
            .insert(r.repeat, r.passed);
    }
    let cells: Vec<CellStats> = by_cell.iter().map(|(n, rs)| cell_stats(n, rs)).collect();
    let per_task: Vec<TaskRow> = by_task
        .into_iter()
        .map(|(task, cells)| TaskRow {
            task: task.to_string(),
            cells,
        })
        .collect();
    let mut pairs = Vec::new();
    for (i, a) in cells.iter().enumerate() {
        for b in &cells[i + 1..] {
            pairs.push(pair_stats(&a.name, &b.name, &outcomes));
        }
    }
    Results {
        runs: records.len(),
        tasks: per_task.len(),
        notes: Vec::new(),
        cells,
        per_task,
        pairs,
    }
}

fn cell_stats(name: &str, runs: &[&RunRecord]) -> CellStats {
    let n = runs.len() as u64;
    let passed = runs.iter().filter(|r| r.passed).count() as u64;
    let spread = |f: &dyn Fn(&RunRecord) -> f64| {
        Spread::of(&runs.iter().map(|r| f(r)).collect::<Vec<f64>>())
    };
    let (lo, hi) = wilson(passed, n).unwrap_or((0.0, 1.0));
    let mut reasons = BTreeMap::new();
    let mut checks = CheckCounts::default();
    for r in runs {
        *reasons.entry(r.reason.clone()).or_default() += 1;
        if r.passed {
            checks.passed += 1;
        } else if r.verify.timed_out {
            checks.timed_out += 1;
        } else if r.verify.could_not_run() {
            checks.could_not_run += 1;
        } else {
            checks.failed += 1;
        }
    }
    let fingerprints: HashSet<_> = runs.iter().map(|r| &r.fingerprint).collect();
    CellStats {
        name: name.to_string(),
        settings: runs.first().map(|r| r.settings.clone()).unwrap_or_default(),
        runs: n,
        passed,
        pass_rate: if n == 0 {
            0.0
        } else {
            passed as f64 / n as f64
        },
        pass_rate_ci95: [lo, hi],
        cost: spread(&|r| r.cost),
        model_calls: spread(&|r| f64::from(r.model_calls)),
        input_tokens: spread(&|r| r.usage.input_tokens as f64),
        output_tokens: spread(&|r| r.usage.output_tokens as f64),
        cache_read_tokens: spread(&|r| r.usage.cache_read_tokens as f64),
        cache_write_tokens: spread(&|r| r.usage.cache_write_tokens as f64),
        steps: spread(&|r| f64::from(r.reported.steps)),
        wall_secs: spread(&|r| r.wall_ms as f64 / 1000.0),
        reasons,
        checks,
        signals: signal_stats(runs),
        fingerprints: fingerprints.len(),
    }
}

fn signal_stats(runs: &[&RunRecord]) -> SignalStats {
    let n = runs.len().max(1) as f64;
    let mut s = SignalStats::default();
    for r in runs {
        let sig = &r.signals;
        for (tool, count) in &sig.tool_calls {
            *s.tool_calls_by_tool.entry(tool.clone()).or_default() += *count as f64 / n;
        }
        for (reason, count) in &sig.tool_errors {
            *s.tool_errors_by_reason.entry(reason.clone()).or_default() += *count as f64 / n;
        }
        s.tool_calls += sig.tool_call_total() as f64 / n;
        s.tool_errors += sig.tool_error_total() as f64 / n;
        s.tool_denials += sig.tool_denial_total() as f64 / n;
        s.compactions += sig.compactions as f64 / n;
        s.plan_refreshes += sig.plan_refreshes as f64 / n;
        s.hook_denials += sig.hook_denials as f64 / n;
        s.warnings += sig.warnings as f64 / n;
    }
    s
}

type Outcomes<'a> = BTreeMap<&'a str, BTreeMap<&'a str, BTreeMap<u32, bool>>>;

/// Per task, compares the two cells over the repeats both have, so a cell with more
/// runs of a task doesn't weigh more.
fn pair_stats(a: &str, b: &str, outcomes: &Outcomes<'_>) -> PairStats {
    let (mut a_wins, mut b_wins, mut ties) = (0, 0, 0);
    for cells in outcomes.values() {
        let (Some(x), Some(y)) = (cells.get(a), cells.get(b)) else {
            continue;
        };
        let common: Vec<u32> = x.keys().filter(|r| y.contains_key(r)).copied().collect();
        if common.is_empty() {
            continue;
        }
        let passes = |m: &BTreeMap<u32, bool>| common.iter().filter(|r| m[r]).count() as u64;
        let n = common.len() as u64;
        match stats::compare(passes(x), n, passes(y), n) {
            Paired::A => a_wins += 1,
            Paired::B => b_wins += 1,
            Paired::Tie => ties += 1,
        }
    }
    PairStats {
        a: a.to_string(),
        b: b.to_string(),
        a_wins,
        b_wins,
        ties,
        p_value: sign_test(a_wins, b_wins),
    }
}

/// Renders `report.md`.
pub fn markdown(r: &Results) -> String {
    let mut md = String::new();
    let cell_names: Vec<&str> = r.cells.iter().map(|c| c.name.as_str()).collect();
    let _ = writeln!(
        md,
        "# ch-eval report\n\n{} of {} in {}. Written from the `run.json` files under \
         `runs/`; `ch-eval report <dir>` writes it again.\n\n\
         Cost, tokens and model calls are counted by the credential proxy from the model \
         API's answers; runs whose model calls didn't go through it (the scripted \
         provider) show 0. Steps, termination reasons and harness signals come from \
         `ch`'s own event stream, which the agent could write to.\n",
        count(r.runs, "run"),
        count(r.tasks, "task"),
        count(r.cells.len(), "cell")
    );
    for note in &r.notes {
        let _ = writeln!(md, "**Note:** {}\n", text(note));
    }
    for c in r.cells.iter().filter(|c| c.fingerprints > 1) {
        let _ = writeln!(
            md,
            "**Note:** the runs of {} were made from {} different sets of binaries, \
             configs and tasks, so they don't all measure the same thing.\n",
            text(&c.name),
            c.fingerprints
        );
    }

    md.push_str("## Cells\n\n| Cell | Settings |\n|---|---|\n");
    for c in &r.cells {
        let settings = if c.settings.is_empty() {
            "(base config)".to_string()
        } else {
            c.settings
                .iter()
                .map(|s| text(s))
                .collect::<Vec<_>>()
                .join("<br>")
        };
        let _ = writeln!(md, "| {} | {settings} |", text(&c.name));
    }

    md.push_str(
        "\n## Pass rate\n\nThe interval is a 95% Wilson interval over runs. It treats every \
         run as independent; repeats of one task aren't, so with repeats it is too narrow. \
         Compare cells with the paired comparison below.\n\n\
         | Cell | Runs | Passed | Pass rate | 95% interval (runs) |\n\
         |---|---:|---:|---:|---|\n",
    );
    for c in &r.cells {
        let [lo, hi] = c.pass_rate_ci95;
        let _ = writeln!(
            md,
            "| {} | {} | {} | {} | {}–{} |",
            text(&c.name),
            c.runs,
            c.passed,
            percent(c.pass_rate),
            percent(lo),
            percent(hi)
        );
    }

    md.push_str(
        "\n## Cost and effort\n\nMean / median per run.\n\n\
         | Cell | Cost (USD) | Model calls | Input tokens | Output tokens | Cache read | Cache write | Steps (reported) | Wall time (s) |\n\
         |---|---:|---:|---:|---:|---:|---:|---:|---:|\n",
    );
    for c in &r.cells {
        let _ = writeln!(
            md,
            "| {} | {:.4} / {:.4} | {} | {} | {} | {} | {} | {} | {} |",
            text(&c.name),
            c.cost.mean,
            c.cost.median,
            spread(c.model_calls, 1),
            spread(c.input_tokens, 0),
            spread(c.output_tokens, 0),
            spread(c.cache_read_tokens, 0),
            spread(c.cache_write_tokens, 0),
            spread(c.steps, 1),
            spread(c.wall_secs, 1),
        );
    }

    let mut reasons: Vec<&str> = r
        .cells
        .iter()
        .flat_map(|c| c.reasons.keys().map(String::as_str))
        .collect();
    reasons.sort_unstable();
    reasons.dedup();
    md.push_str("\n## How runs ended\n\n| Cell |");
    for reason in &reasons {
        let _ = write!(md, " {} |", text(reason));
    }
    let _ = write!(md, "\n|---|{}\n", "---:|".repeat(reasons.len()));
    for c in &r.cells {
        let _ = write!(md, "| {} |", text(&c.name));
        for reason in &reasons {
            let _ = write!(md, " {} |", c.reasons.get(*reason).copied().unwrap_or(0));
        }
        md.push('\n');
    }

    md.push_str(
        "\n## Checks\n\n| Cell | Passed | Failed | Timed out | Couldn't run (126, 127) |\n\
         |---|---:|---:|---:|---:|\n",
    );
    for c in &r.cells {
        let k = c.checks;
        let _ = writeln!(
            md,
            "| {} | {} | {} | {} | {} |",
            text(&c.name),
            k.passed,
            k.failed,
            k.timed_out,
            k.could_not_run
        );
    }

    md.push_str(
        "\n## Harness signals\n\nMean per run. Tool errors are calls that ran and failed; \
         denied calls are those a hook or the permission policy stopped.\n\n\
         | Cell | Tool calls | Tool errors | Denied calls | Compactions | Plan refreshes | Hook denials | Warnings |\n\
         |---|---:|---:|---:|---:|---:|---:|---:|\n",
    );
    for c in &r.cells {
        let s = &c.signals;
        let _ = writeln!(
            md,
            "| {} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1} |",
            text(&c.name),
            s.tool_calls,
            s.tool_errors,
            s.tool_denials,
            s.compactions,
            s.plan_refreshes,
            s.hook_denials,
            s.warnings
        );
    }
    md.push_str("\nBy tool, and tool errors by tool and failure:\n\n");
    for c in &r.cells {
        let s = &c.signals;
        let _ = writeln!(
            md,
            "- **{}**: calls {}; errors {}",
            text(&c.name),
            per_key(&s.tool_calls_by_tool),
            per_key(&s.tool_errors_by_reason)
        );
    }

    md.push_str("\n## Per task\n\nPasses out of runs.\n\n| Task |");
    for name in &cell_names {
        let _ = write!(md, " {} |", text(name));
    }
    let _ = write!(md, "\n|---|{}\n", "---:|".repeat(cell_names.len()));
    for row in &r.per_task {
        let _ = write!(md, "| {} |", text(&row.task));
        for name in &cell_names {
            match row.cells.get(*name) {
                Some(p) => {
                    let _ = write!(md, " {}/{} |", p.passed, p.runs);
                }
                None => md.push_str(" – |"),
            }
        }
        md.push('\n');
    }

    let _ = write!(
        md,
        "\n## Paired comparison\n\nPer task, over the repeats both cells have, the cell that \
         passed more often wins. The sign test is exact and two-sided, over the tasks where \
         the two cells differ; with fewer than {MIN_TASKS_FOR_SIGNIFICANCE} such tasks it \
         can't reach p < 0.05.\n\n"
    );
    if r.pairs.is_empty() {
        md.push_str("One cell: nothing to compare.\n");
    } else {
        md.push_str(
            "| A | B | A wins | B wins | Ties | Sign test p |\n|---|---|---:|---:|---:|---:|\n",
        );
        for p in &r.pairs {
            let few = if p.a_wins + p.b_wins < MIN_TASKS_FOR_SIGNIFICANCE {
                " (too few differing tasks)"
            } else {
                ""
            };
            let _ = writeln!(
                md,
                "| {} | {} | {} | {} | {} | {:.3}{few} |",
                text(&p.a),
                text(&p.b),
                p.a_wins,
                p.b_wins,
                p.ties,
                p.p_value
            );
        }
    }
    md
}

/// `1 run`, `2 runs`.
pub(crate) fn count(n: usize, word: &str) -> String {
    format!("{n} {word}{}", if n == 1 { "" } else { "s" })
}

fn percent(x: f64) -> String {
    format!("{:.1}%", x * 100.0)
}

fn spread(s: Spread, decimals: usize) -> String {
    format!("{:.decimals$} / {:.decimals$}", s.mean, s.median)
}

fn per_key(map: &BTreeMap<String, f64>) -> String {
    if map.is_empty() {
        return "none".into();
    }
    map.iter()
        .map(|(k, v)| format!("{} {v:.1}", text(k)))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Text for the report: HTML and Markdown characters escaped, one line, at most 200
/// characters. Strings from runs are untrusted.
fn text(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars().take(200) {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\n' | '\r' | '\t' => out.push(' '),
            '\\' | '`' | '*' | '_' | '[' | ']' | '|' | '#' | '!' | '~' | '(' | ')' => {
                out.push('\\');
                out.push(c);
            }
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// Reads the `run.json` files of several output directories, names each one's cells
/// after its directory (`<dir>/<cell>`), and writes one report to `to`. Two builds of
/// `ch` run into two directories compare this way, task by task. The directories must
/// hold the same tasks (by id and contents), base config and cells; with
/// `allow_different_config`, the config and cells may differ, and the report says how.
pub fn compare(
    dirs: &[std::path::PathBuf],
    to: &Path,
    allow_different_config: bool,
) -> anyhow::Result<Results> {
    let mut records = Vec::new();
    let mut labels: Vec<String> = Vec::new();
    let mut found_per_dir = Vec::new();
    for dir in dirs {
        let base = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "out".into());
        let mut label = base.clone();
        let mut n = 1;
        while labels.contains(&label) {
            n += 1;
            label = format!("{base}#{n}");
        }
        labels.push(label.clone());
        let found = load_all(dir)?;
        if found.is_empty() {
            bail!("{} has no runs to compare", dir.display());
        }
        found_per_dir.push((label, found));
    }
    let (fatal, allowed) = differences(&found_per_dir);
    if !fatal.is_empty() || (!allowed.is_empty() && !allow_different_config) {
        let mut all = fatal.clone();
        all.extend(allowed.iter().cloned());
        bail!(
            "these directories don't measure the same thing:\n- {}{}",
            all.join("\n- "),
            if fatal.is_empty() {
                "\nPass --allow-different-config to compare them anyway."
            } else {
                ""
            }
        );
    }
    for (label, found) in found_per_dir {
        records.extend(found.into_iter().map(|mut r| {
            r.cell = format!("{label}/{}", r.cell);
            r
        }));
    }
    let mut results = build(&records);
    results.notes.push(
        "The directories' runs were made separately, not interleaved, so a change in the \
         model API between them counts as a difference between the cells."
            .into(),
    );
    results.notes.extend(
        allowed
            .into_iter()
            .map(|d| format!("Compared although {d}.")),
    );
    std::fs::create_dir_all(to).with_context(|| format!("creating {}", to.display()))?;
    write_results(to, &results)
}

/// What differs between the directories: what can't be compared at all (the tasks),
/// and what only `--allow-different-config` lets through (the base config and cells).
fn differences(dirs: &[(String, Vec<RunRecord>)]) -> (Vec<String>, Vec<String>) {
    use std::collections::BTreeSet;
    let mut fatal = Vec::new();
    let mut notes = Vec::new();
    let Some((first_label, first)) = dirs.first() else {
        return (fatal, notes);
    };
    let tasks = |rs: &[RunRecord]| -> BTreeMap<String, BTreeSet<(String, String, String)>> {
        let mut out: BTreeMap<String, BTreeSet<_>> = BTreeMap::new();
        for r in rs {
            let f = &r.fingerprint;
            out.entry(r.task.clone()).or_default().insert((
                f.task.clone(),
                f.repo.clone(),
                f.verify.clone(),
            ));
        }
        out
    };
    let configs = |rs: &[RunRecord]| -> BTreeSet<String> {
        rs.iter().map(|r| r.fingerprint.config.clone()).collect()
    };
    let cells = |rs: &[RunRecord]| -> BTreeMap<String, Vec<String>> {
        rs.iter()
            .map(|r| (r.cell.clone(), r.settings.clone()))
            .collect()
    };
    let (t0, c0, s0) = (tasks(first), configs(first), cells(first));
    for (label, rs) in &dirs[1..] {
        let (t, c, s) = (tasks(rs), configs(rs), cells(rs));
        let ids0: BTreeSet<&String> = t0.keys().collect();
        let ids: BTreeSet<&String> = t.keys().collect();
        if ids0 != ids {
            fatal.push(format!(
                "{first_label} has tasks {}, {label} has {}",
                list(&ids0),
                list(&ids)
            ));
        }
        for id in ids0.intersection(&ids) {
            if t0[*id] != t[*id] {
                fatal.push(format!(
                    "task {id} (its task.toml, repo/ or verify/) differs between {first_label} \
                     and {label}"
                ));
            }
        }
        if c0 != c {
            notes.push(format!(
                "the base config differs between {first_label} and {label}"
            ));
        }
        if s0 != s {
            let names0: BTreeSet<&String> = s0.keys().collect();
            let names: BTreeSet<&String> = s.keys().collect();
            if names0 != names {
                notes.push(format!(
                    "{first_label} has cells {}, {label} has {}",
                    list(&names0),
                    list(&names)
                ));
            }
            for name in names0.intersection(&names) {
                if s0[*name] != s[*name] {
                    notes.push(format!(
                        "cell {name} has settings {:?} in {first_label} and {:?} in {label}",
                        s0[*name], s[*name]
                    ));
                }
            }
        }
    }
    (fatal, notes)
}

fn list(items: &std::collections::BTreeSet<&String>) -> String {
    let items: Vec<&str> = items.iter().map(|s| s.as_str()).collect();
    if items.is_empty() {
        "none".into()
    } else {
        items.join(", ")
    }
}

/// Reads every `run.json` under `out`, and writes `report.md` and `results.json` there.
pub fn write(out: &Path) -> anyhow::Result<Results> {
    let records = load_all(out)?;
    if records.is_empty() {
        bail!(
            "{} has no runs to report (no runs/*/*/*/run.json)",
            out.display()
        );
    }
    write_results(out, &build(&records))
}

fn write_results(out: &Path, results: &Results) -> anyhow::Result<Results> {
    let results = results.clone();
    let md = out.join("report.md");
    std::fs::write(&md, markdown(&results)).with_context(|| format!("writing {}", md.display()))?;
    let json = out.join("results.json");
    std::fs::write(&json, serde_json::to_string_pretty(&results)? + "\n")
        .with_context(|| format!("writing {}", json.display()))?;
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{Signals, Usage};
    use crate::record::tests::record;

    fn run(task: &str, cell: &str, repeat: u32, passed: bool) -> RunRecord {
        let mut r = record(task, cell, repeat, passed);
        r.settings = vec![format!("x={cell}")];
        r
    }

    #[test]
    fn cell_stats_cover_rates_spreads_reasons_checks_and_signals() {
        let mut a = run("t1", "c", 1, true);
        a.cost = 0.1;
        a.reported.steps = 10;
        a.usage = Usage {
            input_tokens: 1000,
            output_tokens: 100,
            cache_read_tokens: 4000,
            cache_write_tokens: 50,
        };
        a.wall_ms = 30_000;
        a.signals = Signals {
            tool_calls: BTreeMap::from([("bash".into(), 3), ("edit_file".into(), 1)]),
            tool_errors: BTreeMap::from([("edit_file:not_found".into(), 1)]),
            tool_denials: BTreeMap::from([("bash".into(), 3)]),
            compactions: 2,
            plan_refreshes: 1,
            hook_denials: 0,
            warnings: 1,
        };
        let mut b = run("t2", "c", 1, false);
        b.cost = 0.3;
        b.reported.steps = 20;
        b.reason = "step_limit".into();
        b.wall_ms = 90_000;
        b.verify.timed_out = true;
        b.verify.exit_code = None;
        b.signals.tool_calls = BTreeMap::from([("bash".into(), 1)]);
        let mut c = run("t3", "c", 1, true);
        c.cost = 0.5;
        c.reported.steps = 3;
        c.reason = "timeout".into();
        let mut d = run("t4", "c", 1, false);
        d.verify.exit_code = Some(127);
        d.cost = 1.5;
        d.fingerprint.ch = "other".into();

        let results = build(&[a, b, c, d]);
        let s = &results.cells[0];
        assert_eq!((s.runs, s.passed), (4, 2));
        assert_eq!(s.pass_rate, 0.5);
        let [lo, hi] = s.pass_rate_ci95;
        assert!(lo < 0.5 && hi > 0.5 && lo > 0.0 && hi < 1.0);
        // Costs 0.1, 0.3, 0.5 and 1.5: the mean (0.6) and the median (0.4) differ.
        assert!((s.cost.mean - 0.6).abs() < 1e-9);
        assert!((s.cost.median - 0.4).abs() < 1e-9);
        assert_eq!(s.steps.median, 7.0);
        assert!((s.cache_read_tokens.mean - 1000.0).abs() < 1e-9);
        assert_eq!(
            s.checks,
            CheckCounts {
                passed: 2,
                failed: 0,
                timed_out: 1,
                could_not_run: 1
            }
        );
        assert_eq!(s.reasons["done"], 2);
        assert_eq!(s.fingerprints, 2);
        assert!((s.signals.tool_calls - 1.25).abs() < 1e-9);
        assert!((s.signals.tool_denials - 0.75).abs() < 1e-9);
        assert!((s.signals.tool_errors_by_reason["edit_file:not_found"] - 0.25).abs() < 1e-9);
        assert_eq!(s.settings, ["x=c"]);

        let md = markdown(&results);
        assert!(md.contains("| c | 0.6000 / 0.4000 |"), "{md}");
        assert!(md.contains("| c | 2 | 0 | 1 | 1 |"), "{md}");
        assert!(
            md.contains("**Note:** the runs of c were made from 2"),
            "{md}"
        );
    }

    #[test]
    fn pairs_compare_tasks_over_common_repeats() {
        let mut records = Vec::new();
        // t1: A passes 2/2, B 1/2: A wins. t2: both 1/2: tie. t3: A 0/2, B 2/2: B wins.
        // t4: A 2/2, B 0/2: A wins. t5 only ran in A: left out.
        for (task, a, b) in [
            ("t1", [true, true], [true, false]),
            ("t2", [true, false], [false, true]),
            ("t3", [false, false], [true, true]),
            ("t4", [true, true], [false, false]),
        ] {
            for (i, (pa, pb)) in a.iter().zip(&b).enumerate() {
                records.push(run(task, "a", i as u32 + 1, *pa));
                records.push(run(task, "b", i as u32 + 1, *pb));
            }
        }
        records.push(run("t5", "a", 1, true));
        // t6: A has an extra repeat that B lacks; over the common one they tie.
        records.push(run("t6", "a", 1, false));
        records.push(run("t6", "a", 2, true));
        records.push(run("t6", "a", 3, true));
        records.push(run("t6", "b", 1, false));
        let results = build(&records);
        assert_eq!(results.tasks, 6);
        let p = &results.pairs[0];
        assert_eq!((p.a.as_str(), p.b.as_str()), ("a", "b"));
        assert_eq!((p.a_wins, p.b_wins, p.ties), (2, 1, 2));
        assert_eq!(p.p_value, 1.0);
        let t1 = &results.per_task[0];
        assert_eq!(t1.cells["a"], PassCount { passed: 2, runs: 2 });
        assert!(!results.per_task[4].cells.contains_key("b"));
        let md = markdown(&results);
        assert!(md.contains("1.000 (too few differing tasks)"), "{md}");
    }

    #[test]
    fn markdown_has_every_section() {
        let mut records = vec![
            run("t1", "planning=on", 1, true),
            run("t1", "planning=off", 1, false),
            run("t2", "planning=on", 1, true),
        ];
        records[1].reason = "timeout".into();
        records[1].settings = vec![r#"plugins.disable=["planning"]"#.into()];
        let md = markdown(&build(&records));
        for heading in [
            "## Cells",
            "## Pass rate",
            "## Cost and effort",
            "## How runs ended",
            "## Checks",
            "## Harness signals",
            "## Per task",
            "## Paired comparison",
        ] {
            assert!(md.contains(heading), "{heading}\n{md}");
        }
        assert!(md.contains("| planning=on | 2 | 2 | 100.0% |"), "{md}");
        assert!(md.contains("| t2 | – | 1/1 |"), "{md}");
        assert!(md.contains(r#"plugins.disable=\["planning"\]"#), "{md}");
        assert!(md.contains("| done | timeout |"), "{md}");
    }

    #[test]
    fn strings_from_runs_are_escaped() {
        let mut r = run("t1", "c", 1, true);
        r.reason = "done|<img src=x onerror=alert(1)>\n# Heading".into();
        r.signals.tool_calls = BTreeMap::from([("[link](http://x)`".into(), 1)]);
        let md = markdown(&build(&[r]));
        assert!(!md.contains("<img"), "{md}");
        assert!(
            md.contains("done\\|&lt;img src=x onerror=alert\\(1\\)&gt; \\# Heading"),
            "{md}"
        );
        assert!(md.contains("\\[link\\]\\(http://x\\)\\`"), "{md}");
        assert!(!md.contains("\n# Heading"), "{md}");
    }

    #[test]
    fn writes_both_files_and_refuses_an_empty_directory() {
        let out = tempfile::tempdir().unwrap();
        assert!(write(out.path()).is_err());
        let r = run("t1", "base", 1, true);
        let dir = crate::record::run_dir(out.path(), "t1", "base", 1);
        std::fs::create_dir_all(&dir).unwrap();
        r.save(&dir).unwrap();
        let results = write(out.path()).unwrap();
        assert_eq!(results.runs, 1);
        let json: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(out.path().join("results.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(json["cells"][0]["pass_rate"], 1.0);
        let md = std::fs::read_to_string(out.path().join("report.md")).unwrap();
        assert!(md.contains("One cell: nothing to compare."), "{md}");
    }

    #[test]
    fn compare_names_cells_after_their_directories() {
        let root = tempfile::tempdir().unwrap();
        let save = |dir: &str, r: RunRecord| {
            let d = crate::record::run_dir(&root.path().join(dir), &r.task, &r.cell, r.repeat);
            std::fs::create_dir_all(&d).unwrap();
            r.save(&d).unwrap();
        };
        // Two directories with the same name, as two builds' `out/` would be.
        for (task, before, after) in [("t1", false, true), ("t2", false, true), ("t3", true, true)]
        {
            save("a/out", run(task, "base", 1, before));
            save("b/out", run(task, "base", 1, after));
        }
        let to = root.path().join("compared");
        let dirs = [root.path().join("a/out"), root.path().join("b/out")];
        let results = compare(&dirs, &to, false).unwrap();
        let names: Vec<&str> = results.cells.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["out#2/base", "out/base"]);
        let p = &results.pairs[0];
        assert_eq!((p.a_wins, p.b_wins, p.ties), (2, 0, 1));
        let md = std::fs::read_to_string(to.join("report.md")).unwrap();
        assert!(md.contains("not interleaved"), "{md}");
        assert!(compare(&[root.path().join("missing"), dirs[0].clone()], &to, false).is_err());
    }

    #[test]
    fn compare_refuses_directories_that_measure_different_things() {
        let root = tempfile::tempdir().unwrap();
        let save = |dir: &str, r: RunRecord| {
            let d = crate::record::run_dir(&root.path().join(dir), &r.task, &r.cell, r.repeat);
            std::fs::create_dir_all(&d).unwrap();
            r.save(&d).unwrap();
        };
        let dirs = |a: &str, b: &str| [root.path().join(a), root.path().join(b)];
        let to = root.path().join("compared");
        save("x", run("t1", "base", 1, true));
        // Other settings for the cell: refused, unless allowed, and then reported.
        let mut other = run("t1", "base", 1, true);
        other.settings = vec!["agent.max_steps=5".into()];
        save("y", other);
        let err = compare(&dirs("x", "y"), &to, false).unwrap_err();
        assert!(format!("{err}").contains("cell base has settings"), "{err}");
        assert!(
            format!("{err}").contains("--allow-different-config"),
            "{err}"
        );
        let results = compare(&dirs("x", "y"), &to, true).unwrap();
        assert!(
            results
                .notes
                .iter()
                .any(|n| n.contains("cell base has settings"))
        );
        // Another task, or the same task with other contents: never comparable.
        save("z", run("t2", "base", 1, true));
        let err = compare(&dirs("x", "z"), &to, true).unwrap_err();
        assert!(
            format!("{err}").contains("x has tasks t1, z has t2"),
            "{err}"
        );
        let mut changed = run("t1", "base", 1, true);
        changed.fingerprint.repo = "other".into();
        save("w", changed);
        let err = compare(&dirs("x", "w"), &to, true).unwrap_err();
        assert!(format!("{err}").contains("task t1"), "{err}");
    }
}
