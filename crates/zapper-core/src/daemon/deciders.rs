//! Who decides a session's `ask`s. The operator does, unless a decider is
//! registered for the session (by its labels): then an `ask` is refused at
//! once, with the decider's word on where it went, and the decider is told
//! of it, to answer later through rules on the session. A decider only sees
//! what the rules leave as `ask`, so `final` rules stay above it.

use std::sync::Arc;

use anyhow::Result;
use zapper_policy::NetRequest;

use super::registry::Registry;
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

/// The deciders, by the sessions they decide for; the earliest registered
/// that picks a session decides, and none is the operator.
pub type Deciders = Registry<Arc<dyn Decider>>;

#[cfg(test)]
mod tests;
