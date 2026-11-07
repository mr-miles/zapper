//! Sandboxes of an OpenShell gateway (v0.1.7) for zapper's layers: The Office
//! and the workbench. [`OpenShell`] makes a sandbox from a zapper profile,
//! drives commands in it over exec, and keeps its network policy in step
//! with zapper's rules (`rules`); it implements [`client::OfficeClient`].

pub mod client;
pub mod edge;
pub mod image;
pub mod rules;
mod sandboxes;

pub use sandboxes::*;

/// The MCP server of the office's tools, as the workers' harnesses know it.
pub const OFFICE_MCP: &str = "office";
/// Where the images put the guest's `office` command.
pub const AGENT_PATH: &str = "/usr/local/bin/zapper-agent";
