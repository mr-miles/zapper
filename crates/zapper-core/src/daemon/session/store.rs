//! The daemon's live sessions.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use anyhow::{bail, Result};

use super::{Session, Status};

/// The daemon's live sessions, by id. Behind one lock; readers take a cloned
/// snapshot so the lock is never held across other work.
pub(crate) struct SessionStore(pub(super) RwLock<BTreeMap<String, Arc<Session>>>);

impl SessionStore {
    pub(crate) fn new() -> SessionStore {
        SessionStore(RwLock::new(BTreeMap::new()))
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
        self.all().into_iter().filter(|s| {
            matches!(
                *s.runtime.status().borrow(),
                Status::Starting | Status::Running
            )
        })
    }

    /// Take a stopped session off the list; a starting or running one stays,
    /// as nothing would manage its VM.
    /// Takes a stopped session off the list; a persistent one's disks go
    /// with it.
    pub(crate) fn forget(&self, id: &str) -> Result<()> {
        let mut sessions = self.0.write().unwrap();
        if let Some(s) = sessions.get(id) {
            if matches!(
                *s.runtime.status().borrow(),
                Status::Starting | Status::Running
            ) {
                bail!("session {id} is still running; stop it first");
            }
        }
        if let Some(s) = sessions.remove(id) {
            if s.persistent {
                // What it kept to be started again, or adopted: gone, so no
                // daemon lists it again. Its record stays.
                let _ = std::fs::remove_dir_all(s.dir.vm());
                for kept in [
                    s.dir.saved(),
                    s.dir.ca(),
                    s.dir.dns(),
                    s.dir.resume_token(),
                    s.dir.vmm(),
                    s.dir.conversation(),
                ] {
                    let _ = std::fs::remove_file(kept);
                }
            }
        }
        Ok(())
    }
}
