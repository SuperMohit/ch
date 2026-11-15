//! Keeps every file path inside the workspace root.
//!
//! [`PathGuard`] turns a model-supplied path into a [`WorkspacePath`] (inside the
//! workspace, so the file tools may write it) or, for reads, a [`ReadablePath`] (which
//! may also be inside the session's own directory, never writable). [`crate::FileState`],
//! which clears a file for writing, takes only a [`WorkspacePath`], so a path in the
//! session directory can't be cleared for a write: the compiler refuses it.

use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use ch_kernel::util::anchored;

/// Symlinks followed while resolving one path before giving up (loops, deep chains).
const MAX_SYMLINKS: usize = 40;

/// A canonical path inside the workspace root: one the file tools may change.
///
/// Only [`PathGuard`] makes one, so holding one means the path passed the guard when it
/// was resolved. It isn't checked again before a write: a parent directory replaced by
/// a symlink in between isn't caught here. [`crate::FileState`] takes nothing else:
///
/// ```
/// # fn f(guard: &ch_safety::PathGuard, state: &ch_safety::FileState) -> Result<(), String> {
/// let path = guard.resolve("src/main.rs")?;
/// state.check_writable(&path)?;
/// # Ok(())
/// # }
/// ```
///
/// A plain path, or a path in the session directory from
/// [`PathGuard::resolve_readable`], is refused by the compiler:
///
/// ```compile_fail,E0308
/// # fn f(state: &ch_safety::FileState) {
/// let path = std::path::Path::new("src/main.rs");
/// state.check_writable(path); // expected `&WorkspacePath`, found `&Path`
/// # }
/// ```
///
/// Displays relative to the workspace root (`.` for the root itself), as messages to
/// the model show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspacePath {
    path: PathBuf,
    root: Arc<Path>,
}

impl WorkspacePath {
    /// The canonical absolute path.
    pub fn as_path(&self) -> &Path {
        &self.path
    }

    /// The canonical absolute path, owned.
    pub fn into_path_buf(self) -> PathBuf {
        self.path
    }

    /// The path relative to the workspace root, `.` for the root itself (as
    /// [`PathGuard::relative`] shows it).
    pub fn relative(&self) -> String {
        relative_to(&self.root, &self.path)
    }
}

impl AsRef<Path> for WorkspacePath {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl fmt::Display for WorkspacePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.relative())
    }
}

/// A canonical path the file tools may read: inside the workspace, or inside the
/// session's own directory, where plugins keep files for the model to read back (spilled
/// output, elided results). Only the workspace side is a [`WorkspacePath`], so a
/// session file can't be handed to anything that writes:
///
/// ```compile_fail,E0308
/// # fn f(guard: &ch_safety::PathGuard, state: &ch_safety::FileState, dir: &std::path::Path) {
/// let path = guard.resolve_readable("spill/out.txt", Some(dir)).unwrap();
/// state.check_writable(&path); // expected `&WorkspacePath`, found `&ReadablePath`
/// # }
/// ```
///
/// Only its workspace side can be:
///
/// ```
/// # fn f(guard: &ch_safety::PathGuard, state: &ch_safety::FileState, dir: &std::path::Path)
/// #     -> Result<(), String> {
/// let path = guard.resolve_readable("spill/out.txt", Some(dir))?;
/// if let Some(p) = path.workspace() {
///     state.check_writable(p)?;
/// }
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadablePath {
    /// Inside the workspace (also when the session directory is inside it too).
    Workspace(WorkspacePath),
    /// Inside the session directory and outside the workspace: read only.
    Session(SessionPath),
}

/// A canonical path inside the session directory and outside the workspace. Only
/// [`PathGuard::resolve_readable`] makes one; the file tools read it and never write it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionPath(PathBuf);

impl SessionPath {
    /// The canonical absolute path.
    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

impl ReadablePath {
    /// The canonical absolute path.
    pub fn as_path(&self) -> &Path {
        match self {
            Self::Workspace(p) => p.as_path(),
            Self::Session(p) => p.as_path(),
        }
    }

    /// The path if it is inside the workspace.
    pub fn workspace(&self) -> Option<&WorkspacePath> {
        match self {
            Self::Workspace(p) => Some(p),
            Self::Session(_) => None,
        }
    }

    /// The canonical absolute path, owned.
    pub fn into_path_buf(self) -> PathBuf {
        match self {
            Self::Workspace(p) => p.into_path_buf(),
            Self::Session(p) => p.0,
        }
    }
}

/// Resolves model-supplied paths against the workspace root and rejects anything that
/// ends up outside it, whether through `..`, an absolute path or a symlink.
///
/// Paths that don't exist yet (a file about to be written) are resolved by following
/// the existing part physically and appending the rest.
///
/// Hard links can't be seen from a path: a file inside the workspace may be the same
/// file as one outside it. Reading one is allowed. [`crate::FileState::check_writable`]
/// refuses to change a file with more than one link (on Unix).
#[derive(Debug, Clone)]
pub struct PathGuard {
    root: Arc<Path>,
}

/// A model-supplied path followed on disk, not yet checked against the roots.
struct Found<'a> {
    /// The input, trimmed.
    input: &'a str,
    /// The input made absolute, before any symlink is followed.
    joined: PathBuf,
    resolved: PathBuf,
}

impl PathGuard {
    /// Canonicalizes `root`; fails if it does not exist.
    pub fn new(root: impl AsRef<Path>) -> io::Result<Self> {
        Ok(Self {
            root: std::fs::canonicalize(root.as_ref())?.into(),
        })
    }

    /// The canonical workspace root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolves `input` (relative to the workspace root, or absolute) to a canonical
    /// path inside the workspace. The error is written for the model.
    pub fn resolve(&self, input: &str) -> Result<WorkspacePath, String> {
        let found = self.find(input)?;
        if self.contains(&found.resolved) {
            return Ok(self.inside(found.resolved));
        }
        Err(self.refusal(&found))
    }

    /// Like [`PathGuard::resolve`] for a path that is only read: it may also be inside
    /// `session_dir`, the session's own directory, while that is a real directory (not
    /// a symlink). Such a path comes back as [`ReadablePath::Session`], which nothing
    /// that writes accepts.
    pub fn resolve_readable(
        &self,
        input: &str,
        session_dir: Option<&Path>,
    ) -> Result<ReadablePath, String> {
        let session = session_dir
            .filter(|d| std::fs::symlink_metadata(d).is_ok_and(|m| m.is_dir()))
            .and_then(|d| anchored(d).ok());
        let found = self.find(input)?;
        if self.contains(&found.resolved) {
            return Ok(ReadablePath::Workspace(self.inside(found.resolved)));
        }
        if session.is_some_and(|d| found.resolved.starts_with(d)) {
            return Ok(ReadablePath::Session(SessionPath(found.resolved)));
        }
        Err(self.refusal(&found))
    }

    fn inside(&self, path: PathBuf) -> WorkspacePath {
        WorkspacePath {
            path,
            root: self.root.clone(),
        }
    }

    fn find<'a>(&self, input: &'a str) -> Result<Found<'a>, String> {
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
        Ok(Found {
            input: trimmed,
            joined,
            resolved,
        })
    }

    /// Why `found` is refused: it is outside every root it may be in.
    fn refusal(&self, found: &Found<'_>) -> String {
        let Found {
            input,
            joined,
            resolved,
        } = found;
        let lexical = normalize_lexically(joined);
        let via_symlink = lexical.starts_with(&self.root);
        if via_symlink {
            format!(
                "`{input}` resolves through a symlink to `{}`, which is outside the \
                 workspace. Paths must be inside `{}`; work with files in the workspace \
                 instead.",
                resolved.display(),
                self.root.display()
            )
        } else {
            format!(
                "`{input}` is outside the workspace. Paths must be inside `{}`; use a \
                 path relative to it, like `src/main.rs`.",
                self.root.display()
            )
        }
    }

    /// `path` relative to the workspace root for display (`.` for the root itself);
    /// paths outside the root are shown in full.
    pub fn relative(&self, path: &Path) -> String {
        relative_to(&self.root, path)
    }

    /// Whether `path` (already canonical) is inside the workspace.
    fn contains(&self, path: &Path) -> bool {
        path.starts_with(&self.root)
    }
}

fn relative_to(root: &Path, path: &Path) -> String {
    match path.strip_prefix(root) {
        Ok(rel) if rel.as_os_str().is_empty() => ".".to_string(),
        Ok(rel) => rel.display().to_string(),
        Err(_) => path.display().to_string(),
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
    fn the_session_dir_is_readable_but_not_resolvable_for_writes() {
        let (_d, g) = guard();
        let outside = tempfile::tempdir().unwrap();
        // Not canonical on macOS (`/var` is a symlink), like a real session dir may be.
        let session = outside.path().join("s1");
        std::fs::create_dir_all(session.join("spill")).unwrap();
        std::fs::write(session.join("spill/out.txt"), "full output").unwrap();
        std::fs::write(outside.path().join("secret.txt"), "no").unwrap();
        let file = session.join("spill/out.txt");
        let file = file.to_str().unwrap();
        let canonical = std::fs::canonicalize(&session)
            .unwrap()
            .join("spill/out.txt");

        let s = Some(session.as_path());
        assert_eq!(
            g.resolve_readable(file, s).unwrap(),
            ReadablePath::Session(SessionPath(canonical))
        );
        assert!(g.resolve(file).is_err(), "writes still need the workspace");
        assert!(g.resolve_readable(file, None).is_err());
        assert_eq!(
            g.resolve_readable("src/main.rs", s).unwrap(),
            ReadablePath::Workspace(g.resolve("src/main.rs").unwrap()),
            "the workspace still works"
        );
        // A session dir that is a file, or not there yet, adds nothing.
        let not_dir = outside.path().join("s1/spill/out.txt");
        assert!(g.resolve_readable(file, Some(&not_dir)).is_err());
        let missing = outside.path().join("s9");
        let in_missing = missing.join("out.txt");
        assert!(
            g.resolve_readable(in_missing.to_str().unwrap(), Some(&missing))
                .is_err()
        );

        // `..` out of it, or a neighbouring session, stays outside.
        let up = session.join("../secret.txt");
        assert!(g.resolve_readable(up.to_str().unwrap(), s).is_err());
        let other = outside.path().join("s10/spill");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("x.txt"), "other session").unwrap();
        let other = other.join("x.txt");
        assert!(g.resolve_readable(other.to_str().unwrap(), s).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_in_or_at_the_session_dir_lead_nowhere() {
        use std::os::unix::fs::symlink;
        let (_d, g) = guard();
        let outside = tempfile::tempdir().unwrap();
        let secrets = outside.path().join("secrets");
        std::fs::create_dir(&secrets).unwrap();
        std::fs::write(secrets.join("key"), "no").unwrap();

        // A symlink inside the session dir.
        let session = outside.path().join("s1");
        std::fs::create_dir(&session).unwrap();
        symlink(&secrets, session.join("spill")).unwrap();
        let via = session.join("spill/key");
        assert!(
            g.resolve_readable(via.to_str().unwrap(), Some(&session))
                .is_err()
        );

        // The session dir itself replaced by a symlink.
        let swapped = outside.path().join("s2");
        symlink(&secrets, &swapped).unwrap();
        let via = swapped.join("key");
        assert!(
            g.resolve_readable(via.to_str().unwrap(), Some(&swapped))
                .is_err()
        );
    }

    #[test]
    fn relative_and_absolute_inside() {
        let (_d, g) = guard();
        let p = g.resolve("src/main.rs").unwrap();
        assert_eq!(p.as_path(), g.root().join("src/main.rs"));
        let abs = g.root().join("src/main.rs");
        assert_eq!(g.resolve(abs.to_str().unwrap()).unwrap(), p);
        assert_eq!(g.resolve("./src/../src/main.rs").unwrap(), p);
        let root = g.resolve(".").unwrap();
        assert_eq!(root.as_path(), g.root());
        assert_eq!(g.relative(p.as_path()), "src/main.rs");
        assert_eq!(g.relative(g.root()), ".");
        // A workspace path shows itself the same way.
        assert_eq!(p.relative(), "src/main.rs");
        assert_eq!(p.to_string(), "src/main.rs");
        assert_eq!(root.to_string(), ".");
    }

    #[test]
    fn a_session_dir_inside_the_workspace_stays_writable() {
        let (d, g) = guard();
        std::fs::create_dir_all(d.path().join(".ch/s1")).unwrap();
        let session = d.path().join(".ch/s1");
        let file = session.join("notes.txt");
        let file = file.to_str().unwrap();
        let read = g.resolve_readable(file, Some(&session)).unwrap();
        let write = g.resolve(file).unwrap();
        assert_eq!(read.workspace(), Some(&write));
        assert_eq!(read.as_path(), g.root().join(".ch/s1/notes.txt"));
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
        assert_eq!(p.as_path(), g.root().join("src/new/deep/file.rs"));
        let p = g.resolve("src/new/../other.rs").unwrap();
        assert_eq!(p.as_path(), g.root().join("src/other.rs"));
    }

    #[test]
    fn new_paths_take_the_on_disk_spelling() {
        let (d, g) = guard();
        std::fs::create_dir_all(d.path().join(".git/hooks")).unwrap();
        let case_insensitive = d.path().join(".GIT").exists();
        let p = g.resolve(".Git/hooks/pre-commit").unwrap();
        if case_insensitive {
            assert_eq!(p.as_path(), g.root().join(".git/hooks/pre-commit"));
        } else {
            assert_eq!(p.as_path(), g.root().join(".Git/hooks/pre-commit"));
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
        assert_eq!(p.as_path(), g.root().join("src/main.rs"));
        // `..` after a symlink is the target's parent.
        let p = g.resolve("alias/../src/main.rs").unwrap();
        assert_eq!(p.as_path(), g.root().join("src/main.rs"));
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
