//! Configuration model and rule evaluation for zapper.
//!
//! A [`Config`] is the whole serialisable state of the daemon (the "running
//! config"). Policy is layered: rules in a session override its profile, a
//! profile overrides the profile it `extends`, and every profile overrides the
//! global policy. Within a layer rules are an ordered list and the first match
//! wins, like an ACL. Rules marked `final` are checked before anything else,
//! from the global layer down, so a global `final` deny cannot be overridden.

mod config;
mod eval;
pub mod mint;
pub mod names;
/// The host–guest protocol, from `zapper-proto`.
pub use zapper_proto::{facility, proto};
pub use zapper_proto::{ExecRequest, FsAccess, FsRule};
mod template;

pub use config::*;
pub use eval::*;
pub use names::valid_worker_name;
pub use template::*;
