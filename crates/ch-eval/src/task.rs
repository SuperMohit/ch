//! Tasks: one directory per task, holding `task.toml`, the starting workspace `repo/`
//! and the hidden check files `verify/`.
//!
//! ```toml
//! instruction = "The total in report.py is off by one for empty input. Fix it."
//! verify = "python3 -m unittest -q test_hidden"   # passes when it exits 0
//! image = "ch-eval-base"      # optional; the default image
//! timeout_secs = 900          # optional; wall time for the agent run
//! verify_timeout_secs = 600   # optional; wall time for the check
//! max_steps = 80              # optional; sets agent.max_steps
//! tags = ["bugfix", "python"] # optional; for filtering and the report
//! ```
//!
//! The check is a shell command: `ch-eval` knows nothing about languages, and the
//! task's image must have the tools the check uses.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, bail};
use serde::Deserialize;

/// The image a task runs in when it doesn't name one.
pub const DEFAULT_IMAGE: &str = "ch-eval-base";
/// Wall time for the agent run when the task doesn't set `timeout_secs`.
pub const DEFAULT_TIMEOUT_SECS: u64 = 900;
/// Wall time for the check when the task doesn't set `verify_timeout_secs`.
pub const DEFAULT_VERIFY_TIMEOUT_SECS: u64 = 600;

/// `task.toml` as written.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskFile {
    instruction: String,
    verify: String,
    image: Option<String>,
    timeout_secs: Option<u64>,
    verify_timeout_secs: Option<u64>,
    max_steps: Option<u32>,
    #[serde(default)]
    tags: Vec<String>,
}

/// A task, read and checked.
#[derive(Debug, Clone, PartialEq)]
pub struct Task {
    /// The directory name.
    pub id: String,
    /// The task's directory.
    pub dir: PathBuf,
    /// Sent to the agent on stdin.
    pub instruction: String,
    /// The check: a shell command run in the workspace; exit 0 is a pass.
    pub verify: String,
    pub image: String,
    pub timeout: Duration,
    pub verify_timeout: Duration,
    /// Sets `agent.max_steps` for the run.
    pub max_steps: Option<u32>,
    pub tags: Vec<String>,
}

impl Task {
    /// The starting workspace.
    pub fn repo(&self) -> PathBuf {
        self.dir.join("repo")
    }

    /// The hidden check files, copied over the workspace after the run. Optional.
    pub fn verify_dir(&self) -> PathBuf {
        self.dir.join("verify")
    }
}

/// Reads the task in `dir`.
pub fn load_task(dir: &Path) -> anyhow::Result<Task> {
    let id = dir
        .file_name()
        .and_then(|n| n.to_str())
        .with_context(|| format!("{}: not a task directory", dir.display()))?
        .to_string();
    check_id(&id).with_context(|| format!("task {}", dir.display()))?;
    let path = dir.join("task.toml");
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let file: TaskFile =
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    let task = Task {
        id,
        dir: dir.to_path_buf(),
        instruction: file.instruction,
        verify: file.verify,
        image: file.image.unwrap_or_else(|| DEFAULT_IMAGE.to_string()),
        timeout: Duration::from_secs(file.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS)),
        verify_timeout: Duration::from_secs(
            file.verify_timeout_secs
                .unwrap_or(DEFAULT_VERIFY_TIMEOUT_SECS),
        ),
        max_steps: file.max_steps,
        tags: file.tags,
    };
    check_task(&task).with_context(|| format!("in {}", path.display()))?;
    Ok(task)
}

fn check_task(task: &Task) -> anyhow::Result<()> {
    if task.instruction.trim().is_empty() {
        bail!("`instruction` is empty");
    }
    if task.verify.trim().is_empty() {
        bail!("`verify` is empty");
    }
    if task.image.trim().is_empty() || task.image.starts_with('-') {
        bail!("`image` must name an image");
    }
    if task.timeout.is_zero() || task.verify_timeout.is_zero() {
        bail!("timeouts must be at least one second");
    }
    if task.max_steps == Some(0) {
        bail!("`max_steps` must be at least 1");
    }
    if let Some(tag) = task.tags.iter().find(|t| t.trim().is_empty()) {
        bail!("tag `{tag}` is empty");
    }
    if !task.repo().is_dir() {
        bail!("no `repo/` directory next to it");
    }
    let verify = task.verify_dir();
    if verify.exists() && !verify.is_dir() {
        bail!("`verify` next to it is not a directory");
    }
    Ok(())
}

/// Task ids name directories in the output, so they are kept to a safe set.
fn check_id(id: &str) -> anyhow::Result<()> {
    let ok = !id.is_empty()
        && !id.starts_with('.')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if !ok {
        bail!("task id `{id}` must be letters, digits, `-`, `_` and `.`, not starting with `.`");
    }
    Ok(())
}

/// Reads every task in the suite directory `dir`, sorted by id. Each subdirectory is a
/// task; hidden ones (starting with `.`) are skipped.
pub fn load_suite(dir: &Path) -> anyhow::Result<Vec<Task>> {
    let entries =
        std::fs::read_dir(dir).with_context(|| format!("reading suite {}", dir.display()))?;
    let mut tasks = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("reading suite {}", dir.display()))?;
        let path = entry.path();
        let hidden = entry.file_name().to_string_lossy().starts_with('.');
        if hidden || !path.is_dir() {
            continue;
        }
        tasks.push(load_task(&path)?);
    }
    if tasks.is_empty() {
        bail!("suite {} has no tasks", dir.display());
    }
    tasks.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(tasks)
}

/// Which tasks to run: with `ids`, only those; with `tags`, only tasks that have at
/// least one of them. Both narrow the selection.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Filter {
    pub ids: Vec<String>,
    pub tags: Vec<String>,
}

/// The tasks `filter` selects, in suite order. An unknown id, or a selection that
/// leaves nothing, is an error.
pub fn select(tasks: Vec<Task>, filter: &Filter) -> anyhow::Result<Vec<Task>> {
    if let Some(id) = filter
        .ids
        .iter()
        .find(|id| !tasks.iter().any(|t| &t.id == *id))
    {
        let known: Vec<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        bail!("no task `{id}` in the suite (tasks: {})", known.join(", "));
    }
    let selected: Vec<Task> = tasks
        .into_iter()
        .filter(|t| filter.ids.is_empty() || filter.ids.contains(&t.id))
        .filter(|t| filter.tags.is_empty() || t.tags.iter().any(|g| filter.tags.contains(g)))
        .collect();
    if selected.is_empty() {
        bail!("no task matches the given --task and --tag filters");
    }
    Ok(selected)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_task(suite: &Path, id: &str, toml: &str) -> PathBuf {
        let dir = suite.join(id);
        std::fs::create_dir_all(dir.join("repo")).unwrap();
        std::fs::write(dir.join("task.toml"), toml).unwrap();
        dir
    }

    const MINIMAL: &str = "instruction = \"Fix it.\"\nverify = \"true\"\n";

    #[test]
    fn reads_a_task_with_defaults() {
        let suite = tempfile::tempdir().unwrap();
        let dir = write_task(suite.path(), "fix-1", MINIMAL);
        let task = load_task(&dir).unwrap();
        assert_eq!(task.id, "fix-1");
        assert_eq!(task.instruction, "Fix it.");
        assert_eq!(task.verify, "true");
        assert_eq!(task.image, DEFAULT_IMAGE);
        assert_eq!(task.timeout, Duration::from_secs(DEFAULT_TIMEOUT_SECS));
        assert_eq!(
            task.verify_timeout,
            Duration::from_secs(DEFAULT_VERIFY_TIMEOUT_SECS)
        );
        assert_eq!(task.max_steps, None);
        assert!(task.tags.is_empty());
    }

    #[test]
    fn reads_every_field() {
        let suite = tempfile::tempdir().unwrap();
        let dir = write_task(
            suite.path(),
            "t",
            r#"
            instruction = "Do it."
            verify = "make check"
            image = "my-image"
            timeout_secs = 60
            verify_timeout_secs = 30
            max_steps = 40
            tags = ["feature", "node"]
            "#,
        );
        let task = load_task(&dir).unwrap();
        assert_eq!(task.image, "my-image");
        assert_eq!(task.timeout, Duration::from_secs(60));
        assert_eq!(task.verify_timeout, Duration::from_secs(30));
        assert_eq!(task.max_steps, Some(40));
        assert_eq!(task.tags, ["feature", "node"]);
    }

    #[test]
    fn rejects_bad_tasks() {
        let suite = tempfile::tempdir().unwrap();
        let cases = [
            (
                "typo",
                "instruction = \"x\"\nverify = \"true\"\ntimeout = 5\n",
            ),
            ("no-verify", "instruction = \"x\"\n"),
            ("empty", "instruction = \" \"\nverify = \"true\"\n"),
            (
                "zero-steps",
                "instruction = \"x\"\nverify = \"true\"\nmax_steps = 0\n",
            ),
            (
                "zero-time",
                "instruction = \"x\"\nverify = \"true\"\ntimeout_secs = 0\n",
            ),
            (
                "flag-image",
                "instruction = \"x\"\nverify = \"true\"\nimage = \"--privileged\"\n",
            ),
        ];
        for (id, toml) in cases {
            let dir = write_task(suite.path(), id, toml);
            assert!(load_task(&dir).is_err(), "{id}");
        }
        let dir = suite.path().join("no-repo");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("task.toml"), MINIMAL).unwrap();
        let err = load_task(&dir).unwrap_err();
        assert!(format!("{err:#}").contains("repo/"), "{err:#}");
        let dir = write_task(suite.path(), "bad id", MINIMAL);
        assert!(load_task(&dir).is_err());
    }

    #[test]
    fn loads_a_suite_sorted_and_filters_it() {
        let suite = tempfile::tempdir().unwrap();
        write_task(
            suite.path(),
            "b",
            "instruction = \"x\"\nverify = \"true\"\ntags = [\"python\"]\n",
        );
        write_task(
            suite.path(),
            "a",
            "instruction = \"x\"\nverify = \"true\"\ntags = [\"node\", \"feature\"]\n",
        );
        write_task(suite.path(), "c", MINIMAL);
        std::fs::create_dir_all(suite.path().join(".hidden")).unwrap();
        std::fs::write(suite.path().join("README.md"), "notes").unwrap();
        let tasks = load_suite(suite.path()).unwrap();
        let ids = |ts: &[Task]| ts.iter().map(|t| t.id.clone()).collect::<Vec<_>>();
        assert_eq!(ids(&tasks), ["a", "b", "c"]);

        let by_tag = Filter {
            tags: vec!["python".into(), "feature".into()],
            ..Default::default()
        };
        assert_eq!(ids(&select(tasks.clone(), &by_tag).unwrap()), ["a", "b"]);
        let both = Filter {
            ids: vec!["a".into(), "c".into()],
            tags: vec!["node".into()],
        };
        assert_eq!(ids(&select(tasks.clone(), &both).unwrap()), ["a"]);
        let unknown = Filter {
            ids: vec!["z".into()],
            ..Default::default()
        };
        assert!(select(tasks.clone(), &unknown).is_err());
        let nothing = Filter {
            tags: vec!["rust".into()],
            ..Default::default()
        };
        assert!(select(tasks, &nothing).is_err());
    }
}
