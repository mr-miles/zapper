//! The workbench: a layer for single users on zapper (proposal 12, part 2).
//! A separate process, a client of zapper's API on its socket and so its
//! operator, which serves its own API and UI through a gateway that
//! authenticates and authorizes its clients, and hosts facilities behind
//! one interface.

pub mod facility;
pub mod gateway;
pub mod sse;
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

/// Where its data is: `~/.local/share/zapper-workbench`, sessions'
/// directories in `work/`.
pub fn data_dir() -> PathBuf {
    home(".local/share/zapper-workbench")
}
