//! What zapper's layers need from where their sandboxes run, behind the
//! `OfficeClient` trait: [`crate::OpenShell`] in production, an in-memory
//! fake in the office's tests.

use crate::session::{NewSession, SessionInfo};
use anyhow::Result;
use futures_util::future::BoxFuture;
use serde_json::Value;
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
    /// The running zapper config (`zapper.yaml`): the profiles the office
    /// validates a start against, and the UI reads.
    fn config(&self) -> zapper_policy::Config;
    /// How many sessions are running now (the firm-level pre-flight: enough
    /// room under `max_sessions` for the firm's workers).
    fn session_count(&self) -> usize;
    /// Whether the host has room for a session of `profile`, and the running
    /// config if so (the host-room decision in one call: limit + memory).
    fn admit(&self, profile: &str) -> anyhow::Result<zapper_policy::Config>;
    /// Promotes a standing approval's `rule` into the running config at
    /// `scope`, under change note `what`, giving it a fresh `net-N` id; the
    /// rule as added.
    fn promote_rule(&self, what: &str, scope: &Scope, rule: NetRule) -> Result<AnyRule>;

    // --- sessions ----------------------------------------------------------
    // The office creates, drives, stops and forgets a worker's (or the
    // cluster's) sandbox through these, keeping only the id it gets back.

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
    /// agent over it ([`zapper_driver::process::start`]).
    fn attach<'a>(
        &'a self,
        id: &'a str,
        req: ShellRequest,
    ) -> BoxFuture<'a, Result<Box<dyn DriveStream>>>;
    /// Opens a raw byte stream to port `port` of session `id`'s sandbox (on
    /// OpenShell, `ForwardTcp`), for the office to proxy: the board's
    /// dashboard and product ports, the cluster's API.
    fn connect_port<'a>(
        &'a self,
        id: &'a str,
        port: u32,
    ) -> BoxFuture<'a, Result<Box<dyn DriveStream>>>;

    // --- a session's own rule layer and traffic ---------------------------
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
    /// can use (the office edge: its MCP tools at `/mcp`, git at `/git/`,
    /// the cluster at `/cluster` and `/apps/`), with their identity added
    /// outside the VM.
    fn edge(&self) -> String;

    // --- drafted rules (OpenShell's policy advisor) -------------------------
    // Where the sandboxes' network drafts rules for review (a worker's
    // proposal through `policy.local`, or a connection it refused), the
    // officers review them.

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

/// Runs a client call to completion from the office's synchronous firm
/// logic. Requires a multi-threaded runtime (the `zapper-office` binary's).
pub fn block<T>(fut: impl std::future::Future<Output = T>) -> T {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(fut))
}
