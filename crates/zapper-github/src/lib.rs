//! GitHub App support for zapper: the App's credentials, its installation on
//! an account, and short-lived tokens limited to one repository and a set of
//! permissions.
//!
//! The private key is read only here. A token is minted by `zapper mint`
//! (or [`GitHub::mint`]) and held by the daemon in memory; it never reaches a
//! guest.

mod app;
mod github;
mod http;
pub mod setup;

pub use app::{apps_dir, owner_file, valid_login, App};
pub use github::{
    check_permissions, expires_unix, split_repo, GitHub, Installation, Scope, Token, API_ENV,
};
