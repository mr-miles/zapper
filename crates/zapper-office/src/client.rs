//! The office's facilities on a socket, and commands run over the client;
//! the client itself (`OfficeClient`) is zapper-openshell's.

use std::path::{Path, PathBuf};
use zapper_policy::proto::ShellRequest;

use anyhow::Result;

pub use zapper_openshell::client::*;

/// What a [`FacilityGuard`] hands back to the core when it drops.
#[derive(Clone, Copy)]
pub(crate) enum Registered {
    /// An MCP server (`remove_mcp_server`) — the office's tools.
    Mcp,
    /// A decider (`remove_decider`) — the officers.
    Decider,
    /// A service (`remove_service`) — the firm's git server on a socket.
    Service,
}

/// A facility the office serves on a socket, undone on drop (RAII): the
/// core registration removed, the serve task aborted, the socket file
/// unlinked. [`serve_facility`] makes one. Replaces the per-facility
/// `Served`/`DeciderServed` structs, which repeated the teardown by hand.
pub(crate) struct FacilityGuard {
    client: std::sync::Arc<dyn OfficeClient>,
    kind: Registered,
    id: u64,
    task: Option<tokio::task::AbortHandle>,
    path: PathBuf,
}

#[cfg(test)]
impl FacilityGuard {
    /// The socket file the facility is served on.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for FacilityGuard {
    fn drop(&mut self) {
        match self.kind {
            Registered::Mcp => {
                self.client.remove_mcp_server(self.id);
            }
            Registered::Decider => {
                self.client.remove_decider(self.id);
            }
            Registered::Service => {
                self.client.remove_service(self.id);
            }
        }
        if let Some(t) = self.task.take() {
            t.abort();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Serves a facility on `socket` in `socket_dir` and registers it through
/// `client`: binds the socket, spawns `serve` on it (if there is a runtime —
/// a test may bind without one), then registers it with `register`, which
/// returns the core id. On a registration error the task is aborted and the
/// socket unlinked. The [`FacilityGuard`] undoes it all on drop.
pub(crate) fn serve_facility<F, Fut>(
    client: &std::sync::Arc<dyn OfficeClient>,
    socket_dir: &Path,
    socket: &str,
    kind: Registered,
    serve: F,
    register: impl FnOnce(&str) -> Result<u64>,
) -> Result<FacilityGuard>
where
    F: FnOnce(std::os::unix::net::UnixListener) -> Fut,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let path = socket_dir.join(socket);
    let _ = std::fs::remove_file(&path);
    let listener = std::os::unix::net::UnixListener::bind(&path)?;
    listener.set_nonblocking(true)?;
    let task = tokio::runtime::Handle::try_current()
        .ok()
        .map(|h| h.spawn(serve(listener)).abort_handle());
    let id = match register(socket) {
        Ok(id) => id,
        Err(e) => {
            if let Some(t) = task {
                t.abort();
            }
            let _ = std::fs::remove_file(&path);
            return Err(e);
        }
    };
    Ok(FacilityGuard {
        client: client.clone(),
        kind,
        id,
        task,
        path,
    })
}

/// Runs one command in session `id` over the office's client and returns its
/// exit code, stdout (capped at `limit`) and stderr. Built on [`attach`]: the
/// office opens a terminal in the sandbox and drives the command over it (the
/// firm's cluster is a sandbox, not an agent), so this works in-process and,
/// in P3, over the socket, without a command-specific route. A shell script
/// may bundle many steps into one call.
pub(crate) async fn run_command(
    client: &dyn OfficeClient,
    id: &str,
    argv: Vec<String>,
    stdin: Option<Vec<u8>>,
    limit: u64,
) -> Result<(i32, Vec<u8>, String)> {
    let stream = client.attach(id, ShellRequest::system(argv)).await?;
    let input = stdin.map(std::io::Cursor::new);
    let mut out = Vec::new();
    let (code, err) = zapper_driver::process::pump_command(stream, input, &mut out, limit).await?;
    Ok((code, out, err))
}
