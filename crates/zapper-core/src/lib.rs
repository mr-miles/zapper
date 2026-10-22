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
/// The office's workers' skills, for `zapper init` and `zapper check`.
#[cfg(feature = "office")]
pub use daemon::office::skills;
pub use interface::{
    Core, Event, NewSession, Pending, Remember, RuleRemoval, Session, SessionInfo, TermChunk,
    TerminalInfo,
};
