//! Where spilled output goes.

use std::fs::{self, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ch_kernel::{Cx, ReadOnlyRoots};

/// Same-named files allowed in one directory before saving gives up.
const MAX_NAME_TRIES: usize = 100;

/// Saves spilled output and registers its directories as readable.
pub struct SpillStore {
    roots: Arc<ReadOnlyRoots>,
    /// Used when the session has no directory (`--no-save`); removed when dropped.
    temp: Mutex<Option<tempfile::TempDir>>,
}

impl SpillStore {
    pub fn new(roots: Arc<ReadOnlyRoots>) -> Self {
        Self {
            roots,
            temp: Mutex::default(),
        }
    }

    /// Writes `content` to a new file named after the call and returns its canonical
    /// path, inside a directory now in [`ReadOnlyRoots`].
    pub fn save(&self, cx: &Cx, call_id: &str, tool: &str, content: &str) -> io::Result<PathBuf> {
        let dir = self.dir(cx)?;
        let base = format!("{}-{}", safe(call_id), safe(tool));
        for n in 0..MAX_NAME_TRIES {
            let name = match n {
                0 => format!("{base}.txt"),
                n => format!("{base}-{n}.txt"),
            };
            let path = dir.join(name);
            match create_private(&path) {
                Ok(mut file) => {
                    file.write_all(content.as_bytes())?;
                    file.sync_all()?;
                    return Ok(path);
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::other(format!(
            "{MAX_NAME_TRIES} files named {base}*.txt already exist in {}",
            dir.display()
        )))
    }

    /// The canonical spill directory for `cx`'s session, created `0700` if needed.
    fn dir(&self, cx: &Cx) -> io::Result<PathBuf> {
        let dir = match &cx.session_dir {
            Some(session) => {
                let dir = session.join("spill");
                fs::create_dir_all(&dir)?;
                restrict(&dir)?;
                dir
            }
            None => {
                let mut temp = self.temp.lock().unwrap_or_else(|p| p.into_inner());
                if temp.is_none() {
                    // `tempdir` creates the directory `0700` on Unix.
                    *temp = Some(tempfile::Builder::new().prefix("ch-spill-").tempdir()?);
                }
                temp.as_ref().expect("just set").path().to_path_buf()
            }
        };
        let dir = fs::canonicalize(dir)?;
        self.roots.add(dir.clone());
        Ok(dir)
    }
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

    #[test]
    fn names_are_sanitized_and_never_overwrite() {
        assert_eq!(safe("toolu_01/../x"), "toolu_01____x");
        assert_eq!(safe("mcp__gh__search"), "mcp__gh__search");
        assert_eq!(safe(""), "result");
        assert_eq!(safe(&"a".repeat(100)).len(), 64);

        let tmp = tempfile::tempdir().unwrap();
        let store = SpillStore::new(Arc::default());
        let mut cx = Cx::detached("/ws");
        cx.session_dir = Some(Arc::new(tmp.path().to_path_buf()));
        let a = store.save(&cx, "c1", "bash", "first").unwrap();
        let b = store.save(&cx, "c1", "bash", "second").unwrap();
        assert_ne!(a, b);
        assert!(b.ends_with("c1-bash-1.txt"), "{}", b.display());
        assert_eq!(fs::read_to_string(&a).unwrap(), "first");
        assert_eq!(fs::read_to_string(&b).unwrap(), "second");
    }

    #[cfg(unix)]
    #[test]
    fn a_planted_symlink_is_not_followed() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("target.txt");
        fs::write(&target, "keep").unwrap();
        let spill = tmp.path().join("s/spill");
        fs::create_dir_all(&spill).unwrap();
        std::os::unix::fs::symlink(&target, spill.join("c1-bash.txt")).unwrap();

        let store = SpillStore::new(Arc::default());
        let mut cx = Cx::detached("/ws");
        cx.session_dir = Some(Arc::new(tmp.path().join("s")));
        let path = store.save(&cx, "c1", "bash", "spilled").unwrap();
        assert!(path.ends_with("c1-bash-1.txt"));
        assert_eq!(fs::read_to_string(&target).unwrap(), "keep");
    }
}
