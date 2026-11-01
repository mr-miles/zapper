//! What the office's firm logic needs from the daemon, behind the
//! `OfficeClient` trait: `Local` (in-process `Core`) and `Remote` (over the
//! API socket). The socket client itself is shared with the workbench in
//! `zapper-client` and re-exported here.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use futures_util::future::BoxFuture;
use hyper::Method;
use serde_json::Value;
// The socket client, shared with the workbench.
use zapper_api_types::{NewSession, ServiceSpec, SessionInfo};
pub use zapper_client::{socket_path, url_encode, Answer, Zapper};
use zapper_core::{ApprovalApi, RuleApi, ServiceApi, ServiceInfo, SessionApi};
use zapper_driver::process::DriveStream;
use zapper_policy::proto::ShellRequest;
use zapper_policy::{AnyRule, NetRequest, NetRule, Scope};

/// What the office's firm logic needs from the daemon, so it reaches it
/// through this interface rather than a `Core` directly. Implemented
/// in-process by [`Local`] (wrapping `Core`) and, once the office is a
/// process of its own, by [`Remote`] (over the API socket). It grows as the
/// office's remaining `&Core` calls move onto it (proposal 13, step 4; see
/// `docs/office-process-assumptions.md`). The firm-logic calls
/// (`security::decide`, revocation) are synchronous; the session surface is
/// `async` — it is driven from the foreman's async loop, and its `Remote`
/// impl is network I/O. The async methods return a [`BoxFuture`] (not RPIT)
/// so the trait stays object-safe: it is held as `Arc<dyn OfficeClient>`.
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
    /// Removes a service or zone; what it was, if it was there.
    fn remove_service(&self, id: u64) -> Option<ServiceInfo>;

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
    /// Whether session `id`'s own policy allows `req` (as the proxy would
    /// judge it, with the session's `{session}`/`{label}` rewriting), and the
    /// rule that did — `None` if it would not be allowed. The officers' tools
    /// use this to see if a host is already reachable before granting.
    fn allowed(&self, id: &str, req: NetRequest) -> Option<String>;
}

/// The in-process office, linked into the daemon: calls `Core` directly.
pub struct Local(pub zapper_core::Core);

impl OfficeClient for Local {
    fn add_layer_rule(&self, name: &str, rule: AnyRule) -> AnyRule {
        self.0.add_layer_rule(name, rule)
    }
    fn remove_layer_rule(&self, name: &str, id: &str) -> bool {
        self.0.remove_layer_rule(name, id)
    }
    fn reset_layer(&self, name: &str) {
        *self.0.named_layer(name).write().unwrap() = zapper_policy::Policy::default();
    }
    fn record_writable(&self) -> bool {
        self.0.record_writable()
    }
    fn config(&self) -> zapper_policy::Config {
        <zapper_core::Core as zapper_core::ConfigApi>::config(&self.0)
    }
    fn session_count(&self) -> usize {
        self.0.sessions().iter().filter(|s| !s.ended()).count()
    }
    fn admit(&self, profile: &str) -> anyhow::Result<zapper_policy::Config> {
        self.0.admit(profile)
    }
    fn promote_rule(&self, what: &str, scope: &Scope, rule: NetRule) -> Result<AnyRule> {
        <zapper_core::Core as zapper_core::ConfigApi>::change(&self.0, what, |c| {
            let mut rule = AnyRule::Net(rule);
            let taken = c.rule_ids();
            rule.set_id(
                (1..)
                    .map(|n| format!("net-{n}"))
                    .find(|i| !taken.contains(i))
                    .unwrap(),
            );
            c.policy_mut(scope)
                .ok_or_else(|| anyhow::anyhow!("no {scope}"))?
                .insert(rule.clone(), Some(0));
            Ok(rule)
        })
    }
    fn create_session<'a>(&'a self, new: NewSession) -> BoxFuture<'a, Result<SessionInfo>> {
        Box::pin(async move {
            let s = self.0.create_session(new).await?;
            Ok(self.0.session_info(&s))
        })
    }
    fn stop_session<'a>(&'a self, id: &'a str, reason: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let s = self.0.session(id)?;
            self.0.stop_session(&s, reason).await;
            let _ = self.0.forget_session(id);
            Ok(())
        })
    }
    fn session(&self, id: &str) -> Result<SessionInfo> {
        let s = self.0.session(id)?;
        Ok(self.0.session_info(&s))
    }
    fn attach<'a>(
        &'a self,
        id: &'a str,
        req: ShellRequest,
    ) -> BoxFuture<'a, Result<Box<dyn DriveStream>>> {
        Box::pin(async move {
            let s = self.0.session(id)?;
            let opened = s.open_shell(req).await?;
            Ok(Box::new(opened.stream) as Box<dyn DriveStream>)
        })
    }
    fn connect_port<'a>(
        &'a self,
        id: &'a str,
        port: u32,
    ) -> BoxFuture<'a, Result<Box<dyn DriveStream>>> {
        Box::pin(async move {
            let s = self.0.session(id)?;
            Ok(Box::new(s.connect_port(port).await?) as Box<dyn DriveStream>)
        })
    }
    fn add_mcp_socket(
        &self,
        name: &str,
        sessions: BTreeMap<String, String>,
        socket: &str,
        instructions: Option<String>,
        owner: &str,
    ) -> Result<u64> {
        self.0
            .add_mcp_socket(name, sessions, socket, instructions, owner)
    }
    fn remove_mcp_server(&self, id: u64) -> bool {
        self.0.remove_mcp_server(id)
    }
    fn add_decider_socket(&self, sessions: BTreeMap<String, String>, socket: &str) -> Result<u64> {
        self.0.add_decider_socket(sessions, socket)
    }
    fn remove_decider(&self, id: u64) -> bool {
        self.0.remove_decider(id)
    }
    fn add_service(&self, spec: ServiceSpec) -> Result<u64> {
        self.0.add_service(spec.into())
    }
    fn add_zone(
        &self,
        domain: &str,
        sessions: BTreeMap<String, String>,
        owner: &str,
    ) -> Result<u64> {
        self.0.add_zone(domain, sessions, owner)
    }
    fn remove_service(&self, id: u64) -> Option<ServiceInfo> {
        self.0.remove_service(id)
    }
    fn requests(&self, id: &str) -> Vec<Value> {
        self.0.session(id).map(|s| s.requests()).unwrap_or_default()
    }
    fn put_session_rule(&self, id: &str, rule: AnyRule) {
        if let Ok(s) = self.0.session(id) {
            s.put_rule(rule);
        }
    }
    fn remove_session_rule(&self, id: &str, rule_id: &str) -> bool {
        self.0
            .session(id)
            .map(|s| s.remove_rule(rule_id).is_some())
            .unwrap_or(false)
    }
    fn allowed(&self, id: &str, req: NetRequest) -> Option<String> {
        let s = self.0.session(id).ok()?;
        let req = s.caller_of(&req);
        let layers = self.0.layers_for(Some(id), None).ok()?;
        let layers: Vec<_> = layers.iter().map(|(sc, p)| (sc.clone(), p)).collect();
        let (d, _) = zapper_policy::evaluate_net(&layers, &req);
        (d.action == zapper_policy::Action::Allow)
            .then(|| d.rule.clone().unwrap_or_else(|| "default".into()))
    }
}

/// The office as a process of its own (`zapper-office`), over the daemon's
/// API socket. The office's firm logic is synchronous, so each call blocks
/// on the current multi-threaded runtime — the binary's, not a test's or the
/// in-process daemon's (that is `Local`).
pub struct Remote(pub Zapper);

impl OfficeClient for Remote {
    fn add_layer_rule(&self, name: &str, rule: AnyRule) -> AnyRule {
        let body = serde_json::json!({ "scope": format!("role:{name}"), "rule": rule });
        let _ = block(self.0.call(Method::POST, "/v1/rules", Some(&body)));
        rule
    }
    fn remove_layer_rule(&self, _name: &str, id: &str) -> bool {
        // `remove_rule` finds the rule by its (unique) id across role layers.
        block(
            self.0
                .call(Method::DELETE, &format!("/v1/rules/{id}"), None),
        )
        .map(|a| a.status.is_success())
        .unwrap_or(false)
    }
    fn reset_layer(&self, name: &str) {
        // A run starts with its role layers empty; clear the named layer
        // through the daemon's layer route.
        let _ = block(self.0.call(
            Method::DELETE,
            &format!("/v1/layers/{}", url_encode(name)),
            None,
        ));
    }
    fn record_writable(&self) -> bool {
        block(self.0.get("/v1/record"))
            .ok()
            .and_then(|a| a.body["healthy"].as_bool())
            .unwrap_or(false)
    }
    fn config(&self) -> zapper_policy::Config {
        block(self.0.get("/v1/config?format=json"))
            .ok()
            .and_then(|a| serde_json::from_value(a.body).ok())
            .unwrap_or_default()
    }
    fn session_count(&self) -> usize {
        block(self.0.get("/v1/sessions"))
            .ok()
            .and_then(|a| a.body.as_array().map(|s| s.len()))
            .unwrap_or(0)
    }
    fn admit(&self, profile: &str) -> anyhow::Result<zapper_policy::Config> {
        let v = block(self.0.get(&format!("/v1/admit?profile={profile}")))?
            .ok()
            .map_err(|(_, why)| anyhow::anyhow!(why))?;
        Ok(serde_json::from_value(v)?)
    }
    fn promote_rule(&self, _what: &str, scope: &Scope, rule: NetRule) -> Result<AnyRule> {
        // The daemon's `/v1/rules` route assigns the rule a fresh `net-N` id
        // (from the rule's kind) and inserts it at `scope`, as `Local` does
        // in process; its answer is the rule as added.
        let body = serde_json::json!({
            "scope": scope.to_string(),
            "rule": AnyRule::Net(rule),
            "position": 0,
        });
        let mut v = block(self.0.call(Method::POST, "/v1/rules", Some(&body)))?
            .ok()
            .map_err(|(_, why)| anyhow::anyhow!(why))?;
        // The route's answer is the rule with an added `scope` key; drop it so
        // the rule deserializes (`NetRule` denies unknown fields).
        if let Some(obj) = v.as_object_mut() {
            obj.remove("scope");
        }
        Ok(serde_json::from_value(v)?)
    }
    fn create_session<'a>(&'a self, new: NewSession) -> BoxFuture<'a, Result<SessionInfo>> {
        // The API takes its own `CreateSession` shape (rules, not a `Policy`;
        // no shared layers — the office sets none on a worker/cluster session).
        let body = serde_json::json!({
            "profile": new.profile,
            "workdir": new.workdir,
            "rules": new.policy.rules(),
            "labels": new.labels,
            "persistent": new.persistent,
            "accept_changed_image": new.accept_changed_image,
            "ssh": new.ssh,
            "files": new.files,
            "mounts": new.mounts,
            "agent_overlay": new.agent_overlay,
            "forwards": new.forwards,
        });
        Box::pin(async move {
            let v = self
                .0
                .call(Method::POST, "/v1/sessions", Some(&body))
                .await?
                .ok()
                .map_err(|(_, why)| anyhow::anyhow!(why))?;
            Ok(serde_json::from_value(v)?)
        })
    }
    fn stop_session<'a>(&'a self, id: &'a str, _reason: &'a str) -> BoxFuture<'a, Result<()>> {
        // Stop then forget; the daemon records its own stop reason (the stop
        // route takes none).
        Box::pin(async move {
            self.0
                .call(Method::POST, &format!("/v1/sessions/{id}/stop"), None)
                .await?
                .ok()
                .map_err(|(_, why)| anyhow::anyhow!(why))?;
            let _ = self
                .0
                .call(Method::DELETE, &format!("/v1/sessions/{id}"), None)
                .await;
            Ok(())
        })
    }
    fn session(&self, id: &str) -> Result<SessionInfo> {
        let v = block(self.0.get(&format!("/v1/sessions/{id}")))?
            .ok()
            .map_err(|(_, why)| anyhow::anyhow!(why))?;
        Ok(serde_json::from_value(v)?)
    }
    fn attach<'a>(
        &'a self,
        id: &'a str,
        req: ShellRequest,
    ) -> BoxFuture<'a, Result<Box<dyn DriveStream>>> {
        Box::pin(async move {
            // The attach route takes the shell as query params and upgrades
            // the connection to the guest terminal (frame-transparent through
            // the daemon's `pump_shell`).
            let mut q = vec![format!(
                "argv={}",
                url_encode(&serde_json::to_string(&req.argv)?)
            )];
            if req.system {
                q.push("system=true".into());
            }
            if let Some((rows, cols)) = req.tty {
                q.push(format!("tty={rows}x{cols}"));
            }
            let path = format!("/v1/sessions/{id}/attach?{}", q.join("&"));
            Ok(Box::new(self.0.upgrade(&path).await?.0) as Box<dyn DriveStream>)
        })
    }
    fn connect_port<'a>(
        &'a self,
        id: &'a str,
        port: u32,
    ) -> BoxFuture<'a, Result<Box<dyn DriveStream>>> {
        Box::pin(async move {
            let path = format!("/v1/sessions/{id}/connect?port={port}");
            Ok(Box::new(self.0.upgrade(&path).await?.0) as Box<dyn DriveStream>)
        })
    }
    fn add_mcp_socket(
        &self,
        name: &str,
        sessions: BTreeMap<String, String>,
        socket: &str,
        instructions: Option<String>,
        _owner: &str,
    ) -> Result<u64> {
        // Owned by the operator the office authenticates as.
        let body = serde_json::json!({
            "name": name, "sessions": sessions, "socket": socket, "instructions": instructions,
        });
        let v = block(self.0.call(Method::POST, "/v1/mcp", Some(&body)))?
            .ok()
            .map_err(|(_, why)| anyhow::anyhow!(why))?;
        v["id"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("no id for the MCP server"))
    }
    fn remove_mcp_server(&self, id: u64) -> bool {
        block(self.0.call(Method::DELETE, &format!("/v1/mcp/{id}"), None))
            .map(|a| a.status.is_success())
            .unwrap_or(false)
    }
    fn add_decider_socket(&self, sessions: BTreeMap<String, String>, socket: &str) -> Result<u64> {
        let body = serde_json::json!({ "sessions": sessions, "socket": socket });
        let v = block(self.0.call(Method::POST, "/v1/deciders", Some(&body)))?
            .ok()
            .map_err(|(_, why)| anyhow::anyhow!(why))?;
        v["id"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("no id for the decider"))
    }
    fn remove_decider(&self, id: u64) -> bool {
        block(
            self.0
                .call(Method::DELETE, &format!("/v1/deciders/{id}"), None),
        )
        .map(|a| a.status.is_success())
        .unwrap_or(false)
    }
    fn add_service(&self, spec: ServiceSpec) -> Result<u64> {
        use zapper_api_types::ServiceTarget as T;
        let to = match spec.target {
            T::Socket { name, tls } => serde_json::json!({ "socket": name, "tls": tls }),
            T::Session { id, port } => serde_json::json!({ "session": id, "port": port }),
            T::Addr(addr) => serde_json::json!({ "addr": addr }),
        };
        let body = serde_json::json!({ "name": spec.name, "port": spec.port, "sessions": spec.sessions, "to": to });
        let v = block(self.0.call(Method::POST, "/v1/services", Some(&body)))?
            .ok()
            .map_err(|(_, why)| anyhow::anyhow!(why))?;
        v["id"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("no id for the service"))
    }
    fn add_zone(
        &self,
        domain: &str,
        sessions: BTreeMap<String, String>,
        _owner: &str,
    ) -> Result<u64> {
        // Owned by the operator the office authenticates as.
        let body = serde_json::json!({ "domain": domain, "sessions": sessions });
        let v = block(self.0.call(Method::POST, "/v1/zones", Some(&body)))?
            .ok()
            .map_err(|(_, why)| anyhow::anyhow!(why))?;
        v["id"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("no id for the zone"))
    }
    fn remove_service(&self, id: u64) -> Option<ServiceInfo> {
        // The office ignores what it was (it only needs it gone), so this
        // doesn't reconstruct the `ServiceInfo` from the response.
        let _ = block(
            self.0
                .call(Method::DELETE, &format!("/v1/services/{id}"), None),
        );
        None
    }
    fn requests(&self, id: &str) -> Vec<Value> {
        block(self.0.get(&format!("/v1/sessions/{id}/requests")))
            .ok()
            .and_then(|a| a.body.as_array().cloned())
            .unwrap_or_default()
    }
    fn put_session_rule(&self, id: &str, rule: AnyRule) {
        let body = serde_json::json!({ "scope": format!("session:{id}"), "rule": rule });
        let _ = block(self.0.call(Method::POST, "/v1/rules", Some(&body)));
    }
    fn remove_session_rule(&self, _id: &str, rule_id: &str) -> bool {
        // The daemon finds the rule by its id across layers.
        block(
            self.0
                .call(Method::DELETE, &format!("/v1/rules/{rule_id}"), None),
        )
        .map(|a| a.status.is_success())
        .unwrap_or(false)
    }
    fn allowed(&self, id: &str, req: NetRequest) -> Option<String> {
        let body = serde_json::json!({ "session": id, "net": req });
        let v = block(self.0.call(Method::POST, "/v1/evaluate", Some(&body)))
            .ok()?
            .ok()
            .ok()?;
        if v["decision"]["action"] != "allow" {
            return None;
        }
        Some(
            v["decision"]["rule"]
                .as_str()
                .unwrap_or("default")
                .to_string(),
        )
    }
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
fn block<T>(fut: impl std::future::Future<Output = T>) -> T {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(fut))
}

#[cfg(test)]
mod tests {
    use zapper_core::testing::shell_vm;
    use zapper_policy::proto::ShellRequest;

    use crate::{test_core, test_run};

    const FIRM: &str = "
profiles:
  w: { agent: { kind: claude-code } }
firms:
  acme:
    roles:
      ceo: { profile: w, agent: ceo }
      eng: { profile: w, agent: software-engineer }
    workers:
      - { name: ceo, role: ceo }
      - { name: pat, role: eng, reports_to: ceo }
";

    // The office reads a worker's session back through its client, by id, and
    // attaches a terminal the VM serves — the surface the foreman drives over
    // once it holds the id rather than an `Arc<Session>` (P1-c).
    #[tokio::test]
    async fn local_session_surface() {
        let t = test_core(FIRM);
        let run = test_run(&t.core, "acme");
        let s = run
            .rt("pat")
            .unwrap()
            .session_arc
            .lock()
            .unwrap()
            .clone()
            .unwrap();
        let id = s.id.clone();

        // session(id): the info the office needs, read over the client.
        let info = run.client.session(&id).unwrap();
        assert_eq!(info.id(), id);
        assert_eq!(info.label("member"), Some("pat"));
        assert_eq!(info.label("firm"), Some("acme"));

        // SessionInfo round-trips through JSON — what Remote relies on to read
        // the API's responses over the socket (P3).
        let json = serde_json::to_value(&info).unwrap();
        let back: zapper_core::SessionInfo = serde_json::from_value(json).unwrap();
        assert_eq!(back.id(), id);
        assert_eq!(back.label("member"), Some("pat"));
        assert_eq!(back.profile(), info.profile());

        // attach(id): a terminal stream the worker's VM receives.
        let mut vm = shell_vm(&s);
        let argv = vec!["claude".to_string(), "-p".to_string()];
        let _stream = run
            .client
            .attach(&id, ShellRequest::exec(argv.clone()))
            .await
            .unwrap();
        let shell = vm.recv().await.unwrap();
        assert_eq!(shell.request.argv, argv);

        // A session the office doesn't know: an error, not a panic.
        assert!(run.client.session("no-such-session").is_err());
    }

    // The `Remote` client against the real API router, served on a unix socket
    // as the daemon serves it — the office↔core path a standalone office takes,
    // end to end without a VM (the one exception, `create_session`, boots a VM
    // at the daemon and so needs KVM; everything else does not).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn remote_over_the_socket() {
        use std::collections::BTreeMap;

        use zapper_api_types::{ServiceSpec, ServiceTarget};
        use zapper_core::{ConfigApi, ServiceApi};

        use super::OfficeClient;

        let t = test_core(FIRM);
        let run = test_run(&t.core, "acme");
        let s = run
            .rt("pat")
            .unwrap()
            .session_arc
            .lock()
            .unwrap()
            .clone()
            .unwrap();
        let id = s.id.clone();

        // Serve the API on a temp socket, exactly as the daemon does.
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let sock =
            std::env::temp_dir().join(format!("zo-remote-{}-{nonce}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        let app = zapper_api::api(t.core.clone(), &zapper_api::Mount::default());
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let serving = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let remote = super::Remote(super::Zapper::new(sock.clone()));

        // session(id) over the socket.
        let info = remote.session(&id).unwrap();
        assert_eq!(info.id(), id);
        assert_eq!(info.label("member"), Some("pat"));
        assert!(remote.session("no-such-session").is_err());

        // A zone and a socket service: registered in the daemon's registry.
        let zone = remote
            .add_zone("acme.test", BTreeMap::new(), "firm acme")
            .unwrap();
        assert_eq!(t.core.service(zone).map(|s| s.kind), Some("zone"));
        let svc = remote
            .add_service(ServiceSpec {
                name: "git.acme.test".into(),
                port: 443,
                sessions: BTreeMap::new(),
                target: ServiceTarget::Socket {
                    name: "office-acme-git.sock".into(),
                    tls: true,
                },
                owner: "firm acme".into(),
            })
            .unwrap();
        assert!(t.core.service(svc).is_some());
        remote.remove_service(svc);
        assert!(t.core.service(svc).is_none());
        remote.remove_service(zone);

        // An MCP socket and a decider socket: added and removed.
        let mcp = remote
            .add_mcp_socket(
                "office",
                BTreeMap::new(),
                "office-acme.sock",
                None,
                "firm acme",
            )
            .unwrap();
        assert!(t.core.mcp_servers().iter().any(|m| m["id"] == mcp));
        assert!(remote.remove_mcp_server(mcp));
        let dec = remote
            .add_decider_socket(BTreeMap::new(), "office-acme-decider.sock")
            .unwrap();
        assert!(remote.remove_decider(dec));

        // attach(id): the upgrade reaches the guest shell, which gets the argv.
        let mut vm = shell_vm(&s);
        let argv = vec!["claude".to_string(), "-p".to_string()];
        let _stream = remote
            .attach(&id, ShellRequest::exec(argv.clone()))
            .await
            .unwrap();
        let shell = vm.recv().await.unwrap();
        assert_eq!(shell.request.argv, argv);

        // reset_layer: a role's shared layer, added to over the socket, is
        // then cleared over the socket (the clear-layer route).
        let layer = "acme/eng";
        let probe = zapper_policy::AnyRule::Net(
            serde_json::from_value(serde_json::json!({
                "id": "net-probe", "action": "allow", "host": "probe.test", "ports": [443]
            }))
            .unwrap(),
        );
        remote.add_layer_rule(layer, probe);
        assert!(!t.core.named_layer(layer).read().unwrap().is_empty());
        remote.reset_layer(layer);
        assert!(t.core.named_layer(layer).read().unwrap().is_empty());

        // promote_rule: a net rule promoted into a profile gets a fresh net-N
        // id (from the `/v1/rules` route) and lands in the running config.
        let rule = serde_json::from_value(serde_json::json!({
            "action": "allow", "host": "promoted.test", "ports": [443]
        }))
        .unwrap();
        let added = remote
            .promote_rule(
                "test promote",
                &zapper_policy::Scope::Profile("w".into()),
                rule,
            )
            .unwrap();
        assert!(added.id().unwrap().starts_with("net-"));
        assert!(t
            .core
            .config()
            .profiles
            .get("w")
            .unwrap()
            .policy
            .rules()
            .iter()
            .any(|r| r.id() == added.id()));

        serving.abort();
        let _ = std::fs::remove_file(&sock);
    }
}
