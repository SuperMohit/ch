//! Typed shared state that plugins provide to each other (`FileState`, `PlanStore`, ...).

use std::any::{Any, TypeId, type_name};
use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Where `ch` keeps the user's own files (`$CH_HOME`, or `~/.ch`). The program that runs
/// the session provides it; plugins look for user-level files under it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChHome(pub PathBuf);

/// Directories outside the workspace that file tools may read but never write, such as
/// tool output a plugin saved for the model to read back. A plugin adds the directories
/// it owns; paths must be canonical.
#[derive(Debug, Default)]
pub struct ReadOnlyRoots {
    roots: Mutex<Vec<PathBuf>>,
}

impl ReadOnlyRoots {
    /// Adds `dir`, which must be canonical. Adding it again does nothing.
    pub fn add(&self, dir: PathBuf) {
        let mut roots = self.lock();
        if !roots.contains(&dir) {
            roots.push(dir);
        }
    }

    /// Whether the canonical `path` is inside one of the directories.
    pub fn contains(&self, path: &Path) -> bool {
        self.lock().iter().any(|root| path.starts_with(root))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<PathBuf>> {
        // Every update is a single push, so a poisoned lock is still consistent.
        self.roots.lock().unwrap_or_else(|p| p.into_inner())
    }
}

#[derive(Default, Clone)]
pub struct Services {
    map: HashMap<TypeId, (&'static str, Arc<dyn Any + Send + Sync>)>,
}

impl Services {
    /// Registers a service. Returns `false` (and keeps the old one) if a service of the
    /// same type already exists.
    pub fn provide<T: Any + Send + Sync>(&mut self, service: Arc<T>) -> bool {
        let id = TypeId::of::<T>();
        if self.map.contains_key(&id) {
            return false;
        }
        self.map.insert(id, (type_name::<T>(), service));
        true
    }

    pub fn get<T: Any + Send + Sync>(&self) -> Option<Arc<T>> {
        self.map
            .get(&TypeId::of::<T>())
            .and_then(|(_, s)| s.clone().downcast::<T>().ok())
    }

    pub fn contains<T: Any + Send + Sync>(&self) -> bool {
        self.map.contains_key(&TypeId::of::<T>())
    }

    pub fn names(&self) -> Vec<&'static str> {
        let mut v: Vec<_> = self.map.values().map(|(n, _)| *n).collect();
        v.sort_unstable();
        v
    }
}

impl fmt::Debug for Services {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.names()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Counter(u32);

    #[test]
    fn read_only_roots_match_paths_inside_them() {
        let roots = ReadOnlyRoots::default();
        assert!(!roots.contains(Path::new("/tmp/spill/a.txt")));
        roots.add(PathBuf::from("/tmp/spill"));
        roots.add(PathBuf::from("/tmp/spill"));
        assert_eq!(roots.lock().len(), 1);
        assert!(roots.contains(Path::new("/tmp/spill/a.txt")));
        assert!(roots.contains(Path::new("/tmp/spill")));
        assert!(!roots.contains(Path::new("/tmp/spilled/a.txt")));
        assert!(!roots.contains(Path::new("/tmp")));
    }

    #[test]
    fn provide_and_get() {
        let mut s = Services::default();
        assert!(s.provide(Arc::new(Counter(1))));
        assert!(!s.provide(Arc::new(Counter(2))));
        assert_eq!(s.get::<Counter>().unwrap().0, 1);
        assert!(s.get::<String>().is_none());
    }
}
