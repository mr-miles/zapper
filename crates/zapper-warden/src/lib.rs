//! The warden: zapperclaw's host side ([proposal 09](../../../docs/proposals/09-zapperclaw.md)).
//!
//! The office (the post office, its foremen and drive) runs in an OpenShell
//! sandbox of its own, with no model credential and no network but the
//! warden. The warden is the only thing that holds the gateway's
//! credentials, and it does as little as it can with them:
//!
//! - **Templates** (`zapper-warden apply`): the office's (`zc-office`) and
//!   one per worker profile (`zcp-<profile>`), from the zapper config, with
//!   the office's tools, Stop hook and edge wired in as the office wired
//!   them per worker.
//! - **The office sandbox** (`zc-office`), made from its template when the
//!   warden starts, if it isn't there.
//! - **Five routes**, for the office sandbox only (its supervisor's token,
//!   from the warden's own edge): `GET /workers`, `PUT` and `DELETE
//!   /workers/{name}`, `POST /workers/{name}/agent` and `GET /jwks`.
//! - **Two forwards** into the office sandbox, bytes only: the workers'
//!   edge (where their supervisors send the office's tools, drive, Stop hook
//!   and token grant) and the board (the person's dashboard and CLI).
//!
//! The office chooses a worker's name and profile, from the warden's list,
//! and when to start its agent; never an image, policy, provider, command
//! line or file.

pub mod apply;
pub mod audit;
pub mod config;
pub mod forward;
pub mod gateway;
pub mod names;
pub mod profiles;
pub mod warden;

#[cfg(test)]
mod fake;
#[cfg(test)]
mod warden_tests;

use std::path::PathBuf;

/// `~/rest`.
fn home(rest: &str) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
    PathBuf::from(home).join(rest)
}

/// Where its config is by default: `~/.config/zapperclaw/warden.yaml`.
pub fn default_config() -> PathBuf {
    home(".config/zapperclaw/warden.yaml")
}
