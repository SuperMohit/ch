//! Resolving the optional `cwd` argument inside the workspace.

use std::path::{Path, PathBuf};

/// Resolves `cwd` (relative to `workspace`, or absolute) to a canonical directory that
/// is inside the canonical workspace. Symlinks are resolved before the check, so a link
/// pointing outside is rejected. Errors are written for the model.
pub(crate) fn resolve_cwd(workspace: &Path, cwd: Option<&str>) -> Result<PathBuf, String> {
    let root = workspace.canonicalize().map_err(|e| {
        format!(
            "The workspace root {} is not accessible ({e}); bash cannot run.",
            workspace.display()
        )
    })?;
    let Some(cwd) = cwd.map(str::trim).filter(|c| !c.is_empty()) else {
        return Ok(root);
    };
    let joined = root.join(cwd);
    let resolved = joined.canonicalize().map_err(|_| {
        format!(
            "cwd `{cwd}` does not exist. Pass an existing directory relative to the \
             workspace root, or omit cwd to run at the root."
        )
    })?;
    if !resolved.starts_with(&root) {
        return Err(format!(
            "cwd `{cwd}` resolves to {}, which is outside the workspace {}. Commands \
             must run inside the workspace; pass a path relative to the workspace root.",
            resolved.display(),
            root.display()
        ));
    }
    if !resolved.is_dir() {
        return Err(format!(
            "cwd `{cwd}` is a file, not a directory. Pass its parent directory instead."
        ));
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_the_canonical_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        assert_eq!(resolve_cwd(dir.path(), None).unwrap(), root);
        assert_eq!(resolve_cwd(dir.path(), Some("  ")).unwrap(), root);
        assert_eq!(resolve_cwd(dir.path(), Some(".")).unwrap(), root);
    }

    #[test]
    fn subdirectories_resolve() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("a/b")).unwrap();
        let got = resolve_cwd(dir.path(), Some("a/b")).unwrap();
        assert!(got.ends_with("a/b"));
    }

    #[test]
    fn escapes_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("ws");
        std::fs::create_dir(&ws).unwrap();
        let err = resolve_cwd(&ws, Some("..")).unwrap_err();
        assert!(err.contains("outside the workspace"), "{err}");
        let err = resolve_cwd(&ws, Some("/")).unwrap_err();
        assert!(err.contains("outside the workspace"), "{err}");
    }

    #[test]
    fn symlink_escapes_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("ws");
        std::fs::create_dir(&ws).unwrap();
        std::os::unix::fs::symlink(dir.path(), ws.join("link")).unwrap();
        let err = resolve_cwd(&ws, Some("link")).unwrap_err();
        assert!(err.contains("outside the workspace"), "{err}");
    }

    #[test]
    fn missing_and_file_paths_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "x").unwrap();
        assert!(
            resolve_cwd(dir.path(), Some("nope"))
                .unwrap_err()
                .contains("does not exist")
        );
        assert!(
            resolve_cwd(dir.path(), Some("f.txt"))
                .unwrap_err()
                .contains("not a directory")
        );
    }
}
