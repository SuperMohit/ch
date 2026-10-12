//! Keeps every file path inside the workspace root.

use std::ffi::OsString;
use std::io;
use std::path::{Component, Path, PathBuf};

/// Symlinks followed while resolving one path before giving up (loops, deep chains).
const MAX_SYMLINKS: usize = 40;

/// Resolves model-supplied paths against the workspace root and rejects anything that
/// ends up outside it, whether through `..`, an absolute path or a symlink.
///
/// Paths that don't exist yet (a file about to be written) are resolved by following
/// the existing part physically and appending the rest.
#[derive(Debug, Clone)]
pub struct PathGuard {
    root: PathBuf,
}

impl PathGuard {
    /// Canonicalizes `root`; fails if it does not exist.
    pub fn new(root: impl AsRef<Path>) -> io::Result<Self> {
        Ok(Self {
            root: std::fs::canonicalize(root.as_ref())?,
        })
    }

    /// The canonical workspace root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolves `input` (relative to the workspace root, or absolute) to a canonical
    /// path inside the workspace. The error is written for the model.
    pub fn resolve(&self, input: &str) -> Result<PathBuf, String> {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Err(format!(
                "The path is empty. Give a path relative to the workspace root `{}` \
                 (for example `src/main.rs`), or `.` for the root itself.",
                self.root.display()
            ));
        }
        if trimmed.contains('\0') {
            return Err("The path contains a NUL byte. Remove it and try again.".into());
        }
        let expanded = expand_home(trimmed);
        let joined = if expanded.is_absolute() {
            expanded
        } else {
            self.root.join(expanded)
        };
        let resolved = resolve_physical(&joined).map_err(|e| match e {
            ResolveError::Dangling(link) => format!(
                "`{trimmed}` goes through the broken symlink `{}`. Pick another path, or \
                 fix the link with bash if you really need it.",
                self.relative(&link)
            ),
            ResolveError::Loop => format!(
                "`{trimmed}` goes through too many symlinks (a loop?). Use a path without \
                 symlinks."
            ),
            ResolveError::Io(err) => format!("Could not resolve `{trimmed}`: {err}."),
        })?;
        if resolved.starts_with(&self.root) {
            return Ok(resolved);
        }
        let lexical = normalize_lexically(&joined);
        let via_symlink = lexical.starts_with(&self.root);
        if via_symlink {
            Err(format!(
                "`{trimmed}` resolves through a symlink to `{}`, which is outside the \
                 workspace. Paths must be inside `{}`; work with files in the workspace \
                 instead.",
                resolved.display(),
                self.root.display()
            ))
        } else {
            Err(format!(
                "`{trimmed}` is outside the workspace. Paths must be inside `{}`; use a \
                 path relative to it, like `src/main.rs`.",
                self.root.display()
            ))
        }
    }

    /// `path` relative to the workspace root for display (`.` for the root itself);
    /// paths outside the root are shown in full.
    pub fn relative(&self, path: &Path) -> String {
        match path.strip_prefix(&self.root) {
            Ok(rel) if rel.as_os_str().is_empty() => ".".to_string(),
            Ok(rel) => rel.display().to_string(),
            Err(_) => path.display().to_string(),
        }
    }

    /// Whether `path` (already canonical) is inside the workspace.
    pub fn contains(&self, path: &Path) -> bool {
        path.starts_with(&self.root)
    }
}

/// `~` and `~/x` become paths under `$HOME`; everything else is unchanged.
fn expand_home(input: &str) -> PathBuf {
    let home = || std::env::var_os("HOME").map(PathBuf::from);
    if input == "~" {
        if let Some(h) = home() {
            return h;
        }
    } else if let Some(rest) = input.strip_prefix("~/")
        && let Some(h) = home()
    {
        return h.join(rest);
    }
    PathBuf::from(input)
}

enum ResolveError {
    Dangling(PathBuf),
    Loop,
    Io(io::Error),
}

/// One step of a path walk.
enum Step {
    Root(OsString),
    Parent,
    Name(OsString),
}

fn steps(path: &Path) -> impl DoubleEndedIterator<Item = Step> + '_ {
    path.components().filter_map(|c| match c {
        Component::Prefix(_) | Component::RootDir => Some(Step::Root(c.as_os_str().into())),
        Component::CurDir => None,
        Component::ParentDir => Some(Step::Parent),
        Component::Normal(n) => Some(Step::Name(n.into())),
    })
}

/// Walks `path` (absolute) one component at a time, following symlinks as the OS
/// would, so `..` after a symlink means the link target's parent. Components below
/// the first missing directory are appended as they are.
fn resolve_physical(path: &Path) -> Result<PathBuf, ResolveError> {
    // Reversed, so `pop` yields the next step and link targets can be pushed in front.
    let mut stack: Vec<Step> = steps(path).rev().collect();
    let mut out = PathBuf::new();
    let mut followed = 0usize;
    let mut missing = false;
    while let Some(step) = stack.pop() {
        match step {
            Step::Root(r) => {
                // Pushing a root (or prefix) replaces what was there, which is what an
                // absolute link target needs.
                out.push(&r);
                missing = false;
            }
            Step::Parent => {
                out.pop();
                // `out` has no symlinks, so popping is the physical parent.
                missing = std::fs::symlink_metadata(&out).is_err();
            }
            Step::Name(name) => {
                out.push(&name);
                if missing {
                    continue;
                }
                let meta = match std::fs::symlink_metadata(&out) {
                    Ok(m) => m,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {
                        missing = true;
                        continue;
                    }
                    Err(e) => return Err(ResolveError::Io(e)),
                };
                if !meta.file_type().is_symlink() {
                    continue;
                }
                followed += 1;
                if followed > MAX_SYMLINKS {
                    return Err(ResolveError::Loop);
                }
                // A dangling link is reported instead of being followed into nothing.
                if std::fs::metadata(&out).is_err() {
                    return Err(ResolveError::Dangling(out));
                }
                let target = std::fs::read_link(&out).map_err(ResolveError::Io)?;
                out.pop();
                stack.extend(steps(&target).rev().collect::<Vec<_>>());
            }
        }
    }
    Ok(canonicalize_existing_prefix(&out))
}

/// Canonicalizes the longest existing prefix of `path` and appends the rest, so the
/// result uses the names as stored on disk even when the last components don't exist
/// yet. On case-insensitive file systems `.Git/hooks/x` then becomes `.git/hooks/x`.
/// Symlinks were already followed, so this only changes spelling.
fn canonicalize_existing_prefix(path: &Path) -> PathBuf {
    let mut rest: Vec<&std::ffi::OsStr> = Vec::new();
    let mut current = path;
    loop {
        if let Ok(mut out) = std::fs::canonicalize(current) {
            for name in rest.iter().rev() {
                out.push(name);
            }
            return out;
        }
        match (current.parent(), current.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name);
                current = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// Resolves `.` and `..` without touching the filesystem.
fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guard() -> (tempfile::TempDir, PathGuard) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "fn main() {}").unwrap();
        let g = PathGuard::new(dir.path()).unwrap();
        (dir, g)
    }

    #[test]
    fn relative_and_absolute_inside() {
        let (_d, g) = guard();
        let p = g.resolve("src/main.rs").unwrap();
        assert_eq!(p, g.root().join("src/main.rs"));
        let abs = g.root().join("src/main.rs");
        assert_eq!(g.resolve(abs.to_str().unwrap()).unwrap(), p);
        assert_eq!(g.resolve("./src/../src/main.rs").unwrap(), p);
        assert_eq!(g.resolve(".").unwrap(), g.root());
        assert_eq!(g.relative(&p), "src/main.rs");
        assert_eq!(g.relative(g.root()), ".");
    }

    #[test]
    fn dotdot_escape_rejected() {
        let (_d, g) = guard();
        let err = g.resolve("../outside.txt").unwrap_err();
        assert!(err.contains("outside the workspace"), "{err}");
        assert!(err.contains(&g.root().display().to_string()), "{err}");
        assert!(g.resolve("src/../../x").is_err());
        assert!(g.resolve("new/dir/../../../x").is_err());
    }

    #[test]
    fn absolute_outside_rejected() {
        let (_d, g) = guard();
        let err = g.resolve("/etc/passwd").unwrap_err();
        assert!(err.contains("must be inside"), "{err}");
        assert!(g.resolve("~/.ssh/id_rsa").is_err());
    }

    #[test]
    fn empty_path_rejected() {
        let (_d, g) = guard();
        assert!(g.resolve("  ").unwrap_err().contains("empty"));
    }

    #[test]
    fn new_file_paths_resolve() {
        let (_d, g) = guard();
        let p = g.resolve("src/new/deep/file.rs").unwrap();
        assert_eq!(p, g.root().join("src/new/deep/file.rs"));
        let p = g.resolve("src/new/../other.rs").unwrap();
        assert_eq!(p, g.root().join("src/other.rs"));
    }

    #[test]
    fn new_paths_take_the_on_disk_spelling() {
        let (d, g) = guard();
        std::fs::create_dir_all(d.path().join(".git/hooks")).unwrap();
        let case_insensitive = d.path().join(".GIT").exists();
        let p = g.resolve(".Git/hooks/pre-commit").unwrap();
        if case_insensitive {
            assert_eq!(p, g.root().join(".git/hooks/pre-commit"));
        } else {
            assert_eq!(p, g.root().join(".Git/hooks/pre-commit"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_rejected() {
        let (d, g) = guard();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "s").unwrap();
        std::os::unix::fs::symlink(outside.path(), d.path().join("link")).unwrap();
        let err = g.resolve("link/secret.txt").unwrap_err();
        assert!(err.contains("symlink"), "{err}");
        // A new file under the escaping link is rejected too.
        assert!(g.resolve("link/new.txt").is_err());
        // A file symlink pointing outside.
        std::os::unix::fs::symlink(
            outside.path().join("secret.txt"),
            d.path().join("secret_link"),
        )
        .unwrap();
        assert!(g.resolve("secret_link").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_inside_is_followed() {
        let (d, g) = guard();
        std::os::unix::fs::symlink(d.path().join("src"), d.path().join("alias")).unwrap();
        let p = g.resolve("alias/main.rs").unwrap();
        assert_eq!(p, g.root().join("src/main.rs"));
        // `..` after a symlink is the target's parent.
        let p = g.resolve("alias/../src/main.rs").unwrap();
        assert_eq!(p, g.root().join("src/main.rs"));
    }

    #[cfg(unix)]
    #[test]
    fn dangling_symlink_rejected() {
        let (d, g) = guard();
        std::os::unix::fs::symlink("/nonexistent/target/file", d.path().join("dangling")).unwrap();
        let err = g.resolve("dangling").unwrap_err();
        assert!(err.contains("broken symlink"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn symlink_loop_rejected() {
        let (d, g) = guard();
        std::os::unix::fs::symlink(d.path().join("b"), d.path().join("a")).unwrap();
        std::os::unix::fs::symlink(d.path().join("a"), d.path().join("b")).unwrap();
        assert!(g.resolve("a/x").is_err());
    }
}
