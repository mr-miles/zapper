//! The daemon's REST API contract: the request and response types the API
//! (`zapper-api`), the core that implements it (`zapper-core`) and the API's
//! clients (`zapper-office`, `zapper-workbench`) all share.
//!
//! This crate is at the bottom: it depends only on `zapper-policy` and
//! `zapper-proto` (the config model and the guest protocol), never on the
//! core. So a client of the API — The Office as its own process — speaks the
//! API's types without linking the daemon (proposal 13, step 4; P4-c). The
//! core re-exports these so its internal callers are unchanged.

use std::collections::BTreeMap;

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

/// A session as the API reports it: the response of the session routes and
/// what the UI and The Office read. Built by the core (`Session::info`) and
/// deserialised by the API's clients, so its fields are `pub` and its shape
/// is the wire contract.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub profile: String,
    #[serde(flatten)]
    pub status: Status,
    pub created: f64,
    #[serde(default)]
    pub expires: Option<f64>,
    pub persistent: bool,
    /// Whether the SSH gateway may open terminals in it.
    pub ssh: bool,
    /// For a persistent session: what its disks take on the host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_bytes: Option<u64>,
    pub image: String,
    pub vcpus: u8,
    pub memory_mib: u32,
    pub kernel: String,
    /// `nic` or `vsock`.
    pub network: zapper_policy::NetworkMode,
    pub user: String,
    pub root: bool,
    pub workdir: String,
    pub counters: serde_json::Value,
    pub usage: Option<zapper_policy::proto::GuestEvent>,
    pub rules: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub firm: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    /// The driver's state for a driven session; `"disabled"` with the
    /// driver off (`daemon.driver: false`).
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub driver: serde_json::Value,
}

impl SessionInfo {
    /// The session's id.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The value of label `key`, if the session has it. The office reads a
    /// firm session's `firm`/`run`/`member` labels this way, over the client,
    /// in place of reaching into the `Session`.
    pub fn label(&self, key: &str) -> Option<&str> {
        self.labels.get(key).map(String::as_str)
    }

    /// The profile the session runs.
    pub fn profile(&self) -> &str {
        &self.profile
    }

    /// The workload user the agent runs as (its home is `/home/<user>`).
    pub fn user(&self) -> &str {
        &self.user
    }
}
