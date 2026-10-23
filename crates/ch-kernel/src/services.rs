//! Typed shared state that plugins provide to each other (`FileState`, `PlanStore`, ...).

use std::any::{Any, TypeId, type_name};
use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

/// Where `ch` keeps the user's own files (`$CH_HOME`, or `~/.ch`). The program that runs
/// the session provides it; plugins look for user-level files under it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChHome(pub PathBuf);

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
    fn provide_and_get() {
        let mut s = Services::default();
        assert!(s.provide(Arc::new(Counter(1))));
        assert!(!s.provide(Arc::new(Counter(2))));
        assert_eq!(s.get::<Counter>().unwrap().0, 1);
        assert!(s.get::<String>().is_none());
    }
}
