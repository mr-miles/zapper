//! What the host and the guest agent say to each other: the vsock protocol
//! (`proto`), and the two kinds of policy item that cross it, an exec the
//! guest asks the host to decide (`ExecRequest`) and the filesystem rules
//! the guest applies at boot (`FsRule`). The agent depends on this alone,
//! not on the config model or the rules' evaluation, which are the host's.

use serde::{Deserialize, Serialize};

pub mod proto;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ExecRequest {
    /// Resolved executable path.
    pub path: String,
    #[serde(default)]
    pub argv: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

/// Filesystem rule, applied inside the sandbox when the session starts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FsRule {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Guest path glob, e.g. `/work/**/.env`.
    pub path: String,
    pub access: FsAccess,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FsAccess {
    /// Covered by an empty file or directory.
    Hidden,
    /// Remounted read-only.
    Ro,
}
