//! zapper's core: the daemon (sessions in Firecracker microVMs, the host
//! proxy and MCP gateway, DNS, policy and approvals, the driver, events and
//! the record), the VMM shim and the guest's network namespace. The
//! `zapper` binary is the CLI over it.

mod daemon;
mod interface;
pub mod netns;
#[cfg(any(test, feature = "test-util"))]
pub mod testing;
pub mod util;
pub mod vmm;

pub use daemon::api_error::ApiError;
/// The driver: an agent's process over a terminal channel, and the queue
/// of tasks a driven session works through.
pub use daemon::driver;
/// Agent harnesses: how each agent is set up, and talked to headless.
pub use daemon::harness;
/// The record: append-only files kept by the daemon, or by
/// `zapper-keeper` as a separate user.
pub use daemon::record;
pub use interface::{
    Core, Decider, Event, McpServer, NewSession, Pending, Referral, Remember, RuleKeeper,
    RuleRemoval, ServiceHandler, ServiceInfo, ServiceIo, ServiceSpec, ServiceTarget, Session,
    SessionInfo, Status, TermChunk, TerminalInfo,
};
