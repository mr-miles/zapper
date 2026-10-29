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
mod pulls;
pub mod setup;

pub use app::{app_file, apps_dir, owner_file, valid_login, App, API_URL, WEB_URL};
pub use github::{
    check_permissions, expires_unix, split_repo, GitHub, Installation, Scope, Token, MAX_REPOS,
};
pub use pulls::{git_auth_header, Check, Pull, Remark, MAX_LOG};
