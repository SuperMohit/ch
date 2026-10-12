//! Read-before-write tracking.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use sha2::{Digest, Sha256};

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
#[derive(Debug, Default)]
pub struct FileState {
    entries: Mutex<HashMap<PathBuf, Entry>>,
    /// Paths in messages are shown relative to this root when set.
    display_root: Option<PathBuf>,
}

/// SHA-256 of `bytes`.
pub fn content_hash(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

impl FileState {
    /// A tracker that shows paths relative to `root` in its messages.
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self {
            entries: Mutex::default(),
            display_root: Some(root.into()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<PathBuf, Entry>> {
        // A panic while holding the lock can't leave the map half-updated (every
        // update is a single insert), so a poisoned lock is still usable.
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn display(&self, path: &Path) -> String {
        match &self.display_root {
            Some(root) => match path.strip_prefix(root) {
                Ok(rel) => rel.display().to_string(),
                Err(_) => path.display().to_string(),
            },
            None => path.display().to_string(),
        }
    }

    /// Records that the agent read `path`, whose current content is `content`. `full`
    /// is true when the whole file was shown. A partial read never downgrades an
    /// earlier full read of the same content.
    pub fn record_read(&self, path: &Path, content: &[u8], full: bool) {
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
        path: &Path,
        content: &[u8],
        start: usize,
        end: usize,
        total_lines: usize,
    ) {
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

    /// Ok if the agent may change `path`: it doesn't exist yet, or it was fully read
    /// and is unchanged on disk since. The error tells the model what to do.
    pub fn check_writable(&self, path: &Path) -> Result<(), String> {
        let shown = self.display(path);
        let current = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(format!("Could not read `{shown}` to check it: {e}.")),
        };
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
        Ok(())
    }

    /// Records that the agent wrote `content` to `path`; it counts as fully read.
    pub fn record_write(&self, path: &Path, content: &[u8]) {
        self.lock().insert(
            path.to_path_buf(),
            Entry {
                hash: content_hash(content),
                full_read: true,
                seen: Vec::new(),
            },
        );
    }

    /// Whether `path` is currently recorded as fully read (ignores on-disk changes).
    pub fn is_fully_read(&self, path: &Path) -> bool {
        self.lock().get(path).is_some_and(|e| e.full_read)
    }

    /// Forgets `path` (after it was deleted, for example).
    pub fn forget(&self, path: &Path) {
        self.lock().remove(path);
    }
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

    fn setup() -> (tempfile::TempDir, FileState, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "one\ntwo\n").unwrap();
        (dir, FileState::default(), path)
    }

    #[test]
    fn new_files_are_writable() {
        let (dir, fs, _) = setup();
        assert!(fs.check_writable(&dir.path().join("new.txt")).is_ok());
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
        let (dir, _, path) = setup();
        let fs = FileState::with_root(dir.path());
        let err = fs.check_writable(&path).unwrap_err();
        assert!(err.contains("`a.txt`"), "{err}");
    }

    #[test]
    fn ranges_merge() {
        assert_eq!(merge_ranges(vec![(5, 9), (1, 3), (4, 4)]), vec![(1, 9)]);
        assert_eq!(merge_ranges(vec![(1, 2), (5, 6)]), vec![(1, 2), (5, 6)]);
    }
}
