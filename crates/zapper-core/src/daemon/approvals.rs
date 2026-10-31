//! Operator approvals: the pending-decision model and the outstanding ones.
//! A session asks, the operator or a decider answers (the `ask`/`answer`
//! orchestration is on Daemon). A session can only have so many outstanding at
//! once, so it can't flood the operator.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;
use zapper_policy::{AnyRule, Kind};

/// A pending `ask` decision.
#[derive(Debug, Clone, Serialize)]
pub struct Pending {
    pub id: String,
    pub session: String,
    pub kind: Kind,
    pub subject: String,
    pub created: f64,
    /// Rule added if the answer is remembered.
    pub suggestion: AnyRule,
    /// Fired with the answer; set by `ask`, taken by `answer` (both on Daemon).
    #[serde(skip)]
    pub(super) tx: Arc<Mutex<Option<oneshot::Sender<bool>>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
/// What an answer to an `ask` is remembered for. Answers are kept in memory:
/// for the request alone (no `remember`) or in the session's own layer, which
/// ends with the session. They never change the config.
pub enum Remember {
    Session,
}

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

    /// Removes every approval of `session` (it stopped), refusing each to
    /// whoever waits on it; the ones removed.
    pub(crate) fn take_session(&self, session: &str) -> Vec<Pending> {
        let mut approvals = self.0.lock().unwrap();
        let ids: Vec<String> = approvals
            .values()
            .filter(|p| p.session == session)
            .map(|p| p.id.clone())
            .collect();
        ids.iter()
            .filter_map(|id| approvals.remove(id))
            .inspect(|p| {
                if let Some(tx) = p.tx.lock().unwrap().take() {
                    let _ = tx.send(false);
                }
            })
            .collect()
    }

    /// The outstanding approvals, oldest first.
    pub(crate) fn list(&self) -> Vec<Pending> {
        let mut v: Vec<_> = self.0.lock().unwrap().values().cloned().collect();
        v.sort_by(|a, b| a.created.total_cmp(&b.created));
        v
    }
}
