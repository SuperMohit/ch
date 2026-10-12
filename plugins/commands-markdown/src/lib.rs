//! Markdown slash commands.
//!
//! Every `*.md` file in `<workspace>/.ch/commands/` and `~/.ch/commands/` becomes a
//! `/name` command (the file name without `.md`). Project files override user files
//! with the same name. Running the command sends the file's text to the model as the
//! next prompt, with `$ARGUMENTS` replaced by whatever follows the command name.
//!
//! Optional front matter sets the description and usage hint shown by `/help`:
//!
//! ```markdown
//! ---
//! description: Review the staged changes
//! argument-hint: [focus]
//! ---
//! Review the staged changes with `git diff --cached`. Focus on: $ARGUMENTS
//! ```
//!
//! Config (`[plugins.commands-markdown]`):
//!
//! ```toml
//! user_dir = "~/.ch/commands"   # "" turns user commands off
//! ```

pub mod parse;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use ch_kernel::{CmdCx, Command, Outcome, Plugin, PluginManifest, RegisterCx};
use serde::Deserialize;

/// Plugin id, also the config section name.
pub const ID: &str = "commands-markdown";

/// Names owned by built-in commands (`commands-core`). Files with these names are
/// skipped, because two plugins can't register the same command.
pub const RESERVED: &[&str] = &[
    "help", "clear", "compact", "model", "cost", "tools", "plugins", "config", "resume", "exit",
    "quit",
];

/// `[plugins.commands-markdown]`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct MarkdownConfig {
    /// Directory of user-level commands. `~/` is the home directory; an empty string
    /// turns user commands off. Defaults to `~/.ch/commands`.
    pub user_dir: Option<String>,
}

impl MarkdownConfig {
    fn user_dir(&self) -> Option<PathBuf> {
        match self.user_dir.as_deref() {
            Some("") => None,
            Some(p) => match p.strip_prefix("~/") {
                Some(rest) => dirs::home_dir().map(|h| h.join(rest)),
                None => Some(PathBuf::from(p)),
            },
            None => dirs::home_dir().map(|h| h.join(".ch").join("commands")),
        }
    }
}

/// Registers one command per markdown file.
pub struct CommandsMarkdownPlugin;

/// Returns the plugin as a trait object.
pub fn plugin() -> Arc<dyn Plugin> {
    Arc::new(CommandsMarkdownPlugin)
}

impl Plugin for CommandsMarkdownPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest::new(
            ID,
            "Prompt-template slash commands from .ch/commands/*.md and ~/.ch/commands/*.md",
        )
        .provides(&["commands"])
    }

    fn register(&self, cx: &mut RegisterCx<'_>) -> anyhow::Result<()> {
        let config: MarkdownConfig = cx.config()?;
        let project_dir = cx.workspace().join(".ch").join("commands");
        for cmd in discover(config.user_dir().as_deref(), &project_dir) {
            cx.add_command(cmd);
        }
        Ok(())
    }
}

/// Where a command came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    User,
    Project,
}

/// A command backed by a markdown file.
#[derive(Debug, Clone)]
pub struct MarkdownCommand {
    pub name: String,
    pub path: PathBuf,
    pub source: Source,
    pub file: parse::CommandFile,
}

/// Finds the commands in `user_dir` and `project_dir`, project files overriding user
/// files with the same name, sorted by name. Reserved and invalid names are skipped
/// with a warning.
pub fn discover(user_dir: Option<&Path>, project_dir: &Path) -> Vec<MarkdownCommand> {
    let mut by_name: BTreeMap<String, MarkdownCommand> = BTreeMap::new();
    let dirs = user_dir
        .map(|d| (d, Source::User))
        .into_iter()
        .chain([(project_dir, Source::Project)]);
    for (dir, source) in dirs {
        for cmd in scan(dir, source) {
            by_name.insert(cmd.name.clone(), cmd);
        }
    }
    by_name.into_values().collect()
}

/// The command files directly inside `dir` (no recursion).
fn scan(dir: &Path, source: Source) -> Vec<MarkdownCommand> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("md") && p.is_file())
        .collect();
    paths.sort();

    let mut out = Vec::new();
    for path in paths {
        let Some(name) = path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(str::to_string)
        else {
            continue;
        };
        if RESERVED.contains(&name.as_str()) {
            tracing::warn!(
                path = %path.display(),
                "skipping markdown command `/{name}`: the name is used by a built-in command; \
                 rename the file"
            );
            continue;
        }
        if !parse::valid_name(&name) {
            tracing::warn!(
                path = %path.display(),
                "skipping markdown command `{name}`: use only letters, digits, `-`, `_`, `.` \
                 and `:` in the file name"
            );
            continue;
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(path = %path.display(), "skipping markdown command: {e}");
                continue;
            }
        };
        let mut file = parse::parse(&text);
        if file.description.is_empty() {
            file.description = format!("Prompt from {}", path.display());
        }
        out.push(MarkdownCommand {
            name,
            path,
            source,
            file,
        });
    }
    out
}

#[async_trait]
impl Command for MarkdownCommand {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.file.description
    }

    fn yields_on_conflict(&self) -> bool {
        true
    }

    fn usage(&self) -> &str {
        &self.file.usage
    }

    async fn run(&self, args: &str, _cx: &mut CmdCx<'_>) -> anyhow::Result<Outcome> {
        // Re-read so edits apply without a restart; fall back to the loaded copy.
        let template = match std::fs::read_to_string(&self.path) {
            Ok(text) => parse::parse(&text).template,
            Err(e) => {
                tracing::warn!(path = %self.path.display(), "re-reading command failed: {e}");
                self.file.template.clone()
            }
        };
        Ok(Outcome::Prompt(parse::render(&template, args)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(dir: &Path, name: &str, text: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join(name), text).unwrap();
    }

    #[test]
    fn project_overrides_user_and_skips_bad_files() {
        let tmp = tempfile::tempdir().unwrap();
        let user = tmp.path().join("user");
        let project = tmp.path().join("ws/.ch/commands");
        write(&user, "review.md", "User review");
        write(&user, "only-user.md", "From the user");
        write(
            &project,
            "review.md",
            "---\ndescription: Project review\n---\nReview $ARGUMENTS",
        );
        write(&project, "help.md", "Shadowing /help");
        write(&project, "quit.md", "Shadowing /quit");
        write(&project, "two words.md", "bad name");
        write(&project, "notes.txt", "not markdown");
        fs::create_dir_all(project.join("sub.md")).unwrap();

        let cmds = discover(Some(&user), &project);
        let names: Vec<&str> = cmds.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["only-user", "review"]);
        let review = &cmds[1];
        assert_eq!(review.source, Source::Project);
        assert_eq!(review.file.description, "Project review");
        assert_eq!(cmds[0].source, Source::User);
        assert_eq!(cmds[0].file.description, "From the user");
    }

    #[test]
    fn missing_dirs_are_fine() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(discover(Some(&tmp.path().join("nope")), &tmp.path().join("nada")).is_empty());
        assert!(discover(None, &tmp.path().join("nada")).is_empty());
    }

    #[test]
    fn user_dir_config() {
        let off = MarkdownConfig {
            user_dir: Some(String::new()),
        };
        assert_eq!(off.user_dir(), None);
        let custom = MarkdownConfig {
            user_dir: Some("/x/y".into()),
        };
        assert_eq!(custom.user_dir(), Some(PathBuf::from("/x/y")));
    }
}
