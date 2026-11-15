//! Read-before-write tracking.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use sha2::{Digest, Sha256};

use crate::path_guard::WorkspacePath;

type Hash = [u8; 32];

/// What the agent has seen of one file.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    /// Hash of the content when it was last read or written.
    hash: Hash,
    full_read: bool,
    /// Line ranges (1-based, inclusive) seen through partial reads of this content.
    seen: Vec<(usize, usize)>,
}

/// Remembers, per file, the hash of the content the agent last saw and whether it saw
/// all of it.
///
/// Edits are only allowed on files that were fully read and haven't changed on disk
/// since. Because the hash is checked again at edit time, changes made through `bash`
/// or by the user can't silently bypass read-before-write.
///
/// Every method takes a [`WorkspacePath`], so only files inside the workspace are
/// tracked or cleared for writing; a file the model reads in the session directory is
/// never written by the file tools, so its reads are not recorded.
#[derive(Debug, Default)]
pub struct FileState {
    entries: Mutex<HashMap<PathBuf, Entry>>,
}

/// SHA-256 of `bytes`.
pub fn content_hash(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

impl FileState {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<PathBuf, Entry>> {
        // A panic while holding the lock can't leave the map half-updated (every
        // update is a single insert), so a poisoned lock is still usable.
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Records that the agent read `path`, whose current content is `content`. `full`
    /// is true when the whole file was shown. A partial read never downgrades an
    /// earlier full read of the same content.
    pub fn record_read(&self, path: &WorkspacePath, content: &[u8], full: bool) {
        let path = path.as_path();
        let hash = content_hash(content);
        let mut map = self.lock();
        let prev = map.get(path).filter(|e| e.hash == hash);
        let full_read = full || prev.is_some_and(|e| e.full_read);
        let seen = prev.map(|e| e.seen.clone()).unwrap_or_default();
        map.insert(
            path.to_path_buf(),
            Entry {
                hash,
                full_read,
                seen,
            },
        );
    }

    /// Records that lines `start..=end` (1-based) of a file with `total_lines` lines
    /// were shown. Once partial reads of the same content cover every line, the file
    /// counts as fully read.
    pub fn record_lines(
        &self,
        path: &WorkspacePath,
        content: &[u8],
        start: usize,
        end: usize,
        total_lines: usize,
    ) {
        let path = path.as_path();
        let hash = content_hash(content);
        let mut map = self.lock();
        let (mut seen, was_full) = match map.get(path).filter(|e| e.hash == hash) {
            Some(e) => (e.seen.clone(), e.full_read),
            None => (Vec::new(), false),
        };
        if start <= end {
            seen.push((start.max(1), end));
        }
        let seen = merge_ranges(seen);
        let covers_all = total_lines == 0
            || seen
                .first()
                .is_some_and(|&(s, e)| s <= 1 && e >= total_lines);
        map.insert(
            path.to_path_buf(),
            Entry {
                hash,
                full_read: was_full || covers_all,
                seen,
            },
        );
    }

    /// Ok if the agent may change `path`: it doesn't exist yet (`None`), or it was fully
    /// read, is unchanged on disk since and has no other hard links (`Some` of the bytes
    /// that were checked, so the caller changes exactly what was verified). The error
    /// tells the model what to do.
    pub fn check_writable(&self, path: &WorkspacePath) -> Result<Option<Vec<u8>>, String> {
        let shown = path.relative();
        let path = path.as_path();
        let current = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("Could not read `{shown}` to check it: {e}.")),
        };
        // A hard link is the same file under another name, maybe outside the workspace,
        // where the path checks can't see it.
        if let Some(links) = hard_links(path).filter(|&n| n > 1) {
            return Err(format!(
                "`{shown}` has {links} hard links, so changing it would also change the file \
                 under its other names, which may be outside the workspace. The file tools \
                 don't change hard-linked files; if the change is intended, use bash."
            ));
        }
        let map = self.lock();
        let Some(entry) = map.get(path) else {
            return Err(format!(
                "You haven't read `{shown}` yet. Read the whole file with read_file before \
                 editing it."
            ));
        };
        if entry.hash != content_hash(&current) {
            return Err(format!(
                "`{shown}` changed on disk since you last read it (maybe by bash or the \
                 user). Read it again before editing."
            ));
        }
        if !entry.full_read {
            return Err(format!(
                "You have only read part of `{shown}`. Read the whole file with read_file \
                 before editing it (call it without offset/limit, or page through every \
                 line)."
            ));
        }
        Ok(Some(current))
    }

    /// Records that the agent wrote `content` to `path`; it counts as fully read.
    pub fn record_write(&self, path: &WorkspacePath, content: &[u8]) {
        self.lock().insert(
            path.as_path().to_path_buf(),
            Entry {
                hash: content_hash(content),
                full_read: true,
                seen: Vec::new(),
            },
        );
    }

    /// Whether `path` is currently recorded as fully read (ignores on-disk changes).
    pub fn is_fully_read(&self, path: &WorkspacePath) -> bool {
        self.lock().get(path.as_path()).is_some_and(|e| e.full_read)
    }

    /// Forgets `path` (after it was deleted, for example).
    pub fn forget(&self, path: &WorkspacePath) {
        self.lock().remove(path.as_path());
    }
}

#[cfg(unix)]
fn hard_links(path: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).ok().map(|m| m.nlink())
}

#[cfg(not(unix))]
fn hard_links(_path: &Path) -> Option<u64> {
    None
}

/// Sorts and merges overlapping or adjacent inclusive ranges.
fn merge_ranges(mut ranges: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    ranges.sort_unstable();
    let mut out: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
    for (s, e) in ranges {
        match out.last_mut() {
            Some(last) if s <= last.1.saturating_add(1) => last.1 = last.1.max(e),
            _ => out.push((s, e)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PathGuard;

    /// A workspace holding `a.txt`, its guard, a tracker and `a.txt`'s path.
    fn workspace() -> (tempfile::TempDir, PathGuard, FileState, WorkspacePath) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
        let guard = PathGuard::new(dir.path()).unwrap();
        let path = guard.resolve("a.txt").unwrap();
        (dir, guard, FileState::default(), path)
    }

    fn setup() -> (tempfile::TempDir, FileState, WorkspacePath) {
        let (dir, _, fs, path) = workspace();
        (dir, fs, path)
    }

    #[test]
    fn new_files_are_writable() {
        let (_d, guard, fs, _) = workspace();
        let new = guard.resolve("new.txt").unwrap();
        assert_eq!(fs.check_writable(&new), Ok(None));
    }

    #[cfg(unix)]
    #[test]
    fn hard_linked_files_are_not_writable() {
        let (_d, fs, path) = setup();
        let bytes = std::fs::read(&path).unwrap();
        fs.record_read(&path, &bytes, true);
        let outside = tempfile::tempdir().unwrap();
        let other = outside.path().join("other.txt");
        std::fs::hard_link(&path, &other).unwrap();
        let err = fs.check_writable(&path).unwrap_err();
        assert!(err.contains("has 2 hard links"), "{err}");
        std::fs::remove_file(&other).unwrap();
        assert!(fs.check_writable(&path).is_ok());
    }

    #[test]
    fn writable_returns_the_checked_bytes() {
        let (_d, fs, path) = setup();
        fs.record_read(&path, b"one\ntwo\n", true);
        assert_eq!(fs.check_writable(&path), Ok(Some(b"one\ntwo\n".to_vec())));
    }

    #[test]
    fn unread_file_is_rejected() {
        let (_d, fs, path) = setup();
        let err = fs.check_writable(&path).unwrap_err();
        assert!(err.contains("Read the whole file with read_file"), "{err}");
    }

    #[test]
    fn partial_read_is_rejected_full_read_ok() {
        let (_d, fs, path) = setup();
        let bytes = std::fs::read(&path).unwrap();
        fs.record_read(&path, &bytes, false);
        let err = fs.check_writable(&path).unwrap_err();
        assert!(err.contains("only read part"), "{err}");
        fs.record_read(&path, &bytes, true);
        assert!(fs.check_writable(&path).is_ok());
        // A later partial read of the same content keeps the full read.
        fs.record_read(&path, &bytes, false);
        assert!(fs.check_writable(&path).is_ok());
    }

    #[test]
    fn partial_reads_covering_everything_count_as_full() {
        let (_d, fs, path) = setup();
        let bytes = std::fs::read(&path).unwrap();
        fs.record_lines(&path, &bytes, 2, 2, 2);
        assert!(!fs.is_fully_read(&path));
        fs.record_lines(&path, &bytes, 1, 1, 2);
        assert!(fs.is_fully_read(&path));
    }

    #[test]
    fn external_change_is_detected() {
        let (_d, fs, path) = setup();
        fs.record_read(&path, &std::fs::read(&path).unwrap(), true);
        std::fs::write(&path, "changed by bash\n").unwrap();
        let err = fs.check_writable(&path).unwrap_err();
        assert!(err.contains("changed on disk"), "{err}");
    }

    #[test]
    fn writes_count_as_reads() {
        let (_d, fs, path) = setup();
        std::fs::write(&path, "new").unwrap();
        fs.record_write(&path, b"new");
        assert!(fs.check_writable(&path).is_ok());
    }

    #[test]
    fn messages_use_relative_paths() {
        let (_d, _guard, _, path) = workspace();
        let fs = FileState::default();
        let err = fs.check_writable(&path).unwrap_err();
        assert!(err.contains("`a.txt`"), "{err}");
    }

    #[test]
    fn ranges_merge() {
        assert_eq!(merge_ranges(vec![(5, 9), (1, 3), (4, 4)]), vec![(1, 9)]);
        assert_eq!(merge_ranges(vec![(1, 2), (5, 6)]), vec![(1, 2), (5, 6)]);
    }
}
