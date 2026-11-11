//! Diagnostics (plugin `diagnostics`): tells the model about problems its own edits
//! introduced, once per step. See ADR 0003.
//!
//! - `pre_tool` on `edit_file` / `write_file` keeps the file's content before this
//!   step's first edit of it; `post_tool` marks the files that a call changed.
//! - `before_model` runs the file's checker on that content (**before**) and on the file
//!   as it is now (**after**), and adds what `after` has and `before` hasn't as a
//!   reminder on that one request. Findings match on rule and message, not on line, so
//!   a problem the edit only moved isn't reported.
//! - `turn_end`, when the model says it is done: if problems the turn introduced are
//!   still there, one note lists them and the turn continues. At most once per turn.
//!
//! The plugin knows no language. Checkers come from config, one command per file
//! extension, and read the file on stdin; with none configured it does nothing. Any
//! output line with a location (`path:L:C: message`, `path(L,C): message`) is a
//! finding. Nothing runs while the model has no `bash`: an agent without bash runs no
//! programs.
//!
//! ```toml
//! [plugins.diagnostics]
//! timeout_secs = 10
//! max_report_chars = 2000
//! max_files_per_step = 10
//! nudge_at_turn_end = true
//!
//! [plugins.diagnostics.checkers.ts]   # by extension; `enabled = false` turns one off
//! command = "npx --no-install eslint --format unix --stdin --stdin-filename={path}"
//! ```
//!
//! `{path}` is the file's shell-quoted path relative to the workspace. A checker whose
//! program can't be found, or that fails three runs in a row, is off for the session,
//! with a warning to the user and nothing to the model.

mod checker;
mod findings;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{Context as _, bail};
use ch_kernel::reexports::async_trait;
use ch_kernel::{
    AgentEvent, Cx, Hook, HookDecision, HookEvent, HookPoint, Plugin, PluginManifest, RegisterCx,
    TerminationReason,
};
use ch_safety::{PathGuard, Policy, content_hash};
use serde::Deserialize;
use serde_json::{Value, json};

pub use checker::Checker;
pub use findings::{Finding, introduced, parse};

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

/// `[plugins.diagnostics]`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DiagnosticsConfig {
    pub timeout_secs: u64,
    /// The reminder is cut to about this many characters.
    pub max_report_chars: usize,
    /// Changed files checked per step; the reminder says how many were left out.
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
    /// Reads the file on stdin. `{path}` is the shell-quoted path relative to the
    /// workspace.
    pub command: Option<String>,
    /// Whether the command reads the file on stdin, the only kind supported for now: a
    /// checker that reads the file on disk can't see the content from before the edit.
    pub stdin: bool,
}

impl Default for CheckerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            command: None,
            stdin: true,
        }
    }
}

impl DiagnosticsConfig {
    /// The configured checker for each extension.
    pub fn checkers(&self) -> anyhow::Result<BTreeMap<String, Checker>> {
        if self.timeout_secs == 0 {
            bail!("plugins.diagnostics.timeout_secs must be at least 1");
        }
        if self.max_files_per_step == 0 {
            bail!("plugins.diagnostics.max_files_per_step must be at least 1");
        }
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
            match c.command.as_deref().map(str::trim) {
                Some("") => bail!("{field}.command is empty; use `enabled = false` to turn it off"),
                Some(command) => {
                    checkers.insert(ext, Checker::new(command, timeout));
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
        .requires(&["safety"])
        .provides(&["hook:diagnostics"])
        .project_settings(&["timeout_secs", "max_report_chars", "max_files_per_step"])
    }

    fn register(&self, cx: &mut RegisterCx<'_>) -> anyhow::Result<()> {
        let config: DiagnosticsConfig = cx.config()?;
        let guard = cx.service::<PathGuard>().context(
            "diagnostics needs the PathGuard service from the `safety` plugin; enable `safety`",
        )?;
        let policy = cx.service::<Policy>();
        cx.add_hook(DiagnosticsHook::new(&config, guard, policy)?);
        Ok(())
    }
}

/// A file's content as the checkers see it.
#[derive(Debug, Clone, PartialEq)]
enum Content {
    /// The file doesn't exist (yet).
    Absent,
    Text(String),
    /// Too large, not UTF-8, or not a regular file: not checked.
    Unusable,
}

impl Content {
    fn read(path: &Path) -> Self {
        match std::fs::metadata(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::Absent,
            Ok(m) if m.is_file() && m.len() <= MAX_FILE_BYTES => std::fs::read_to_string(path)
                .map(Self::Text)
                .unwrap_or(Self::Unusable),
            _ => Self::Unusable,
        }
    }
}

#[derive(Default)]
struct State {
    /// Each file's content before this step's first edit of it.
    before: HashMap<String, Content>,
    /// Files an edit or write changed in this step.
    changed: BTreeSet<String>,
    /// Each file's content before its first change in this turn (`None`: it didn't
    /// exist).
    turn_start: BTreeMap<String, Option<String>>,
    /// Whether this turn's end has already listed problems.
    nudged: bool,
    /// Extensions whose checker passed its probe.
    probed: HashSet<String>,
    /// Extensions whose checker is off for the session.
    off: HashSet<String>,
    /// Failed runs in a row, by extension.
    failures: HashMap<String, u32>,
    /// Findings by command and content hash.
    cache: HashMap<(String, [u8; 32]), Arc<Vec<Finding>>>,
}

/// The `user_prompt`, `pre_tool`, `post_tool`, `before_model` and `turn_end` hook.
pub struct DiagnosticsHook {
    checkers: BTreeMap<String, Checker>,
    guard: Arc<PathGuard>,
    policy: Option<Arc<Policy>>,
    max_report_chars: usize,
    max_files_per_step: usize,
    nudge_at_turn_end: bool,
    state: Mutex<State>,
}

impl DiagnosticsHook {
    pub fn new(
        config: &DiagnosticsConfig,
        guard: Arc<PathGuard>,
        policy: Option<Arc<Policy>>,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            checkers: config.checkers()?,
            guard,
            policy,
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

    /// Checkers run only while the model may run programs itself.
    fn bash_available(&self, cx: &Cx) -> bool {
        cx.tools.iter().any(|t| t.name == "bash")
            && !self.policy.as_ref().is_some_and(|p| p.denies_tool("bash"))
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
            Err(why) => {
                self.turn_off(ext, &why, cx);
                false
            }
        }
    }

    /// The findings for `content` as the file `rel`, from the cache or a run.
    async fn findings(
        &self,
        ext: &str,
        rel: &str,
        content: &str,
        cx: &Cx,
    ) -> Option<Arc<Vec<Finding>>> {
        let checker = &self.checkers[ext];
        let key = (checker.command(rel), content_hash(content.as_bytes()));
        if let Some(found) = self.state().cache.get(&key) {
            return Some(found.clone());
        }
        match checker.run(cx, rel, content).await {
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
            Err(why) => {
                tracing::debug!("diagnostics: {why}");
                let failures = {
                    let mut state = self.state();
                    let n = state.failures.entry(ext.to_string()).or_default();
                    *n += 1;
                    *n
                };
                if failures >= MAX_FAILURES {
                    self.turn_off(
                        ext,
                        &format!("{MAX_FAILURES} runs in a row failed; the last: {why}"),
                        cx,
                    );
                }
                None
            }
        }
    }

    /// What `after` has that `before` hadn't, as lines for the model. `None` when the
    /// file isn't checked or a run failed: nothing is reported without both sides.
    async fn introduced_in(
        &self,
        rel: &str,
        before: &Content,
        after: &str,
        cx: &Cx,
    ) -> Option<Vec<String>> {
        let ext = self.checked_ext(rel)?;
        if !self.ready(&ext, cx).await {
            return None;
        }
        let before = match before {
            Content::Absent => Arc::default(),
            Content::Text(text) => self.findings(&ext, rel, text, cx).await?,
            Content::Unusable => return None,
        };
        let after = self.findings(&ext, rel, after, cx).await?;
        Some(
            introduced(&before, &after)
                .into_iter()
                .map(|f| f.display.clone())
                .collect(),
        )
    }

    /// Checks `files` (path, content before) against what is on disk now.
    async fn check(&self, files: Vec<(String, Content)>, cx: &Cx) -> (Vec<String>, usize) {
        let mut lines = Vec::new();
        let mut skipped = 0;
        let mut checked = 0;
        for (rel, before) in files {
            let Content::Text(now) = Content::read(&self.guard.root().join(&rel)) else {
                continue;
            };
            if before == Content::Text(now.clone()) {
                continue;
            }
            if checked == self.max_files_per_step {
                skipped += 1;
                continue;
            }
            checked += 1;
            if let Some(new) = self.introduced_in(&rel, &before, &now, cx).await {
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
                let start = match content {
                    Content::Text(text) => Some(text.clone()),
                    _ => None,
                };
                state.turn_start.entry(rel.clone()).or_insert(start);
            }
            files
        };
        if files.is_empty() || !self.bash_available(cx) {
            return HookDecision::Continue;
        }
        let (lines, skipped) = self.check(files, cx).await;
        if lines.is_empty() {
            return HookDecision::Continue;
        }
        cx.emit_custom(ID, "introduced", json!({ "findings": lines.len() }));
        let mut text = report(INTRODUCED, &lines, self.max_report_chars);
        if skipped > 0 {
            text.push_str(&format!(
                "\n({skipped} more changed files were not checked in this step.)"
            ));
        }
        HookDecision::note(text)
    }

    async fn turn_end(&self, cx: &Cx) -> HookDecision {
        let files: Vec<(String, Content)> = {
            let state = self.state();
            if state.nudged {
                return HookDecision::Continue;
            }
            state
                .turn_start
                .iter()
                .map(|(rel, start)| {
                    let content = start.clone().map_or(Content::Absent, Content::Text);
                    (rel.clone(), content)
                })
                .collect()
        };
        if files.is_empty() || !self.bash_available(cx) {
            return HookDecision::Continue;
        }
        let (lines, _) = self.check(files, cx).await;
        if lines.is_empty() {
            return HookDecision::Continue;
        }
        self.state().nudged = true;
        cx.emit_custom(ID, "turn_end", json!({ "findings": lines.len() }));
        HookDecision::note(report(TURN_END, &lines, self.max_report_chars))
    }
}

/// `template` with `{findings}` replaced by `lines`, cut to about `max_chars`.
fn report(template: &str, lines: &[String], max_chars: usize) -> String {
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
    template.trim_end().replace("{findings}", &shown)
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
                state.turn_start.clear();
                state.nudged = false;
                HookDecision::Continue
            }
            HookEvent::PreTool { call, .. } if EDIT_TOOLS.contains(&call.name.as_str()) => {
                let Some(raw) = call.input.get("path").and_then(Value::as_str) else {
                    return HookDecision::Continue;
                };
                // The same resolution the tool uses; a path it refuses is never changed.
                let Ok(abs) = self.guard.resolve(raw) else {
                    return HookDecision::Continue;
                };
                let rel = self.guard.relative(&abs);
                // pre_tool can run twice for one call: the first snapshot stays.
                if self.checked_ext(&rel).is_some() && !self.state().before.contains_key(&rel) {
                    let content = Content::read(&abs);
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
            command = "eslint --stdin {path}"
            [checkers.md]
            enabled = false
            command = "mdlint -"
            "#,
        )
        .unwrap();
        let checkers = config.checkers().unwrap();
        assert_eq!(checkers.keys().collect::<Vec<_>>(), ["ts"]);
        assert_eq!(checkers["ts"].command("a.ts"), "eslint --stdin a.ts");
    }

    #[test]
    fn bad_checker_config_is_refused() {
        for (toml_cfg, needle) in [
            ("[checkers.ts]\nenabled = true", "needs a `command`"),
            ("[checkers.ts]\ncommand = \" \"", "is empty"),
            (
                "[checkers.ts]\ncommand = \"tsc {path}\"\nstdin = false",
                "stdin = false",
            ),
            ("timeout_secs = 0", "timeout_secs"),
            ("max_files_per_step = 0", "max_files_per_step"),
        ] {
            let config: DiagnosticsConfig = toml::from_str(toml_cfg).unwrap();
            let err = config.checkers().unwrap_err().to_string();
            assert!(err.contains(needle), "{toml_cfg}: {err}");
        }
    }

    #[test]
    fn the_report_is_cut_at_whole_lines() {
        let lines: Vec<String> = (1..=50)
            .map(|i| format!("a.txt:{i}:1: undefined name `x{i}`"))
            .collect();
        let text = report(INTRODUCED, &lines, 200);
        assert!(
            text.starts_with("Your last edits introduced these problems:\na.txt:1:1:"),
            "{text}"
        );
        assert!(text.contains("… and 44 more."), "{text}");
        assert!(text.ends_with("already does."), "{text}");
        assert!(text.len() < 400, "{}", text.len());
    }
}
