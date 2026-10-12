//! Helpers shared by the file tools.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ch_kernel::ToolOutput;
use ch_safety::{FileState, PathGuard};
use globset::{GlobBuilder, GlobMatcher};
use serde::de::DeserializeOwned;
use serde_json::Value;

/// Directories never listed or searched.
pub const SKIP_DIRS: &[&str] = &[".git", "node_modules", ".venv", "__pycache__", "target"];

/// What every file tool needs: the safety services and the output budget.
#[derive(Clone)]
pub struct FsContext {
    pub guard: Arc<PathGuard>,
    pub state: Arc<FileState>,
    /// The kernel truncates tool results above this many chars; tools that page
    /// (read_file) stay under it so nothing is cut silently.
    pub max_output_chars: usize,
}

impl FsContext {
    pub fn new(guard: Arc<PathGuard>, state: Arc<FileState>) -> Self {
        Self {
            guard,
            state,
            max_output_chars: 24_000,
        }
    }

    /// Resolves an optional directory argument (missing or empty means the root).
    pub fn resolve_or_root(&self, path: Option<&str>) -> Result<PathBuf, ToolOutput> {
        match path.map(str::trim) {
            None | Some("") => Ok(self.guard.root().to_path_buf()),
            Some(p) => self.guard.resolve(p).map_err(ToolOutput::error),
        }
    }

    pub fn rel(&self, path: &Path) -> String {
        self.guard.relative(path)
    }
}

/// Deserializes a tool's input, or returns an error observation that shows the
/// expected shape.
pub fn parse_input<T: DeserializeOwned>(
    tool: &str,
    input: Value,
    usage: &str,
) -> Result<T, ToolOutput> {
    serde_json::from_value(input)
        .map_err(|e| ToolOutput::error(format!("Invalid input for {tool}: {e}. Expected {usage}.")))
}

/// A unified diff of `old` → `new` with 3 lines of context, plus added/removed counts.
pub fn unified_diff(rel: &str, old: &str, new: &str) -> (String, usize, usize) {
    let diff = similar::TextDiff::from_lines(old, new);
    let mut added = 0;
    let mut removed = 0;
    for change in diff.iter_all_changes() {
        match change.tag() {
            similar::ChangeTag::Insert => added += 1,
            similar::ChangeTag::Delete => removed += 1,
            similar::ChangeTag::Equal => {}
        }
    }
    let text = diff
        .unified_diff()
        .context_radius(3)
        .header(&format!("a/{rel}"), &format!("b/{rel}"))
        .to_string();
    (text, added, removed)
}

/// Diff text shown to the model: long diffs are shortened (the full one goes in
/// metadata).
pub fn diff_for_model(diff: &str) -> String {
    ch_kernel::util::truncate_middle(diff, 6_000)
}

/// Compiles a file glob. A pattern without `/` matches file names at any depth;
/// otherwise it matches the path relative to the search root (`*` stops at `/`).
pub struct FileGlob {
    matcher: GlobMatcher,
    name_only: bool,
}

impl FileGlob {
    pub fn new(pattern: &str) -> Result<Self, String> {
        let pattern = pattern.trim();
        let pattern = pattern.strip_prefix("./").unwrap_or(pattern);
        let matcher = GlobBuilder::new(pattern)
            .literal_separator(true)
            .build()
            .map_err(|e| {
                format!(
                    "Invalid glob `{pattern}`: {e}. Use `*` within a name, `**` across \
                     directories and `{{a,b}}` for alternatives, e.g. `src/**/*.rs`."
                )
            })?
            .compile_matcher();
        Ok(Self {
            matcher,
            name_only: !pattern.contains('/'),
        })
    }

    /// Whether `rel` (relative to the search root, `/`-separated) matches.
    pub fn is_match(&self, rel: &Path) -> bool {
        if self.name_only {
            rel.file_name().is_some_and(|n| self.matcher.is_match(n))
        } else {
            self.matcher.is_match(rel)
        }
    }
}

/// Directories skipped during a walk, for a note in the output.
pub type Skipped = Arc<Mutex<BTreeSet<String>>>;

/// A gitignore-aware walker over `base` that never enters [`SKIP_DIRS`] and doesn't
/// follow symlinks. Hidden files are included.
pub fn walker(base: &Path, max_depth: Option<usize>, skipped: Option<Skipped>) -> ignore::Walk {
    let mut b = ignore::WalkBuilder::new(base);
    b.hidden(false)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(false)
        .ignore(true)
        .parents(true)
        .require_git(false)
        .follow_links(false)
        .max_depth(max_depth)
        .sort_by_file_name(|a, b| a.cmp(b))
        .filter_entry(move |entry| {
            let skip = entry.depth() > 0
                && entry.file_type().is_some_and(|t| t.is_dir())
                && entry
                    .file_name()
                    .to_str()
                    .is_some_and(|n| SKIP_DIRS.contains(&n));
            if skip && let Some(s) = &skipped {
                let mut set = s.lock().unwrap_or_else(|p| p.into_inner());
                set.insert(entry.file_name().to_string_lossy().into_owned());
            }
            !skip
        });
    b.build()
}

/// `n` bytes in a short human form (`512 B`, `1.2 KB`, `3.4 MB`).
pub fn human_size(n: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut v = n as f64 / 1024.0;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    format!("{v:.1} {}", UNITS[unit])
}

/// Runs blocking filesystem work off the async runtime.
pub async fn blocking<F>(f: F) -> ToolOutput
where
    F: FnOnce() -> ToolOutput + Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(out) => out,
        Err(e) => ToolOutput::error(format!("The tool failed internally: {e}. Try again.")),
    }
}

/// Up to three names in `path`'s directory that look like what was meant, for "not
/// found" errors.
pub fn similar_names(ctx: &FsContext, path: &Path) -> Vec<String> {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Vec::new();
    };
    let wanted = name.to_string_lossy().to_lowercase();
    let wanted_stem = Path::new(&wanted)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let Ok(entries) = std::fs::read_dir(parent) else {
        return Vec::new();
    };
    let mut out: Vec<String> = entries
        .filter_map(Result::ok)
        .filter(|e| {
            let n = e.file_name().to_string_lossy().to_lowercase();
            let stem = Path::new(&n)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            n == wanted || (!wanted_stem.is_empty() && stem == wanted_stem)
        })
        .map(|e| ctx.rel(&e.path()))
        .collect();
    out.sort();
    out.truncate(3);
    out
}

/// The standard "does not exist" error, with suggestions.
pub fn not_found(ctx: &FsContext, path: &Path, hint: &str) -> ToolOutput {
    let rel = ctx.rel(path);
    let similar = similar_names(ctx, path);
    let mut msg = format!("`{rel}` does not exist.");
    if !similar.is_empty() {
        msg.push_str(&format!(
            " Did you mean {}?",
            similar
                .iter()
                .map(|s| format!("`{s}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    msg.push(' ');
    msg.push_str(hint);
    ToolOutput::error(msg)
}

/// The `path` field of an input, for `summarize`.
pub fn input_path(input: &Value) -> &str {
    input.get("path").and_then(Value::as_str).unwrap_or(".")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(human_size(10), "10 B");
        assert_eq!(human_size(2048), "2.0 KB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MB");
    }

    #[test]
    fn globs() {
        let g = FileGlob::new("*.rs").unwrap();
        assert!(g.is_match(Path::new("a/b/c.rs")));
        let g = FileGlob::new("src/*.rs").unwrap();
        assert!(g.is_match(Path::new("src/c.rs")));
        assert!(!g.is_match(Path::new("src/a/c.rs")));
        let g = FileGlob::new("src/**/*.rs").unwrap();
        assert!(g.is_match(Path::new("src/a/c.rs")));
        assert!(FileGlob::new("a[").is_err());
    }

    #[test]
    fn diffs() {
        let (d, a, r) = unified_diff("x.txt", "a\nb\nc\n", "a\nB\nc\n");
        assert_eq!((a, r), (1, 1));
        assert!(d.contains("--- a/x.txt"));
        assert!(d.contains("-b\n+B"));
    }
}
