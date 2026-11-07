//! What the office's firm logic needs from where its workers run, behind the
//! `OfficeClient` trait: [`crate::openshell::OpenShell`] in production, an
//! in-memory fake in the tests (`crate::fake`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use futures_util::future::BoxFuture;
use serde_json::Value;
use zapper_api_types::{NewSession, ServiceSpec, SessionInfo};
use zapper_driver::process::DriveStream;
use zapper_policy::proto::ShellRequest;
use zapper_policy::{AnyRule, NetRule, Scope};

/// What the office's firm logic needs from where its workers run. The
/// firm-logic calls (`security::decide`, revocation) are synchronous; the
/// session surface is `async`, driven from the foreman's async loop. The
/// async methods return a [`BoxFuture`] (not RPIT) so the trait stays
/// object-safe: it is held as `Arc<dyn OfficeClient>`.
pub trait OfficeClient: Send + Sync {
    /// Adds a rule, first, to a firm role's shared layer `name`; the rule
    /// (with the id the caller gave it).
    fn add_layer_rule(&self, name: &str, rule: AnyRule) -> AnyRule;
    /// Removes rule `id` from role layer `name`; whether it was there.
    fn remove_layer_rule(&self, name: &str, id: &str) -> bool;
    /// Empties a firm role's shared layer `name`, at the start of a run (the
    /// layer registry outlives a run, which starts with no approvals).
    fn reset_layer(&self, name: &str);
    /// Whether the record can be written now (the office takes no firm turn
    /// while it can't).
    fn record_writable(&self) -> bool;
    /// The daemon's running config — the firms' definitions and profiles the
    /// office validates a start against, and the UI reads.
    fn config(&self) -> zapper_policy::Config;
    /// How many sessions are running now (the firm-level pre-flight: enough
    /// room under `daemon.max_sessions` for the firm's workers).
    fn session_count(&self) -> usize;
    /// Whether the host has room for a session of `profile`, and the running
    /// config if so (the host-room decision in one call: limit + memory).
    fn admit(&self, profile: &str) -> anyhow::Result<zapper_policy::Config>;
    /// Promotes a standing approval's `rule` into the running config at
    /// `scope`, under change note `what`, giving it a fresh `net-N` id; the
    /// rule as added. Config mutation stays the operator's: `Local` changes
    /// the daemon's config in process; `Remote` reaches the daemon's config
    /// API (filled in with the standalone bin, P4-d).
    fn promote_rule(&self, what: &str, scope: &Scope, rule: NetRule) -> Result<AnyRule>;

    // --- session surface (P1-b) --------------------------------------------
    // The office creates, drives, stops and forgets a worker's (or facility's)
    // session through these, storing the id it gets back rather than an
    // `Arc<Session>` (P1-c). `Local` calls `Core` directly; `Remote` is
    // filled in over the API socket in P3-a.

    /// Starts a session; its info once the guest is ready (the office keeps
    /// the `id`).
    fn create_session<'a>(&'a self, new: NewSession) -> BoxFuture<'a, Result<SessionInfo>>;
    /// Stops session `id` and takes it off the list (the office always
    /// discards a worker's or the cluster's session when done). Errors if
    /// there is no such session.
    fn stop_session<'a>(&'a self, id: &'a str, reason: &'a str) -> BoxFuture<'a, Result<()>>;
    /// Session `id`'s info; errors if there is no such session.
    fn session(&self, id: &str) -> Result<SessionInfo>;
    /// Opens a terminal in session `id` and returns the stream to drive an
    /// agent over it ([`zapper_driver::process::start`]). Local in-process; the
    /// `Remote` impl upgrades the API connection to a raw stream (P3-a).
    fn attach<'a>(
        &'a self,
        id: &'a str,
        req: ShellRequest,
    ) -> BoxFuture<'a, Result<Box<dyn DriveStream>>>;
    /// Opens a raw byte stream to port `port` of session `id`'s guest (over
    /// vsock), for the office to proxy — the board's dashboard/product port.
    /// `Remote` upgrades the API connection and the daemon splices it.
    fn connect_port<'a>(
        &'a self,
        id: &'a str,
        port: u32,
    ) -> BoxFuture<'a, Result<Box<dyn DriveStream>>>;

    // --- facility registration (P2-a) --------------------------------------
    // The office serves a facility (its tools, the officers' decider) on a
    // socket in the daemon's `state/services/` directory (`run.socket_dir`,
    // P1-a) and registers it here, so the gateway/core routes the run's
    // sessions to it. `serve_facility` folds the bind + serve-loop + register
    // + cleanup around these. Local calls `Core`; Remote is P3-a.

    /// Serves the MCP server a facility serves on `socket` as `name` to the
    /// sessions `sessions` picks; its id.
    fn add_mcp_socket(
        &self,
        name: &str,
        sessions: BTreeMap<String, String>,
        socket: &str,
        instructions: Option<String>,
        owner: &str,
    ) -> Result<u64>;
    /// Stops serving a facility's MCP server.
    fn remove_mcp_server(&self, id: u64) -> bool;
    /// Makes the decider a facility serves on `socket` decide the `ask`s of
    /// the sessions `sessions` picks; its id.
    fn add_decider_socket(&self, sessions: BTreeMap<String, String>, socket: &str) -> Result<u64>;
    /// Hands a decider's sessions' `ask`s back to the operator.
    fn remove_decider(&self, id: u64) -> bool;

    /// Registers a service of the firm (its git server, its cluster's ports);
    /// its id.
    fn add_service(&self, spec: ServiceSpec) -> Result<u64>;
    /// Makes `domain` a zone for the sessions `sessions` picks: its names
    /// resolve only to the firm's services, never on the host; its id.
    fn add_zone(
        &self,
        domain: &str,
        sessions: BTreeMap<String, String>,
        owner: &str,
    ) -> Result<u64>;
    /// Removes a service or zone (the office only needs it gone; it ignores
    /// what it was).
    fn remove_service(&self, id: u64);

    // --- a session's own rule layer and traffic (P4) -----------------------
    // The officers decide access by putting rules on a worker's (or the
    // cluster's) own session layer, and read its traffic; by session id, so
    // the office needs no `Session`.

    /// Session `id`'s captured requests (the officers' traffic view).
    fn requests(&self, id: &str) -> Vec<Value>;
    /// Adds `rule` to session `id`'s own layer (the rule carries its id).
    fn put_session_rule(&self, id: &str, rule: AnyRule);
    /// Removes rule `rule_id` from session `id`'s own layer; whether it went.
    fn remove_session_rule(&self, id: &str, rule_id: &str) -> bool;
    /// Where the firm's workers reach the office over HTTP, as a URL they
    /// can use (the office edge on OpenShell: its MCP tools at `/mcp`, git
    /// at `/git/`), with their identity added outside the VM; `None` where
    /// the daemon carries them (its MCP gateway, `git.<firm domain>`).
    fn edge(&self) -> Option<String> {
        None
    }

    // --- drafted rules (OpenShell's policy advisor) -------------------------
    // Where the sandboxes' network drafts rules for review (a worker's
    // proposal through `policy.local`, or a connection it refused), the
    // officers review them in place of the daemon's `ask` referrals.

    /// Whether access requests come as drafted rules.
    fn drafts(&self) -> bool {
        false
    }
    /// The rules session `id`'s network drafted that wait for review, as last
    /// read from the gateway; `None` before they were first read.
    fn pending_drafts(&self, _id: &str) -> Option<Vec<DraftRule>> {
        None
    }
    /// Approves (narrowed to `methods` and `path` if given) or rejects draft
    /// `chunk` of session `id`. An approval returns the id of the rule it
    /// became, which [`OfficeClient::remove_session_rule`] takes back out.
    fn decide_draft(&self, _id: &str, _chunk: &str, _d: DraftDecision) -> Result<Option<String>> {
        anyhow::bail!("this office's sessions draft no rules")
    }
}

/// A rule a session's network drafted for review: a worker's proposal (its
/// `intent_summary` the rationale), or OpenShell's own from a connection it
/// refused. The first endpoint is the host and port the request shows; `rule`
/// is all of it, as the gateway will apply it.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct DraftRule {
    /// The session it is for.
    pub session: String,
    pub chunk: String,
    pub host: String,
    pub port: u16,
    /// The methods its rules allow (empty: the whole connection, or every
    /// method of its access preset).
    pub methods: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// The programs it lets through.
    pub binaries: Vec<String>,
    /// Why, as the worker put it (or the gateway, for a refusal).
    pub rationale: String,
    /// What the gateway's checks found (its prover, its security notes).
    pub findings: String,
    /// Proposed by the worker, rather than drafted from a refusal.
    pub proposed: bool,
    pub rule: Value,
}

/// An officer's answer to a [`DraftRule`].
#[derive(Debug, Clone)]
pub enum DraftDecision {
    Approve {
        methods: Option<Vec<String>>,
        path: Option<String>,
    },
    Reject(String),
}

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
    let (code, err) = crate::drive::pump_command(stream, input, &mut out, limit).await?;
    Ok((code, out, err))
}

/// Runs a client call to completion from the office's synchronous firm
/// logic. Requires a multi-threaded runtime (the `zapper-office` binary's).
pub(crate) fn block<T>(fut: impl std::future::Future<Output = T>) -> T {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(fut))
}
