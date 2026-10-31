//! The workbench: a layer for single users on zapper (proposal 12, part 2).
//! A separate process, a client of zapper's API on its socket and so its
//! operator, which serves its own API and UI through a gateway that
//! authenticates and authorizes its clients, and hosts facilities behind
//! one interface.

pub mod facility;
pub mod gateway;
pub mod secrets;
pub mod sse;
pub mod ssh;
pub mod tokens;
pub mod zapper;

use std::path::PathBuf;

/// `~/rest`.
fn home(rest: &str) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
    PathBuf::from(home).join(rest)
}

/// Where its tokens are kept: `~/.config/zapper-workbench`.
pub fn config_dir() -> PathBuf {
    home(".config/zapper-workbench")
}

/// The SSH gateway's host key: zapper's own (`zapper ssh`), so one
/// `known_hosts` line serves both.
pub fn ssh_host_key() -> PathBuf {
    home(".config/zapper/ssh_host_ed25519")
}

/// The SSH connector's keys, written to this user's `authorized_keys`, the
/// forced command running `exe` for each.
pub fn ssh_keys(exe: &std::path::Path) -> anyhow::Result<ssh::Keys> {
    ssh::Keys::new(
        config_dir().join("ssh-keys.json"),
        home(".ssh/authorized_keys"),
        exe,
        &zapper::socket_path(),
    )
}

/// Where its data is: `~/.local/share/zapper-workbench`, sessions'
/// directories in `work/`.
pub fn data_dir() -> PathBuf {
    home(".local/share/zapper-workbench")
}
