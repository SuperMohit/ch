//! Where spilled output goes: `<session dir>/spill/`.

use std::fs::{self, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

/// Attempts at a fresh random file name before saving gives up.
const NAME_TRIES: usize = 3;

/// Writes `content` to a new file in `session_dir`'s `spill` directory and returns the
/// path to show the model. Refuses when the directory isn't a real directory inside
/// the session directory, or when the session's spill files would pass `max_bytes`.
pub fn save(
    session_dir: &Path,
    call_id: &str,
    tool: &str,
    content: &str,
    max_bytes: u64,
) -> io::Result<PathBuf> {
    let dir = spill_dir(session_dir)?;
    let used = used_bytes(&dir)?;
    let size = content.len() as u64;
    if used.saturating_add(size) > max_bytes {
        return Err(io::Error::other(format!(
            "the session's spill files would pass plugins.spill.max_session_bytes \
             ({used} + {size} > {max_bytes} bytes)"
        )));
    }
    let base = format!("{}-{}", safe(call_id), safe(tool));
    for _ in 0..NAME_TRIES {
        let suffix = &uuid::Uuid::new_v4().simple().to_string()[..8];
        let path = dir.join(format!("{base}-{suffix}.txt"));
        let mut file = match create_private(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        };
        // A cache of output the model already got in part: losing it on a crash costs
        // nothing, so there is no fsync.
        if let Err(e) = file.write_all(content.as_bytes()) {
            drop(file);
            let _ = fs::remove_file(&path);
            return Err(e);
        }
        return Ok(path);
    }
    Err(io::Error::other(format!(
        "no free file name for {base} in {}",
        dir.display()
    )))
}

/// `<session dir>/spill`, created `0700` if needed, with the session directory's parent
/// canonicalized (the same anchor the file tools check reads against). Fails if either
/// directory is a symlink, so nothing placed there can redirect where output goes.
fn spill_dir(session_dir: &Path) -> io::Result<PathBuf> {
    fs::create_dir_all(session_dir)?;
    let session = anchored(session_dir)?;
    let dir = session.join("spill");
    match fs::create_dir(&dir) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let meta = fs::symlink_metadata(&dir)?;
    if !meta.is_dir() {
        return Err(io::Error::other(format!(
            "{} is not a directory (a symlink?)",
            dir.display()
        )));
    }
    restrict(&dir)?;
    Ok(dir)
}

/// `dir` with its parent canonicalized and its own name kept; fails if `dir` is a
/// symlink.
fn anchored(dir: &Path) -> io::Result<PathBuf> {
    if fs::symlink_metadata(dir)?.file_type().is_symlink() {
        return Err(io::Error::other(format!("{} is a symlink", dir.display())));
    }
    let (Some(parent), Some(name)) = (dir.parent(), dir.file_name()) else {
        return Err(io::Error::other(format!("{} has no parent", dir.display())));
    };
    Ok(fs::canonicalize(parent)?.join(name))
}

/// Bytes in the regular files directly in `dir`.
fn used_bytes(dir: &Path) -> io::Result<u64> {
    let mut used = 0u64;
    for entry in fs::read_dir(dir)? {
        let meta = entry?.metadata()?;
        if meta.is_file() {
            used = used.saturating_add(meta.len());
        }
    }
    Ok(used)
}

/// `s` with everything but ASCII letters, digits, `-` and `_` replaced, at most 64
/// characters, so ids and tool names are safe in a file name.
fn safe(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .take(64)
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' => c,
            _ => '_',
        })
        .collect();
    if cleaned.is_empty() {
        "result".into()
    } else {
        cleaned
    }
}

/// Creates `path`, failing if anything (a file or a symlink) is already there, readable
/// only by the user.
fn create_private(path: &Path) -> io::Result<fs::File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)
}

#[cfg(unix)]
fn restrict(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn restrict(_dir: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const BIG: u64 = 1 << 30;

    #[test]
    fn names_are_sanitized_and_never_overwrite() {
        assert_eq!(safe("toolu_01/../x"), "toolu_01____x");
        assert_eq!(safe("mcp__gh__search"), "mcp__gh__search");
        assert_eq!(safe(""), "result");
        assert_eq!(safe(&"a".repeat(100)).len(), 64);

        let tmp = tempfile::tempdir().unwrap();
        let session = tmp.path().join("s1");
        let a = save(&session, "call_0", "bash", "first", BIG).unwrap();
        let b = save(&session, "call_0", "bash", "second", BIG).unwrap();
        assert_ne!(a, b);
        let name = b.file_name().unwrap().to_str().unwrap();
        assert!(
            name.starts_with("call_0-bash-") && name.ends_with(".txt"),
            "{name}"
        );
        assert_eq!(fs::read_to_string(&a).unwrap(), "first");
        assert_eq!(fs::read_to_string(&b).unwrap(), "second");
        let canonical_parent = fs::canonicalize(tmp.path()).unwrap();
        assert!(
            a.starts_with(canonical_parent.join("s1/spill")),
            "{}",
            a.display()
        );
    }

    #[test]
    fn the_session_budget_is_enforced() {
        let tmp = tempfile::tempdir().unwrap();
        let session = tmp.path().join("s1");
        save(&session, "c1", "bash", &"x".repeat(600), 1000).unwrap();
        let err = save(&session, "c2", "bash", &"x".repeat(600), 1000).unwrap_err();
        assert!(err.to_string().contains("max_session_bytes"), "{err}");
        save(&session, "c3", "bash", &"x".repeat(400), 1000).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_directories_are_refused() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();

        let session = tmp.path().join("s1");
        fs::create_dir(&session).unwrap();
        symlink(&target, session.join("spill")).unwrap();
        assert!(save(&session, "c1", "bash", "x", BIG).is_err());

        let swapped = tmp.path().join("s2");
        symlink(&target, &swapped).unwrap();
        assert!(save(&swapped, "c1", "bash", "x", BIG).is_err());

        // The target is untouched: no file, and its mode is unchanged.
        assert_eq!(fs::read_dir(&target).unwrap().count(), 0);
        let mode = fs::metadata(&target).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
    }

    #[cfg(unix)]
    #[test]
    fn files_and_the_directory_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = save(&tmp.path().join("s1"), "c1", "bash", "x", BIG).unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
    }
}
