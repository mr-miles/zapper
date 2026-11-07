//! The workbench: what one person's OpenShell sandboxes share that OpenShell
//! doesn't keep, served to them through an edge of its own. A drive (files
//! the user puts in and the sandboxes write out), a repository (where an
//! agent proposes a change on a branch, which the user merges), and the
//! repository applied to the gateway as sandbox templates. Everything else
//! (making, driving and reaching sandboxes, their policy and credentials,
//! SSH) is OpenShell's and its UI's.

pub mod apply;
pub mod backend;
pub mod drive;
pub mod render;
pub mod repo;
pub mod settings;

use std::path::PathBuf;

/// `~/rest`.
fn home(rest: &str) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
    PathBuf::from(home).join(rest)
}

/// Where its config is: `~/.config/zapper-workbench`.
pub fn config_dir() -> PathBuf {
    home(".config/zapper-workbench")
}

/// Where its data is (the drive, the repository, applied checkouts):
/// `~/.local/share/zapper-workbench`.
pub fn data_dir() -> PathBuf {
    home(".local/share/zapper-workbench")
}
