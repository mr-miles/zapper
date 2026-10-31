//! zapper's core: the daemon (sessions in Firecracker microVMs, the host
//! proxy and MCP gateway, DNS, policy and approvals, the driver, events and
//! the record), the VMM shim and the guest's network namespace. The
//! `zapper` binary is the CLI over it.

mod daemon;
pub mod errors;
#[cfg(feature = "fuzz")]
pub mod fuzz;
pub mod images;
mod interface;
pub mod mint;
pub(crate) mod netns;
pub(crate) mod subids;
#[cfg(any(test, feature = "test-util"))]
pub mod testing;
pub mod util;
pub(crate) mod vmm;

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
    ApprovalApi, BuildInfo, BuildMessage, BuildRequest, Catchup, ConfigApi, Core, Cursor, Decider,
    Event, EventApi, EventBody, Follow, ImageApi, ImageBuild, McpServer, NewSession, Outcome,
    Pending, Referral, Remember, RuleApi, RuleKeeper, RuleRemoval, ServiceApi, ServiceHandler,
    ServiceInfo, ServiceIo, ServiceSpec, ServiceTarget, Session, SessionApi, SessionInfo, Status,
    TermChunk, TerminalInfo,
};
/// The subordinate-id range a session's Firecracker can take, for `check`
/// (see [`subids::host_range`]).
pub use subids::host_range as subids_host_range;
/// The `zapper __vmm` re-exec, for the binary's `main` (see [`vmm::shim`]).
pub use vmm::shim as vmm_shim;

/// The `Core` API traits, so a consumer can `use zapper_core::prelude::*;`
/// and call any of them on a `Core` (or depend on one narrow trait).
pub mod prelude {
    pub use crate::interface::{
        ApprovalApi, ConfigApi, EventApi, ImageApi, RuleApi, ServiceApi, SessionApi,
    };
}
