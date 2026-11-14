//! What a run was made from, as hashes: the `ch` binary and image, the base config, the
//! cell's settings, and the task. Resuming onto runs made from something else would mix
//! two experiments in one report, so `ch-eval run` refuses, and the report flags cells
//! whose runs differ.

use std::fmt::Write as _;
use std::path::Path;

use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The hashes of what a run was made from.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct Fingerprint {
    /// The `ch` binary and, in a container, the task image's id.
    pub ch: String,
    /// The base config's text.
    pub config: String,
    /// `ch`'s arguments for the cell: profile, `--set` flags and the step limit.
    pub settings: String,
    /// `task.toml`.
    pub task: String,
    /// `repo/`.
    pub repo: String,
    /// `verify/`.
    pub verify: String,
}

impl Fingerprint {
    /// The names of the parts that differ from `other`.
    pub fn differences(&self, other: &Fingerprint) -> Vec<&'static str> {
        [
            ("ch binary or image", self.ch != other.ch),
            ("base config", self.config != other.config),
            ("settings", self.settings != other.settings),
            ("task.toml", self.task != other.task),
            ("repo/", self.repo != other.repo),
            ("verify/", self.verify != other.verify),
        ]
        .into_iter()
        .filter(|(_, differs)| *differs)
        .map(|(name, _)| name)
        .collect()
    }
}

/// SHA-256 of `bytes`, in hex.
pub fn hash(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for b in digest {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// The hash of a file's contents.
pub fn hash_file(path: &Path) -> anyhow::Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(hash(&bytes))
}

/// [`hash_file`], remembered for the process by path, size and modification time, so a
/// large binary is read once.
pub fn hash_file_cached(path: &Path) -> anyhow::Result<String> {
    type Key = (std::path::PathBuf, u64, Option<std::time::SystemTime>);
    static CACHE: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<Key, String>>> =
        std::sync::OnceLock::new();
    let meta = std::fs::metadata(path).with_context(|| format!("reading {}", path.display()))?;
    let key = (path.to_path_buf(), meta.len(), meta.modified().ok());
    let cache = CACHE.get_or_init(Default::default);
    if let Some(h) = cache.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
        return Ok(h.clone());
    }
    let h = hash_file(path)?;
    cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key, h.clone());
    Ok(h)
}

/// The hash of a directory tree: every entry's path, kind, executable bit and contents
/// (a symlink's target), in sorted order. A missing directory hashes as empty.
pub fn hash_tree(dir: &Path) -> anyhow::Result<String> {
    let mut listing = String::new();
    if dir.exists() {
        walk(dir, Path::new(""), &mut listing)
            .with_context(|| format!("reading {}", dir.display()))?;
    }
    Ok(hash(listing.as_bytes()))
}

fn walk(root: &Path, rel: &Path, out: &mut String) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut entries: Vec<_> = std::fs::read_dir(root.join(rel))?.collect::<Result<_, _>>()?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = rel.join(entry.file_name());
        let meta = std::fs::symlink_metadata(entry.path())?;
        let name = path.to_string_lossy();
        if meta.is_symlink() {
            let target = std::fs::read_link(entry.path())?;
            let _ = writeln!(out, "l {name:?} {:?}", target.to_string_lossy());
        } else if meta.is_dir() {
            let _ = writeln!(out, "d {name:?}");
            walk(root, &path, out)?;
        } else {
            let exec = meta.permissions().mode() & 0o111 != 0;
            let contents = hash(&std::fs::read(entry.path())?);
            let _ = writeln!(out, "f {name:?} {exec} {contents}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn tree_hash_sees_contents_names_modes_and_links() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("t");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("a"), "1").unwrap();
        std::fs::write(root.join("sub/b"), "2").unwrap();
        let first = hash_tree(&root).unwrap();
        assert_eq!(hash_tree(&root).unwrap(), first);

        let mut seen = vec![first];
        let changed = |seen: &mut Vec<String>| {
            let h = hash_tree(&root).unwrap();
            assert!(!seen.contains(&h), "the hash didn't change");
            seen.push(h);
        };
        std::fs::write(root.join("sub/b"), "3").unwrap();
        changed(&mut seen);
        std::fs::set_permissions(root.join("a"), std::fs::Permissions::from_mode(0o755)).unwrap();
        changed(&mut seen);
        std::os::unix::fs::symlink("a", root.join("link")).unwrap();
        changed(&mut seen);
        std::fs::rename(root.join("sub/b"), root.join("sub/c")).unwrap();
        changed(&mut seen);
        std::fs::create_dir(root.join("empty")).unwrap();
        changed(&mut seen);
        assert_eq!(hash_tree(&dir.path().join("missing")).unwrap(), hash(b""));
    }

    #[test]
    fn differences_name_the_parts() {
        let a = Fingerprint {
            ch: "1".into(),
            repo: "r".into(),
            ..Default::default()
        };
        let mut b = a.clone();
        assert!(a.differences(&b).is_empty());
        b.ch = "2".into();
        b.verify = "v".into();
        assert_eq!(a.differences(&b), ["ch binary or image", "verify/"]);
        // Each part on its own counts.
        type Change = fn(&mut Fingerprint);
        let parts: [(&str, Change); 6] = [
            ("ch binary or image", |f| f.ch.push('x')),
            ("base config", |f| f.config.push('x')),
            ("settings", |f| f.settings.push('x')),
            ("task.toml", |f| f.task.push('x')),
            ("repo/", |f| f.repo.push('x')),
            ("verify/", |f| f.verify.push('x')),
        ];
        for (name, change) in parts {
            let mut c = a.clone();
            change(&mut c);
            assert_eq!(a.differences(&c), [name]);
        }
    }
}
