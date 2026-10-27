//! zapper's core: the daemon (sessions in Firecracker microVMs, the host
//! proxy and MCP gateway, DNS, policy and approvals, the driver, events and
//! the record), the VMM shim and the guest's network namespace. The
//! `zapper` binary is the CLI over it.

mod daemon;
#[cfg(feature = "fuzz")]
pub mod fuzz;
mod interface;
pub(crate) mod netns;
pub mod subids;
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
    ApprovalApi, ConfigApi, Core, Decider, Event, EventApi, EventBody, McpServer, NewSession,
    Pending, Referral, Remember, RuleApi, RuleKeeper, RuleRemoval, ServiceApi, ServiceHandler,
    ServiceInfo, ServiceIo, ServiceSpec, ServiceTarget, Session, SessionApi, SessionInfo, Status,
    TermChunk, TerminalInfo,
};

/// The `Core` API traits, so a consumer can `use zapper_core::prelude::*;`
/// and call any of them on a `Core` (or depend on one narrow trait).
pub mod prelude {
    pub use crate::interface::{ApprovalApi, ConfigApi, EventApi, RuleApi, ServiceApi, SessionApi};
}
