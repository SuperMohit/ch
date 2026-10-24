//! Building the merged configuration.
//!
//! Layers, lowest first: kernel defaults → `<ch home>/config.toml` →
//! `<workspace>/.ch/config.toml` → `--profile` → `--model` → `--set` → `--allow` →
//! `--permission-mode`.
//!
//! Some project settings apply only once the user trusts the project; see
//! [`crate::trust`].

use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use ch_kernel::Config;
use toml::{Table, Value};

use crate::args::Cli;
use crate::trust::{
    ADDITIVE_LISTS, ProjectLayer, ProjectSettings, TRUSTED_ADDITIVE_LISTS, TrustStore, append_list,
    take_list,
};

/// `$CH_HOME`, or `~/.ch`.
pub fn ch_home() -> anyhow::Result<PathBuf> {
    if let Some(v) = std::env::var_os("CH_HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(v));
    }
    dirs::home_dir()
        .map(|h| h.join(".ch"))
        .context("cannot find the home directory; set CH_HOME to choose where ch keeps its files")
}

/// Where things live for one run.
#[derive(Debug, Clone, PartialEq)]
pub struct Paths {
    /// `$CH_HOME` or `~/.ch`.
    pub home: PathBuf,
    /// The project directory (`--cwd` or the current directory).
    pub workspace: PathBuf,
}

impl Paths {
    pub fn user_config(&self) -> PathBuf {
        self.home.join("config.toml")
    }
    pub fn project_config(&self) -> PathBuf {
        self.workspace.join(".ch").join("config.toml")
    }
    pub fn sessions(&self) -> PathBuf {
        self.home.join("sessions")
    }
    pub fn logs(&self) -> PathBuf {
        self.home.join("logs")
    }
}

/// The command-line inputs that affect configuration.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Overrides {
    pub profile: Option<String>,
    pub model: Option<String>,
    pub set: Vec<String>,
    pub allow: Vec<String>,
    pub permission_mode: Option<String>,
    /// Apply the project's restricted settings for this run without asking.
    pub trust_project: bool,
}

impl Overrides {
    pub fn from_cli(cli: &Cli) -> Self {
        Self {
            profile: cli.profile.clone(),
            model: cli.model.clone(),
            set: cli.set.clone(),
            allow: cli.allow.clone(),
            permission_mode: cli.permission_mode.map(|m| m.as_str().to_string()),
            trust_project: cli.trust_project,
        }
    }
}

/// The merged config and which files contributed to it.
#[derive(Debug, Clone)]
pub struct LoadedConfig {
    pub config: Config,
    /// Config files that existed and were merged, lowest layer first.
    pub files: Vec<PathBuf>,
    /// The project's restricted settings, and whether they were applied.
    pub project: ProjectTrust,
}

/// What happened to the project config's restricted settings.
#[derive(Debug, Clone, PartialEq)]
pub enum ProjectTrust {
    /// There are none (or no project config).
    NothingToTrust,
    /// Applied: the project is trusted with these settings.
    Trusted { settings: Table, saved: bool },
    /// Not applied: the user hasn't trusted these settings.
    Untrusted { settings: Table, hash: String },
}

/// Builds the merged config for `paths` with the command-line `overrides`. `settings`
/// says which project settings apply without trust.
pub fn build_config(
    paths: &Paths,
    overrides: &Overrides,
    settings: &ProjectSettings,
) -> anyhow::Result<LoadedConfig> {
    let mut config = Config::default();
    let mut files = Vec::new();
    if config.merge_file(&paths.user_config())? {
        files.push(paths.user_config());
    }
    let mut project = ProjectTrust::NothingToTrust;
    if let Some(table) = read_project_config(paths)? {
        files.push(paths.project_config());
        let ProjectLayer { open, restricted } = ProjectLayer::split(table, settings);
        merge_adding(&mut config, open, ADDITIVE_LISTS)?;
        if !restricted.is_empty() {
            let hash = crate::trust::settings_hash(&restricted);
            let saved = TrustStore::new(&paths.home).is_trusted(&paths.workspace, &hash);
            project = if saved || overrides.trust_project {
                merge_adding(&mut config, restricted.clone(), TRUSTED_ADDITIVE_LISTS)?;
                ProjectTrust::Trusted {
                    settings: restricted,
                    saved,
                }
            } else {
                ProjectTrust::Untrusted {
                    settings: restricted,
                    hash,
                }
            };
        }
    }
    apply_overrides(&mut config, overrides)?;
    Ok(LoadedConfig {
        config,
        files,
        project,
    })
}

/// Merges `table` into `config`, adding the lists at `lists` to the ones already there
/// instead of replacing them.
fn merge_adding(config: &mut Config, mut table: Table, lists: &[&str]) -> anyhow::Result<()> {
    let additions: Vec<(&str, Vec<Value>)> = lists
        .iter()
        .filter_map(|path| take_list(&mut table, path).map(|v| (*path, v)))
        .collect();
    config.merge(table);
    for (path, values) in additions {
        append_list(config, path, values)?;
    }
    Ok(())
}

/// Called after the user saves `rule` to the project's allow list from the REPL. If the
/// project's restricted settings are now exactly `trusted` plus that rule, records trust
/// for them so the next run doesn't ask about a change the user made, and makes them the
/// new `trusted` for the next rule. Returns whether it did.
pub fn trust_saved_rule(
    paths: &Paths,
    settings: &ProjectSettings,
    trusted: &mut Table,
    rule: &str,
) -> anyhow::Result<bool> {
    let Some(table) = read_project_config(paths)? else {
        return Ok(false);
    };
    let now = ProjectLayer::split(table, settings).restricted;
    let mut expected = Config::from_table(trusted.clone());
    append_list(
        &mut expected,
        "plugins.safety.allow",
        vec![Value::String(rule.to_string())],
    )?;
    if &now != expected.as_table() {
        return Ok(false);
    }
    TrustStore::new(&paths.home).trust(&paths.workspace, &crate::trust::settings_hash(&now))?;
    *trusted = now;
    Ok(true)
}

/// Parses the project's `.ch/config.toml`, if there is one. It is refused when it
/// resolves outside the workspace, and a parse error gives the line but not its text:
/// the error may end up in a CI log, and the file could be any file on the machine.
fn read_project_config(paths: &Paths) -> anyhow::Result<Option<Table>> {
    let path = paths.project_config();
    let real = match std::fs::canonicalize(&path) {
        Ok(p) => p,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading config {}", path.display())),
    };
    let root = std::fs::canonicalize(&paths.workspace).unwrap_or_else(|_| paths.workspace.clone());
    if !real.starts_with(&root) {
        bail!(
            "{} points outside the workspace, to {}; ch only reads a project config inside \
             the project. Put shared settings in {} instead.",
            path.display(),
            real.display(),
            paths.user_config().display()
        );
    }
    let text = std::fs::read_to_string(&real)
        .with_context(|| format!("reading config {}", path.display()))?;
    match text.parse::<Table>() {
        Ok(table) => Ok(Some(table)),
        Err(e) => {
            let line = e.span().map_or(0, |s| {
                text[..s.start.min(text.len())].matches('\n').count() + 1
            });
            bail!(
                "parsing config {} (line {line}): {}",
                path.display(),
                e.message()
            )
        }
    }
}

/// Applies the command-line layers to an already merged config.
pub fn apply_overrides(config: &mut Config, o: &Overrides) -> anyhow::Result<()> {
    if let Some(profile) = &o.profile {
        config.apply_profile(profile)?;
    }
    if let Some(model) = &o.model {
        config.set_value("model.id", Value::String(model.clone()))?;
    }
    for assignment in &o.set {
        config
            .set_override(assignment)
            .with_context(|| format!("--set {assignment}"))?;
    }
    if !o.allow.is_empty() {
        let mut allow = allow_list(config)?;
        for rule in &o.allow {
            let rule = rule.trim();
            if rule.is_empty() {
                bail!("--allow needs a rule such as `edit_file` or `bash:cargo *`");
            }
            if !allow.iter().any(|r| r == rule) {
                allow.push(rule.to_string());
            }
        }
        config.set_value(
            "plugins.safety.allow",
            Value::Array(allow.into_iter().map(Value::String).collect()),
        )?;
    }
    if let Some(mode) = &o.permission_mode {
        config.set_value("plugins.safety.mode", Value::String(mode.clone()))?;
    }
    Ok(())
}

fn allow_list(config: &Config) -> anyhow::Result<Vec<String>> {
    config
        .get::<Vec<String>>("plugins.safety.allow")
        .context("`plugins.safety.allow` must be a list of rule strings")
        .map(Option::unwrap_or_default)
}

/// The workspace directory: `--cwd` or the current directory, made absolute.
pub fn resolve_workspace(cwd: Option<&Path>) -> anyhow::Result<PathBuf> {
    let dir = match cwd {
        Some(d) => d.to_path_buf(),
        None => std::env::current_dir().context("cannot read the current directory")?,
    };
    if !dir.is_dir() {
        bail!("--cwd {}: not a directory", dir.display());
    }
    dir.canonicalize()
        .with_context(|| format!("resolving {}", dir.display()))
}

/// Text for `ch --config-path`.
pub fn describe_paths(paths: &Paths) -> String {
    let status = |p: &Path| if p.exists() { "" } else { "  (not found)" };
    let user = paths.user_config();
    let project = paths.project_config();
    format!(
        "user config:    {}{}\nproject config: {}{}\nsessions:       {}\nlogs:           {}\n",
        user.display(),
        status(&user),
        project.display(),
        status(&project),
        paths.sessions().display(),
        paths.logs().display(),
    )
}
