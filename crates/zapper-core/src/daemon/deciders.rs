//! Who decides a session's `ask`s. The operator does, unless a decider is
//! registered for the session (by its labels): then an `ask` is refused at
//! once, with the decider's word on where it went, and the decider is told
//! of it, to answer later through rules on the session. A decider only sees
//! what the rules leave as `ask`, so `final` rules stay above it.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use anyhow::Result;
use zapper_policy::NetRequest;

use super::session::Session;

/// Where an `ask` went: the session is told "access to WHAT requested from
/// `to` (request `id`); `then`", and its request log "access requested
/// from `to` (`id`)".
pub struct Referral {
    pub id: String,
    /// Who decides it, e.g. `security`.
    pub to: String,
    /// What the session should do meanwhile.
    pub then: String,
}

/// Decides the `ask`s of the sessions it is registered for.
pub trait Decider: Send + Sync {
    /// A request a rule asks about, refused at once: where it went. `Err`
    /// refuses it as a denial.
    fn ask(&self, s: &Session, req: &NetRequest) -> Result<Referral>;
    /// A request allowed by `rule`, for the decider's records (an approval
    /// for once is used up).
    fn allowed(&self, _s: &Session, _rule: &str) {}
}

struct Entry {
    sessions: BTreeMap<String, String>,
    decider: Arc<dyn Decider>,
}

#[derive(Default)]
pub struct Deciders {
    next: AtomicU64,
    entries: RwLock<BTreeMap<u64, Entry>>,
}

impl Deciders {
    pub fn add(&self, sessions: BTreeMap<String, String>, decider: Arc<dyn Decider>) -> u64 {
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        self.entries
            .write()
            .unwrap()
            .insert(id, Entry { sessions, decider });
        id
    }

    pub fn remove(&self, id: u64) -> bool {
        self.entries.write().unwrap().remove(&id).is_some()
    }

    /// The decider of `s`'s asks, the earliest registered that picks it;
    /// none for the operator.
    pub fn of(&self, s: &Session) -> Option<Arc<dyn Decider>> {
        self.entries
            .read()
            .unwrap()
            .values()
            .find(|e| {
                e.sessions
                    .iter()
                    .all(|(k, v)| s.label(k) == Some(v.as_str()))
            })
            .map(|e| e.decider.clone())
    }
}

#[cfg(test)]
mod tests;
