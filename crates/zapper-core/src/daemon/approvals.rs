//! Outstanding operator approvals: a session asks, the operator or a decider
//! answers (the `ask`/`answer` orchestration is on Daemon). This holds the
//! pending ones; a session can only have so many at once, so it can't flood
//! the operator.

use std::collections::HashMap;
use std::sync::Mutex;

use super::Pending;

/// How many approvals one session may have outstanding at once.
const MAX_PER_SESSION: usize = 32;

/// The outstanding approvals, by id.
pub(crate) struct Approvals(Mutex<HashMap<String, Pending>>);

impl Approvals {
    pub(crate) fn new() -> Approvals {
        Approvals(Mutex::new(HashMap::new()))
    }

    /// Register a pending approval, unless its session already has the most it
    /// may have outstanding. Returns whether it was registered.
    pub(crate) fn register(&self, pending: Pending) -> bool {
        let mut approvals = self.0.lock().unwrap();
        if approvals
            .values()
            .filter(|p| p.session == pending.session)
            .count()
            >= MAX_PER_SESSION
        {
            return false;
        }
        approvals.insert(pending.id.clone(), pending);
        true
    }

    /// Remove an approval by id, if it is still outstanding.
    pub(crate) fn take(&self, id: &str) -> Option<Pending> {
        self.0.lock().unwrap().remove(id)
    }

    /// The outstanding approvals, oldest first.
    pub(crate) fn list(&self) -> Vec<Pending> {
        let mut v: Vec<_> = self.0.lock().unwrap().values().cloned().collect();
        v.sort_by(|a, b| a.created.total_cmp(&b.created));
        v
    }
}
