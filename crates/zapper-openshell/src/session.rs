//! A session as the office sees it: what it asks for one, and what it
//! reports of one (the VMs view's rows, the run's records).

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use zapper_policy::proto::GuestFile;
use zapper_policy::{Mount, Policy};

/// A session's lifecycle status, flattened into [`SessionInfo`] as
/// `{state, detail}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", content = "detail", rename_all = "lowercase")]
pub enum Status {
    Starting,
    Running,
    /// Its stop is under way.
    Stopping(String),
    Failed(String),
    Stopped(String),
}

impl Status {
    /// Starting or running: its sandbox is being made, or runs.
    pub fn is_live(&self) -> bool {
        matches!(self, Status::Starting | Status::Running)
    }
}

/// A session as the office reports it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub profile: String,
    #[serde(flatten)]
    pub status: Status,
    pub created: f64,
    pub image: String,
    pub vcpus: u8,
    pub memory_mib: u32,
    pub user: String,
    pub workdir: String,
    /// The rules of its own layer.
    pub rules: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub firm: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
}

impl SessionInfo {
    /// The session's id.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The value of label `key`, if the session has it.
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

/// What the office asks of a new session: the profile, the host directory
/// that is its work directory, its own rule layer and labels, and files,
/// directories and agent settings besides the profile's.
pub struct NewSession {
    pub profile: String,
    pub workdir: PathBuf,
    pub policy: Policy,
    pub labels: BTreeMap<String, String>,
    /// Files put in the sandbox besides the profile's.
    pub files: Vec<GuestFile>,
    /// Directories copied in besides the profile's.
    pub mounts: Vec<Mount>,
    /// Merged over the profile's `agent`.
    pub agent_overlay: Option<serde_json::Value>,
}

impl NewSession {
    pub fn plain(profile: String, workdir: PathBuf, policy: Policy) -> Self {
        NewSession {
            profile,
            workdir,
            policy,
            labels: BTreeMap::new(),
            files: vec![],
            mounts: vec![],
            agent_overlay: None,
        }
    }
}
