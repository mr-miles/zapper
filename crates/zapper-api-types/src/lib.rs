//! The daemon's REST API contract: the request and response types the API
//! (`zapper-api`), the core that implements it (`zapper-core`) and the API's
//! clients (`zapper-office`, `zapper-workbench`) all share.
//!
//! This crate is at the bottom: it depends only on `zapper-policy` and
//! `zapper-proto` (the config model and the guest protocol), never on the
//! core. So a client of the API — The Office as its own process — speaks the
//! API's types without linking the daemon (proposal 13, step 4; P4-c). The
//! core re-exports these so its internal callers are unchanged.

use serde::{Deserialize, Serialize};

/// A session's lifecycle status, flattened into [`SessionInfo`] as
/// `{state, detail}`. The live state machine that drives it lives in the
/// core (`GuestRuntime`); this is only its serialised shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", content = "detail", rename_all = "lowercase")]
pub enum Status {
    Starting,
    Running,
    /// Its stop is under way: the VM and disks are being torn down. Reached
    /// once, by the caller that owns the stop; it becomes `Stopped` only when
    /// teardown has finished everything.
    Stopping(String),
    Failed(String),
    Stopped(String),
}

impl Status {
    /// Starting or running: its VM is being made, or runs.
    pub fn is_live(&self) -> bool {
        matches!(self, Status::Starting | Status::Running)
    }
}
