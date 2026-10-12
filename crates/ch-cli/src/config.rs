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
use crate::trust::{ADDITIVE_LISTS, ProjectLayer, TrustStore, append_list, take_list};

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

/// Builds the merged config for `paths` with the command-line `overrides`.
pub fn build_config(paths: &Paths, overrides: &Overrides) -> anyhow::Result<LoadedConfig> {
    let mut config = Config::default();
    let mut files = Vec::new();
    if config.merge_file(&paths.user_config())? {
        files.push(paths.user_config());
    }
    let mut project = ProjectTrust::NothingToTrust;
    if let Some(table) = read_table(&paths.project_config())? {
        files.push(paths.project_config());
        let ProjectLayer {
            mut open,
            restricted,
        } = ProjectLayer::split(table);
        let additions: Vec<(&str, Vec<Value>)> = ADDITIVE_LISTS
            .iter()
            .filter_map(|path| take_list(&mut open, path).map(|v| (*path, v)))
            .collect();
        config.merge(open);
        for (path, values) in additions {
            append_list(&mut config, path, values);
        }
        if !restricted.is_empty() {
            let hash = crate::trust::settings_hash(&restricted);
            let saved = TrustStore::new(&paths.home).is_trusted(&paths.workspace, &hash);
            project = if saved || overrides.trust_project {
                config.merge(restricted.clone());
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

/// Called after the user saves `rule` to the project's allow list from the REPL. If the
/// project's restricted settings are now exactly `trusted` plus that rule, records trust
/// for them so the next run doesn't ask about a change the user made. Returns whether it
/// did.
pub fn trust_saved_rule(paths: &Paths, trusted: &Table, rule: &str) -> anyhow::Result<bool> {
    let Some(table) = read_table(&paths.project_config())? else {
        return Ok(false);
    };
    let now = ProjectLayer::split(table).restricted;
    let mut expected = Config::from_table(trusted.clone());
    append_list(
        &mut expected,
        "plugins.safety.allow",
        vec![Value::String(rule.to_string())],
    );
    if &now != expected.as_table() {
        return Ok(false);
    }
    TrustStore::new(&paths.home).trust(&paths.workspace, &crate::trust::settings_hash(&now))?;
    Ok(true)
}

/// Parses a TOML file if it exists.
fn read_table(path: &Path) -> anyhow::Result<Option<Table>> {
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading config {}", path.display()))?;
    let table = text
        .parse()
        .with_context(|| format!("parsing config {}", path.display()))?;
    Ok(Some(table))
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
