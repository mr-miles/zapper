//! What clients register for the sessions a label selector picks (services,
//! deciders, MCP servers): numbered in the order registered, and removed by
//! that number.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;

use super::session::Session;

/// Labels a session must all have; an empty selector picks every session.
pub type Selector = BTreeMap<String, String>;

pub struct Entry<T> {
    pub sessions: Selector,
    pub item: T,
}

impl<T> Entry<T> {
    pub fn picks(&self, s: &Session) -> bool {
        self.sessions
            .iter()
            .all(|(k, v)| s.label(k) == Some(v.as_str()))
    }
}

pub struct Registry<T> {
    next: AtomicU64,
    entries: RwLock<BTreeMap<u64, Entry<T>>>,
}

impl<T> Default for Registry<T> {
    fn default() -> Self {
        Registry {
            next: AtomicU64::new(0),
            entries: RwLock::new(BTreeMap::new()),
        }
    }
}

impl<T> Registry<T> {
    /// Registers `item` for `sessions`, unless `clash` says an entry already
    /// there stands in its way; its id.
    pub fn add_unless(
        &self,
        sessions: Selector,
        item: T,
        clash: impl Fn(&Entry<T>) -> bool,
    ) -> Option<u64> {
        let mut entries = self.entries.write().unwrap();
        if entries.values().any(clash) {
            return None;
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        entries.insert(id, Entry { sessions, item });
        Some(id)
    }

    pub fn add(&self, sessions: Selector, item: T) -> u64 {
        self.add_unless(sessions, item, |_| false).unwrap()
    }

    pub fn remove(&self, id: u64) -> Option<Entry<T>> {
        self.entries.write().unwrap().remove(&id)
    }

    /// Reads the entries, in the order registered.
    pub fn with<R>(&self, f: impl FnOnce(&BTreeMap<u64, Entry<T>>) -> R) -> R {
        f(&self.entries.read().unwrap())
    }

    /// The earliest registered for `s` that `pred` takes.
    pub fn first(&self, s: &Session, pred: impl Fn(&T) -> bool) -> Option<T>
    where
        T: Clone,
    {
        self.with(|entries| {
            entries
                .values()
                .find(|e| e.picks(s) && pred(&e.item))
                .map(|e| e.item.clone())
        })
    }
}
