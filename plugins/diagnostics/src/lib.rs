//! Diagnostics (plugin `diagnostics`): tells the model about problems its own edits
//! introduced, once per step. See ADR 0003.
//!
//! - `pre_tool` on `edit_file` / `write_file` keeps the file's content before this
//!   step's first edit of it; `post_tool` marks the files that a call changed.
//! - `before_model` runs the file's checker on that content (**before**) and on the file
//!   as it is now (**after**), and adds what `after` has that neither `before` nor the
//!   turn so far had as a reminder on that one request. Findings match on their message,
//!   not their line, so a problem the edit only moved isn't reported.
//! - `turn_end`, when the model says it is done: if problems the turn introduced are
//!   still there, one note lists them and the turn continues. At most once per turn.
//!
//! The plugin knows no language. Checkers come from config, one command per file
//! extension, reading the file on stdin; with none configured it adds no hook. A
//! finding is an output line that starts with a location (`file:L:C: message`,
//! `file(L,C): message`).
//!
//! A checker runs only when the `safety` plugin's policy would run the same command as a
//! `bash` call without asking, and only while the model has `bash`: diagnostics never
//! runs a program the model couldn't run itself without the user's approval.
//!
//! ```toml
//! [plugins.diagnostics]
//! timeout_secs = 10          # 1–60
//! max_report_chars = 2000    # 200–20000
//! max_files_per_step = 10    # 1–50
//! nudge_at_turn_end = true
//!
//! [plugins.diagnostics.checkers.py]   # by extension; `enabled = false` turns one off
//! command = "my-linter --stdin --filename={path}"
//! findings_exit = [1]                 # exit codes that mean "findings"
//! ```
//!
//! `{path}` is the file's path relative to the workspace, passed to the shell as
//! `"$CH_PATH"`. A checker whose program can't be found, or that fails three runs in a
//! row, is off for the session, with a warning to the user and nothing to the model.

mod checker;
mod findings;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::Read as _;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use anyhow::bail;
use ch_kernel::reexports::async_trait;
use ch_kernel::{
    AgentEvent, Cx, Hook, HookDecision, HookEvent, HookPoint, Plugin, PluginManifest, RegisterCx,
    TerminationReason,
};
use ch_safety::{Decision, PathGuard, Policy, content_hash};
use serde::Deserialize;
use serde_json::{Value, json};

pub use checker::{Checker, Failure};
pub use findings::{Counts, Finding, add, beyond, counts, parse};

/// Plugin id, also the hook id and the `source` of its events.
pub const ID: &str = "diagnostics";

/// The reminder after a step whose edits introduced problems.
const INTRODUCED: &str = include_str!("../../../prompts/diagnostics-introduced.md");

/// The note when the model ends a turn with problems the turn introduced.
const TURN_END: &str = include_str!("../../../prompts/diagnostics-turn-end.md");

/// The tools whose changes are checked.
const EDIT_TOOLS: &[&str] = &["edit_file", "write_file"];

/// Larger files aren't checked.
const MAX_FILE_BYTES: u64 = 1 << 20;

/// Failed runs in a row that turn a checker off for the session.
const MAX_FAILURES: u32 = 3;

/// Cached results kept before the cache starts over.
const CACHE_ENTRIES: usize = 512;

/// Files followed through a turn for its end; later ones are checked per step only.
const MAX_TURN_FILES: usize = 100;

/// `[plugins.diagnostics]`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DiagnosticsConfig {
    /// Per checker run, 1–60. A project may set it without trust, hence the cap.
    pub timeout_secs: u64,
    /// The reminder is cut to about this many characters, 200–20000.
    pub max_report_chars: usize,
    /// Changed files checked per step, 1–50; the reminder says how many were left out.
    pub max_files_per_step: usize,
    /// Whether the end of a turn lists the problems it introduced that are still there.
    pub nudge_at_turn_end: bool,
    /// By extension (lowercase, no dot).
    pub checkers: BTreeMap<String, CheckerConfig>,
}

impl Default for DiagnosticsConfig {
    fn default() -> Self {
        Self {
            timeout_secs: 10,
            max_report_chars: 2000,
            max_files_per_step: 10,
            nudge_at_turn_end: true,
            checkers: BTreeMap::new(),
        }
    }
}

/// `[plugins.diagnostics.checkers.<ext>]`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CheckerConfig {
    /// `false` turns off a checker another config layer added.
    pub enabled: bool,
    /// Reads the file on stdin. `{path}` is the path relative to the workspace.
    pub command: Option<String>,
    /// Whether the command reads the file on stdin, the only kind supported for now: a
    /// checker that reads the file on disk can't see the content from before the edit.
    pub stdin: bool,
    /// Exit codes that mean "findings". Any other non-zero exit is a failure, and its
    /// output never reaches the model.
    pub findings_exit: Vec<i32>,
}

impl Default for CheckerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            command: None,
            stdin: true,
            findings_exit: vec![1],
        }
    }
}

impl DiagnosticsConfig {
    /// The settings a project may set without trust, brought into range with a warning
    /// rather than an error, so a repository's config can't stop `ch` from starting.
    pub fn clamped(mut self) -> Self {
        fn clamp<T: Copy + Ord + std::fmt::Display>(name: &str, value: &mut T, min: T, max: T) {
            let fixed = (*value).clamp(min, max);
            if fixed != *value {
                tracing::warn!(
                    "plugins.diagnostics.{name} = {value} is outside {min}–{max}; using {fixed}"
                );
                *value = fixed;
            }
        }
        clamp("timeout_secs", &mut self.timeout_secs, 1, 60);
        clamp("max_report_chars", &mut self.max_report_chars, 200, 20_000);
        clamp("max_files_per_step", &mut self.max_files_per_step, 1, 50);
        self
    }

    /// The configured checker for each extension.
    pub fn checkers(&self) -> anyhow::Result<BTreeMap<String, Checker>> {
        let timeout = Duration::from_secs(self.timeout_secs);
        let mut checkers = BTreeMap::new();
        for (ext, c) in &self.checkers {
            let ext = ext.trim_start_matches('.').to_ascii_lowercase();
            let field = format!("plugins.diagnostics.checkers.{ext}");
            if !c.enabled {
                continue;
            }
            if !c.stdin {
                bail!(
                    "{field}.stdin = false isn't supported yet: the checker has to read the \
                     file on stdin so the content from before an edit can be checked too"
                );
            }
            if c.findings_exit.is_empty() || c.findings_exit.iter().any(|&e| e <= 0 || e >= 126) {
                bail!("{field}.findings_exit must list exit codes between 1 and 125");
            }
            match c.command.as_deref().map(str::trim) {
                Some("") => bail!("{field}.command is empty; use `enabled = false` to turn it off"),
                Some(command) => {
                    let checker = Checker::new(command, c.findings_exit.clone(), timeout);
                    checkers.insert(ext, checker);
                }
                None => bail!("{field} needs a `command`"),
            }
        }
        Ok(checkers)
    }
}

/// Registers the hook.
pub struct DiagnosticsPlugin;

/// The plugin, for the CLI's plugin list.
pub fn plugin() -> Arc<dyn Plugin> {
    Arc::new(DiagnosticsPlugin)
}

impl Plugin for DiagnosticsPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest::new(
            ID,
            "Runs configured checkers on edited files and tells the model what its edits broke",
        )
        .version(env!("CARGO_PKG_VERSION"))
        .provides(&["hook:diagnostics"])
        .project_settings(&["timeout_secs", "max_report_chars", "max_files_per_step"])
    }

    fn register(&self, cx: &mut RegisterCx<'_>) -> anyhow::Result<()> {
        let config: DiagnosticsConfig = cx.config()?;
        let hook = DiagnosticsHook::new(&config)?;
        // Nothing to run: no hook, so nothing else is needed either.
        if !hook.checkers.is_empty() {
            cx.add_hook(hook);
        }
        Ok(())
    }
}

/// A file's content as the checkers see it.
#[derive(Debug, Clone, PartialEq)]
enum Content {
    /// The file doesn't exist (yet).
    Absent,
    Text(String),
    /// Too large, not UTF-8, not a regular file, or no longer inside the workspace: not
    /// checked.
    Unusable,
}

impl Content {
    /// Reads `rel` if it is still a regular file inside the workspace. The path is
    /// resolved again, since it may have become a symlink since the edit, and the file is
    /// opened without following a symlink or waiting on a FIFO.
    fn read(guard: &PathGuard, rel: &str) -> Self {
        let path = guard.root().join(rel);
        if guard.resolve(rel).ok().as_deref() != Some(path.as_path()) {
            return Self::Unusable;
        }
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let file = match options.open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Self::Absent,
            Err(_) => return Self::Unusable,
        };
        if !file.metadata().is_ok_and(|m| m.is_file()) {
            return Self::Unusable;
        }
        let mut bytes = Vec::new();
        match file.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes) {
            Ok(n) if n as u64 <= MAX_FILE_BYTES => {
                String::from_utf8(bytes).map_or(Self::Unusable, Self::Text)
            }
            _ => Self::Unusable,
        }
    }

    fn text(&self) -> Option<&str> {
        match self {
            Self::Text(t) => Some(t),
            _ => None,
        }
    }
}

/// A file changed in this turn.
struct TurnFile {
    /// Its content before the turn's first change (`Absent` or `Text`).
    start: Content,
    /// Findings reported this turn and not fixed since.
    reported: Counts,
}

#[derive(Default)]
struct State {
    /// Each file's content before this step's first edit of it.
    before: HashMap<String, Content>,
    /// Files an edit or write changed in this step.
    changed: BTreeSet<String>,
    /// The files changed in this turn.
    turn: BTreeMap<String, TurnFile>,
    /// Whether this turn's end has already listed problems.
    nudged: bool,
    /// Extensions whose checker passed its probe.
    probed: HashSet<String>,
    /// Extensions whose checker is off for the session.
    off: HashSet<String>,
    /// Extensions whose checker the permission policy kept from running (warned once).
    held: HashSet<String>,
    /// Whether the user was told that `safety` is missing.
    warned_no_safety: bool,
    /// Failed runs in a row, by extension.
    failures: HashMap<String, u32>,
    /// Findings by extension, path and content hash.
    cache: HashMap<(String, String, [u8; 32]), Arc<Vec<Finding>>>,
}

/// How a changed file is compared.
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// After a step: what is new since the step and the turn so far.
    Step,
    /// At the end of the turn: what the turn introduced that is still there.
    TurnEnd,
}

/// The `user_prompt`, `pre_tool`, `post_tool`, `before_model` and `turn_end` hook.
pub struct DiagnosticsHook {
    checkers: BTreeMap<String, Checker>,
    max_report_chars: usize,
    max_files_per_step: usize,
    nudge_at_turn_end: bool,
    state: Mutex<State>,
}

impl DiagnosticsHook {
    pub fn new(config: &DiagnosticsConfig) -> anyhow::Result<Self> {
        let config = config.clone().clamped();
        Ok(Self {
            checkers: config.checkers()?,
            max_report_chars: config.max_report_chars,
            max_files_per_step: config.max_files_per_step,
            nudge_at_turn_end: config.nudge_at_turn_end,
            state: Mutex::default(),
        })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        // Every update leaves the state consistent, so a poisoned lock is still usable.
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The `safety` plugin's path guard and policy, without which nothing is checked.
    fn safety(&self, cx: &Cx) -> Option<(Arc<PathGuard>, Arc<Policy>)> {
        let found = cx
            .services
            .get::<PathGuard>()
            .zip(cx.services.get::<Policy>());
        if found.is_none() && !std::mem::replace(&mut self.state().warned_no_safety, true) {
            cx.emit(AgentEvent::Warning {
                message: "diagnostics: checkers are configured but the `safety` plugin is off, \
                          so nothing is checked"
                    .into(),
            });
        }
        found
    }

    /// The extension of `rel`, if it has a checker.
    fn checked_ext(&self, rel: &str) -> Option<String> {
        let ext = Path::new(rel).extension()?.to_str()?.to_ascii_lowercase();
        self.checkers.contains_key(&ext).then_some(ext)
    }

    /// Turns the checker for `ext` off for the session and tells the user why.
    fn turn_off(&self, ext: &str, why: &str, cx: &Cx) {
        if self.state().off.insert(ext.to_string()) {
            cx.emit(AgentEvent::Warning {
                message: format!("diagnostics: the .{ext} checker is off for this session: {why}"),
            });
        }
    }

    /// Whether the model could run the checker's command for `rel` itself, as a `bash`
    /// call that runs without asking. Tells the user once per checker when it couldn't.
    ///
    /// Both the command that runs (with `"$CH_PATH"`, which no file name can change) and
    /// the command as the model would write it (with the path, which path rules match)
    /// must be allowed.
    fn permitted(&self, ext: &str, rel: &str, policy: &Policy, cx: &Cx) -> bool {
        let checker = &self.checkers[ext];
        let command = checker.command(rel);
        let decide = |command: &str| policy.decide("bash", &json!({ "command": command }), false);
        let why = if !cx.tools.iter().any(|t| t.name == "bash") {
            "the model has no `bash`".to_string()
        } else {
            let decision = match decide(&checker.script()) {
                Decision::Allow => decide(&command),
                held => held,
            };
            match decision {
                Decision::Allow => return true,
                Decision::Ask { .. } => format!(
                    "the permission policy would ask before running `{command}`; allow it \
                     with a rule in plugins.safety.allow, or use bypass mode"
                ),
                Decision::Deny { .. } => {
                    format!("the permission policy denies running `{command}`")
                }
            }
        };
        if self.state().held.insert(ext.to_string()) {
            cx.emit(AgentEvent::Warning {
                message: format!("diagnostics: the .{ext} checker didn't run: {why}"),
            });
        }
        false
    }

    /// Whether the checker for `ext` can run, probing it the first time.
    async fn ready(&self, ext: &str, cx: &Cx) -> bool {
        {
            let state = self.state();
            if state.off.contains(ext) {
                return false;
            }
            if state.probed.contains(ext) {
                return true;
            }
        }
        match self.checkers[ext].probe(cx).await {
            Ok(()) => {
                self.state().probed.insert(ext.to_string());
                true
            }
            Err(Failure::Cancelled) => false,
            Err(Failure::Failed(why)) => {
                self.turn_off(ext, &why, cx);
                false
            }
        }
    }

    /// The findings for `content` as the file `rel`, from the cache or a run. `Absent`
    /// has none; `None` means there is no result to compare.
    async fn findings(
        &self,
        ext: &str,
        rel: &str,
        content: &Content,
        cx: &Cx,
    ) -> Option<Arc<Vec<Finding>>> {
        let text = match content {
            Content::Absent => return Some(Arc::default()),
            Content::Text(text) => text,
            Content::Unusable => return None,
        };
        let key = (
            ext.to_string(),
            rel.to_string(),
            content_hash(text.as_bytes()),
        );
        if let Some(found) = self.state().cache.get(&key) {
            return Some(found.clone());
        }
        match self.checkers[ext].run(cx, rel, text).await {
            Ok(found) => {
                let found = Arc::new(found);
                let mut state = self.state();
                state.failures.remove(ext);
                if state.cache.len() >= CACHE_ENTRIES {
                    state.cache.clear();
                }
                state.cache.insert(key, found.clone());
                Some(found)
            }
            Err(Failure::Cancelled) => None,
            Err(Failure::Failed(why)) => {
                tracing::debug!("diagnostics: {why}");
                let failures = {
                    let mut state = self.state();
                    let n = state.failures.entry(ext.to_string()).or_default();
                    *n += 1;
                    *n
                };
                if failures >= MAX_FAILURES {
                    let why = format!("{MAX_FAILURES} runs in a row failed; the last: {why}");
                    self.turn_off(ext, &why, cx);
                }
                None
            }
        }
    }

    /// The new findings in `rel`, as lines for the model. `None` when the file isn't
    /// checked or a run gave no result: nothing is reported without every side.
    async fn compare(
        &self,
        mode: Mode,
        rel: &str,
        before: &Content,
        now: &Content,
        policy: &Policy,
        cx: &Cx,
    ) -> Option<Vec<String>> {
        let ext = self.checked_ext(rel)?;
        if !self.permitted(&ext, rel, policy, cx) || !self.ready(&ext, cx).await {
            return None;
        }
        let start = match self.state().turn.get(rel) {
            Some(t) => t.start.clone(),
            None => before.clone(),
        };
        let after = self.findings(&ext, rel, now, cx).await?;
        let new: Vec<Finding> = match mode {
            Mode::TurnEnd => {
                let start_found = self.findings(&ext, rel, &start, cx).await?;
                beyond(&[&counts(&start_found)], &after)
                    .into_iter()
                    .cloned()
                    .collect()
            }
            Mode::Step => {
                let before_found = self.findings(&ext, rel, before, cx).await?;
                // Without the turn's start, the step's own "before" is the baseline.
                let start_found = if start == *before {
                    Some(before_found.clone())
                } else {
                    self.findings(&ext, rel, &start, cx).await
                };
                let mut state = self.state();
                let mut turn = start_found.map(|f| counts(&f)).unwrap_or_default();
                if let Some(t) = state.turn.get(rel) {
                    for (key, n) in &t.reported {
                        *turn.entry(key.clone()).or_default() += n;
                    }
                }
                let new: Vec<Finding> = beyond(&[&counts(&before_found), &turn], &after)
                    .into_iter()
                    .cloned()
                    .collect();
                if let Some(t) = state.turn.get_mut(rel) {
                    // With nothing new, nothing can be hiding a reported problem that is
                    // gone: it was fixed, and if it comes back it is reported again.
                    if new.is_empty() {
                        let now = counts(&after);
                        t.reported.retain(|key, n| {
                            *n = (*n).min(now.get(key).copied().unwrap_or(0));
                            *n > 0
                        });
                    }
                    add(&mut t.reported, new.iter());
                }
                new
            }
        };
        Some(new.into_iter().map(|f| f.display).collect())
    }

    /// Compares `files` (path, content to compare with) with what is on disk now.
    /// Returns the lines to report and how many files were left out.
    async fn check(
        &self,
        mode: Mode,
        files: Vec<(String, Content)>,
        guard: &PathGuard,
        policy: &Policy,
        cx: &Cx,
    ) -> (Vec<String>, usize) {
        let mut lines = Vec::new();
        let mut skipped = 0;
        let mut checked = 0;
        for (rel, before) in files {
            if cx.cancel.is_cancelled() {
                break;
            }
            let now = Content::read(guard, &rel);
            if now.text().is_none() || now == before {
                continue;
            }
            // The turn's end mostly reuses cached results, so it isn't capped.
            if mode == Mode::Step && checked == self.max_files_per_step {
                skipped += 1;
                continue;
            }
            checked += 1;
            if let Some(new) = self.compare(mode, &rel, &before, &now, policy, cx).await {
                lines.extend(new);
            }
        }
        (lines, skipped)
    }

    async fn before_model(&self, cx: &Cx) -> HookDecision {
        let files: Vec<(String, Content)> = {
            let mut state = self.state();
            let mut before = std::mem::take(&mut state.before);
            let changed = std::mem::take(&mut state.changed);
            let files: Vec<_> = changed
                .into_iter()
                .filter_map(|rel| {
                    let content = before.remove(&rel)?;
                    (content != Content::Unusable).then_some((rel, content))
                })
                .collect();
            for (rel, content) in &files {
                if state.turn.len() < MAX_TURN_FILES || state.turn.contains_key(rel) {
                    state.turn.entry(rel.clone()).or_insert_with(|| TurnFile {
                        start: content.clone(),
                        reported: Counts::new(),
                    });
                }
            }
            files
        };
        if files.is_empty() {
            return HookDecision::Continue;
        }
        let Some((guard, policy)) = self.safety(cx) else {
            return HookDecision::Continue;
        };
        let (lines, skipped) = self.check(Mode::Step, files, &guard, &policy, cx).await;
        if lines.is_empty() {
            return HookDecision::Continue;
        }
        cx.emit_custom(ID, "introduced", json!({ "findings": lines.len() }));
        HookDecision::note(report(INTRODUCED, &lines, skipped, self.max_report_chars))
    }

    async fn turn_end(&self, cx: &Cx) -> HookDecision {
        let files: Vec<(String, Content)> = {
            let state = self.state();
            if state.nudged {
                return HookDecision::Continue;
            }
            let turn = state.turn.iter();
            turn.map(|(rel, t)| (rel.clone(), t.start.clone()))
                .collect()
        };
        if files.is_empty() {
            return HookDecision::Continue;
        }
        let Some((guard, policy)) = self.safety(cx) else {
            return HookDecision::Continue;
        };
        let (lines, skipped) = self.check(Mode::TurnEnd, files, &guard, &policy, cx).await;
        if lines.is_empty() {
            return HookDecision::Continue;
        }
        self.state().nudged = true;
        cx.emit_custom(ID, "turn_end", json!({ "findings": lines.len() }));
        HookDecision::note(report(TURN_END, &lines, skipped, self.max_report_chars))
    }
}

/// `template` with `{findings}` replaced by `lines`, cut to about `max_chars`, and a
/// line for the files that weren't checked.
fn report(template: &str, lines: &[String], skipped: usize, max_chars: usize) -> String {
    let mut shown = String::new();
    let mut count = 0;
    for line in lines {
        if count > 0 && shown.len() + line.len() > max_chars {
            break;
        }
        if count > 0 {
            shown.push('\n');
        }
        shown.push_str(&ch_kernel::util::truncate_middle(line, max_chars));
        count += 1;
    }
    if count < lines.len() {
        shown.push_str(&format!("\n… and {} more.", lines.len() - count));
    }
    let mut text = template.trim_end().replace("{findings}", &shown);
    match skipped {
        0 => {}
        1 => text.push_str("\n(1 more changed file was not checked.)"),
        n => text.push_str(&format!("\n({n} more changed files were not checked.)")),
    }
    text
}

#[async_trait]
impl Hook for DiagnosticsHook {
    fn id(&self) -> &str {
        ID
    }

    fn points(&self) -> &[HookPoint] {
        &[
            HookPoint::UserPrompt,
            HookPoint::PreTool,
            HookPoint::PostTool,
            HookPoint::BeforeModel,
            HookPoint::TurnEnd,
        ]
    }

    async fn on(&self, event: &HookEvent, cx: &Cx) -> HookDecision {
        match event {
            // Every turn starts here, also after `/clear` or `/resume`.
            HookEvent::UserPrompt { .. } => {
                let mut state = self.state();
                state.before.clear();
                state.changed.clear();
                state.turn.clear();
                state.nudged = false;
                HookDecision::Continue
            }
            HookEvent::PreTool { call, .. } if EDIT_TOOLS.contains(&call.name.as_str()) => {
                let Some(raw) = call.input.get("path").and_then(Value::as_str) else {
                    return HookDecision::Continue;
                };
                let Some(guard) = cx.services.get::<PathGuard>() else {
                    let checked = Path::new(raw)
                        .extension()
                        .and_then(|e| e.to_str())
                        .is_some_and(|e| self.checkers.contains_key(&e.to_ascii_lowercase()));
                    if checked {
                        self.safety(cx);
                    }
                    return HookDecision::Continue;
                };
                // The same resolution the tool uses; a path it refuses is never changed.
                let Ok(abs) = guard.resolve(raw) else {
                    return HookDecision::Continue;
                };
                let rel = guard.relative(&abs);
                // pre_tool can run twice for one call: the first snapshot stays.
                if self.checked_ext(&rel).is_some() && !self.state().before.contains_key(&rel) {
                    let content = Content::read(&guard, &rel);
                    self.state().before.entry(rel).or_insert(content);
                }
                HookDecision::Continue
            }
            HookEvent::PostTool {
                call,
                output,
                not_run: false,
            } if !output.is_error && EDIT_TOOLS.contains(&call.name.as_str()) => {
                // The tool's own resolved path, not the model's input.
                if let Some(rel) = output.metadata.get("path").and_then(Value::as_str) {
                    let mut state = self.state();
                    if state.before.contains_key(rel) {
                        state.changed.insert(rel.to_string());
                    }
                }
                HookDecision::Continue
            }
            HookEvent::BeforeModel { .. } => self.before_model(cx).await,
            HookEvent::TurnEnd {
                reason: TerminationReason::Done,
                ..
            } if self.nudge_at_turn_end => self.turn_end(cx).await,
            _ => HookDecision::Continue,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn there_are_no_checkers_until_config_adds_them() {
        assert!(DiagnosticsConfig::default().checkers().unwrap().is_empty());
        let config: DiagnosticsConfig = toml::from_str(
            r#"
            [checkers.".TS"]
            command = "lint --stdin {path}"
            [checkers.md]
            enabled = false
            command = "mdlint -"
            "#,
        )
        .unwrap();
        let checkers = config.checkers().unwrap();
        assert_eq!(checkers.keys().collect::<Vec<_>>(), ["ts"]);
        assert_eq!(checkers["ts"].command("a.ts"), "lint --stdin a.ts");
    }

    #[test]
    fn bad_config_is_refused() {
        for (toml_cfg, needle) in [
            ("[checkers.ts]\nenabled = true", "needs a `command`"),
            ("[checkers.ts]\ncommand = \" \"", "is empty"),
            (
                "[checkers.ts]\ncommand = \"x {path}\"\nstdin = false",
                "stdin = false",
            ),
            (
                "[checkers.ts]\ncommand = \"x\"\nfindings_exit = []",
                "findings_exit",
            ),
            (
                "[checkers.ts]\ncommand = \"x\"\nfindings_exit = [0]",
                "findings_exit",
            ),
            (
                "[checkers.ts]\ncommand = \"x\"\nfindings_exit = [127]",
                "findings_exit",
            ),
        ] {
            let config: DiagnosticsConfig = toml::from_str(toml_cfg).unwrap();
            let err = config.checkers().unwrap_err().to_string();
            assert!(err.contains(needle), "{toml_cfg}: {err}");
        }
    }

    #[test]
    fn limits_a_project_may_set_are_clamped_not_refused() {
        let config: DiagnosticsConfig =
            toml::from_str("timeout_secs = 0\nmax_report_chars = 5\nmax_files_per_step = 999")
                .unwrap();
        let c = config.clamped();
        assert_eq!(
            (c.timeout_secs, c.max_report_chars, c.max_files_per_step),
            (1, 200, 50)
        );
        let big: DiagnosticsConfig = toml::from_str("timeout_secs = 3600").unwrap();
        assert_eq!(big.clamped().timeout_secs, 60);
        assert!(DiagnosticsHook::new(&DiagnosticsConfig::default()).is_ok());
    }

    #[test]
    fn the_report_is_cut_at_whole_lines_and_counts_skipped_files() {
        let lines: Vec<String> = (1..=50)
            .map(|i| format!("a.txt:{i}:1: undefined name `x{i}`"))
            .collect();
        let text = report(INTRODUCED, &lines, 0, 200);
        assert!(
            text.starts_with("Your last edits introduced these problems"),
            "{text}"
        );
        assert!(
            text.contains("\na.txt:1:1: undefined name `x1`\n"),
            "{text}"
        );
        assert!(text.contains("… and 44 more."), "{text}");
        assert!(text.len() < 500, "{}", text.len());
        let one = report(TURN_END, &lines[..1], 1, 200);
        assert!(
            one.ends_with("\n(1 more changed file was not checked.)"),
            "{one}"
        );
        let two = report(TURN_END, &lines[..1], 2, 200);
        assert!(
            two.ends_with("\n(2 more changed files were not checked.)"),
            "{two}"
        );
    }
}
