//! `plugin.toml`: what an external plugin is and how to start it.
//!
//! ```toml
//! name = "todo-scanner"          # [a-z0-9-]{1,24}; tools are ext__<name>__<tool>
//! description = "Finds TODO comments"
//! command = "./todo-scanner"     # a path (relative to this directory) or a name on PATH
//! args = []
//! env = { TOKEN = "${MY_TOKEN}" } # ${VAR} from ch's environment
//! startup_timeout_secs = 10
//! call_timeout_secs = 120        # tool calls and commands
//! hook_timeout_secs = 10         # hooks and prompt contributions
//! on_error = "deny"              # what a failing pre_tool hook means: "deny" or "allow"
//! guards = ["*"]                 # tools whose calls are refused while the plugin isn't
//!                                # running (also before it first starts), with on_error = "deny"
//! hook_priority = 0              # where its hooks run; never before the permission check
//!
//! [[commands]]                   # slash commands, named here so ch knows them up front
//! name = "todos"
//! description = "List the TODOs in the workspace"
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use ch_kernel::util::printable;
use serde::Deserialize;

/// Lowest hook priority a plugin may ask for: the permission check runs at -100 and
/// must stay first.
pub const MIN_HOOK_PRIORITY: i32 = -99;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnError {
    /// A pre_tool hook that fails, times out or isn't running denies the call.
    #[default]
    Deny,
    Allow,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandDecl {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub usage: String,
}

/// A parsed `plugin.toml`.
#[derive(Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub version: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub startup_timeout_secs: Option<u64>,
    #[serde(default)]
    pub call_timeout_secs: Option<u64>,
    #[serde(default)]
    pub hook_timeout_secs: Option<u64>,
    #[serde(default)]
    pub on_error: OnError,
    #[serde(default)]
    pub hook_priority: i32,
    /// Tool name globs this plugin checks with `pre_tool` hooks. With `on_error =
    /// "deny"`, calls to them are refused whenever the plugin isn't running, including
    /// before it ever finished starting, so a guard that fails to start doesn't let
    /// everything through.
    #[serde(default)]
    pub guards: Vec<String>,
    #[serde(default)]
    pub commands: Vec<CommandDecl>,
    /// The directory holding `plugin.toml`; the plugin runs there.
    #[serde(skip)]
    pub dir: PathBuf,
}

/// Env values may be credentials: only their names are shown.
impl std::fmt::Debug for Manifest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Manifest")
            .field("name", &self.name)
            .field("command", &self.command)
            .field("args", &self.args)
            .field("env", &self.env.keys().collect::<Vec<_>>())
            .field("dir", &self.dir)
            .finish_non_exhaustive()
    }
}

impl Manifest {
    pub fn parse(text: &str, dir: &Path) -> anyhow::Result<Self> {
        let mut m: Manifest = toml::from_str(text)?;
        m.dir = dir.to_path_buf();
        m.validate()?;
        Ok(m)
    }

    fn validate(&self) -> anyhow::Result<()> {
        let valid = |n: &str| {
            (1..=24).contains(&n.len())
                && n.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        };
        if !valid(&self.name) {
            bail!(
                "the plugin name `{}` must be 1 to 24 lowercase letters, digits or `-`",
                printable(&self.name)
            );
        }
        if self.command.trim().is_empty() {
            bail!("`command` is empty");
        }
        for (key, value) in [
            ("startup_timeout_secs", self.startup_timeout_secs),
            ("call_timeout_secs", self.call_timeout_secs),
            ("hook_timeout_secs", self.hook_timeout_secs),
        ] {
            if value == Some(0) {
                bail!("`{key}` must be at least 1");
            }
        }
        for c in &self.commands {
            let ok = !c.name.is_empty()
                && c.name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
            if !ok {
                bail!(
                    "the command name `{}` must be letters, digits, `-` or `_`",
                    printable(&c.name)
                );
            }
        }
        Ok(())
    }

    /// `ext:<name>`.
    pub fn source_id(&self) -> String {
        format!("ext:{}", self.name)
    }

    /// `ext__<name>__`.
    pub fn prefix(&self) -> String {
        format!("ext__{}__", self.name)
    }

    pub fn hook_priority(&self) -> i32 {
        self.hook_priority.max(MIN_HOOK_PRIORITY)
    }
}

/// The plugins under `dir` (`<dir>/<plugin>/plugin.toml`), sorted by directory name,
/// and a message for each that couldn't be read.
pub fn discover(dir: &Path) -> (Vec<Manifest>, Vec<String>) {
    let mut found = Vec::new();
    let mut problems = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (found, problems);
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.join("plugin.toml").is_file())
        .collect();
    dirs.sort();
    for plugin_dir in dirs {
        let file = plugin_dir.join("plugin.toml");
        let parsed = std::fs::read_to_string(&file)
            .with_context(|| format!("reading {}", file.display()))
            .and_then(|text| Manifest::parse(&text, &plugin_dir));
        match parsed {
            Ok(m) if found.iter().any(|f: &Manifest| f.name == m.name) => problems.push(format!(
                "ignoring {}: another plugin is named `{}`",
                printable(&file.display().to_string()),
                m.name
            )),
            Ok(m) => found.push(m),
            Err(e) => problems.push(format!(
                "ignoring {}: {}",
                printable(&file.display().to_string()),
                printable(&format!("{e:#}"))
            )),
        }
    }
    (found, problems)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifests() {
        let m = Manifest::parse(
            "name = \"todo\"\ncommand = \"./run\"\nenv = { T = \"${SECRET}\" }\nhook_priority = -500\n\
             [[commands]]\nname = \"todos\"\n",
            Path::new("/p"),
        )
        .unwrap();
        assert_eq!(m.prefix(), "ext__todo__");
        assert_eq!(m.hook_priority(), MIN_HOOK_PRIORITY);
        assert_eq!(m.on_error, OnError::Deny);
        assert!(!format!("{m:?}").contains("SECRET"), "{m:?}");
        for bad in [
            "name = \"Todo\"\ncommand = \"x\"",
            "name = \"t\"\ncommand = \" \"",
            "name = \"t\"\ncommand = \"x\"\nunknown = 1",
            "name = \"t\"\ncommand = \"x\"\n[[commands]]\nname = \"a b\"",
            "name = \"t\"\ncommand = \"x\"\nhook_timeout_secs = 0",
        ] {
            assert!(Manifest::parse(bad, Path::new("/p")).is_err(), "{bad}");
        }
    }

    #[test]
    fn discovery_skips_bad_and_repeated_plugins() {
        let dir = tempfile::tempdir().unwrap();
        for (sub, text) in [
            ("a", "name = \"one\"\ncommand = \"x\""),
            ("b", "name = \"one\"\ncommand = \"y\""),
            ("c", "name = \"BAD\"\ncommand = \"x\""),
        ] {
            std::fs::create_dir(dir.path().join(sub)).unwrap();
            std::fs::write(dir.path().join(sub).join("plugin.toml"), text).unwrap();
        }
        std::fs::create_dir(dir.path().join("empty")).unwrap();
        let (found, problems) = discover(dir.path());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].command, "x");
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(discover(&dir.path().join("missing")).0.is_empty());
    }
}
