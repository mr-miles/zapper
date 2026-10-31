//! The daemon's live sessions.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, RwLock};

use anyhow::{bail, Result};

use super::Session;

/// The daemon's live sessions, by id. Behind one lock; readers take a cloned
/// snapshot so the lock is never held across other work. The second lock is
/// held by an admission (`create::admit`) until the session it admits counts
/// as starting, so two can't both pass the limits.
pub(crate) struct SessionStore(pub(super) RwLock<BTreeMap<String, Arc<Session>>>, Mutex<()>);

impl SessionStore {
    pub(crate) fn new() -> SessionStore {
        SessionStore(RwLock::new(BTreeMap::new()), Mutex::new(()))
    }

    /// Held while a session is admitted and until it counts as starting.
    pub(super) fn admitting(&self) -> MutexGuard<'_, ()> {
        self.1.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn get(&self, id: &str) -> Option<Arc<Session>> {
        self.0.read().unwrap().get(id).cloned()
    }

    pub(crate) fn all(&self) -> Vec<Arc<Session>> {
        self.0.read().unwrap().values().cloned().collect()
    }

    pub(crate) fn insert(&self, session: Arc<Session>) {
        self.0.write().unwrap().insert(session.id.clone(), session);
    }

    /// How many sessions are starting or running, for the session limit.
    pub(crate) fn running(&self) -> usize {
        self.live().count()
    }

    /// The memory given to the VMs starting or running, in MiB.
    pub(crate) fn memory_mib(&self) -> u64 {
        self.live()
            .map(|s| s.resolved.engine.memory_mib as u64)
            .sum()
    }

    fn live(&self) -> impl Iterator<Item = Arc<Session>> {
        self.all()
            .into_iter()
            .filter(|s| s.runtime.current().is_live())
    }

    /// Take a stopped session off the list; a starting or running one stays,
    /// as nothing would manage its VM.
    /// Takes a stopped session off the list; a persistent one's disks go
    /// with it.
    pub(crate) fn forget(&self, id: &str) -> Result<()> {
        let mut sessions = self.0.write().unwrap();
        if let Some(s) = sessions.get(id) {
            if s.runtime.current().is_live() {
                bail!("session {id} is still running; stop it first");
            }
        }
        if let Some(s) = sessions.remove(id) {
            if s.persistent {
                super::persist::forget(&s.dir);
            }
        }
        Ok(())
    }
}
