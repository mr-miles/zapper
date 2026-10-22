//! `Core`: what the daemon offers its clients (the REST API and UI server,
//! the binary, and the office), and nothing of its internals.
//! Clients hold a `Core` and call it; the daemon's state stays behind it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Result};
use futures_util::future::BoxFuture;
use serde_json::{json, Value};
use tokio::net::UnixStream;
use tokio::sync::broadcast;
use zapper_policy::{AnyRule, Config, Policy, Scope};

use crate::daemon::{self, session, Daemon, EventBody};

pub use crate::daemon::clients::RuleKeeper;
pub use crate::daemon::deciders::{Decider, Referral};
pub use crate::daemon::gateway::McpServer;
pub use crate::daemon::services::{
    Handler as ServiceHandler, Info as ServiceInfo, Io as ServiceIo, Spec as ServiceSpec,
    Target as ServiceTarget,
};
pub use crate::daemon::session::{
    NewSession, Session, SessionInfo, Status, TermChunk, TerminalInfo,
};
pub use crate::daemon::{Event, Pending, Remember};

/// A handle on a running daemon. Cheap to clone.
#[derive(Clone)]
pub struct Core {
    d: Arc<Daemon>,
}

/// What removing a rule did.
pub enum RuleRemoval {
    /// The rule left its layer.
    Removed(Scope, Box<AnyRule>),
    /// It was a client's own rule (a firm officer's standing approval):
    /// removed through the client, so its records agree.
    Revoked,
}

impl Core {
    /// The daemon, for tests of its parts that take it.
    #[cfg(any(test, feature = "test-util"))]
    pub(crate) fn daemon(&self) -> &Arc<Daemon> {
        &self.d
    }

    /// Loads the startup config and starts the daemon's state.
    pub fn start(startup_path: PathBuf) -> Result<Core> {
        Ok(Core {
            d: Daemon::new(startup_path)?,
        })
    }

    /// The state of type `T` a client keeps in the core, made on first use:
    /// one per core, whatever else the process holds.
    pub fn client<T: Default + Send + Sync + 'static>(&self) -> Arc<T> {
        self.d.clients.state()
    }

    /// Adds a check every config change must pass (a client's section of
    /// it, say), besides the config's own validation.
    pub fn add_config_check(&self, check: impl Fn(&Config) -> Result<()> + Send + Sync + 'static) {
        self.d
            .clients
            .config_checks
            .write()
            .unwrap()
            .push(Arc::new(check));
    }

    /// Checks the running config against the checks added since it was
    /// loaded: for a client's checks of the startup config.
    pub fn check_config(&self) -> Result<()> {
        self.d.clients.check(&self.d.config.read().unwrap())
    }

    /// Adds a client's own rules to the layers listed, and lets it remove
    /// them itself.
    pub fn add_rule_keeper(&self, keeper: Arc<dyn RuleKeeper>) {
        self.d.clients.rule_keepers.write().unwrap().push(keeper);
    }

    /// Runs `hook` when the daemon stops, before sessions are stopped.
    pub fn on_shutdown(
        &self,
        hook: impl Fn(Core) -> BoxFuture<'static, ()> + Send + Sync + 'static,
    ) {
        self.d
            .clients
            .shutdown
            .write()
            .unwrap()
            .push(Arc::new(hook));
    }

    // The config.

    /// The running config.
    pub fn config(&self) -> Config {
        self.d.config.read().unwrap().clone()
    }

    /// Reads the running config without copying it.
    pub fn with_config<T>(&self, f: impl FnOnce(&Config) -> T) -> T {
        f(&self.d.config.read().unwrap())
    }

    /// The running config with inline secret values hidden.
    pub fn redacted_config(&self) -> Config {
        self.d.redacted_config()
    }

    /// The startup file, as it is on disk.
    pub fn startup_config(&self) -> Result<Config> {
        self.d.startup_config()
    }

    pub fn startup_path(&self) -> &Path {
        &self.d.startup_path
    }

    /// What `save` would change in the startup file.
    pub fn diff(&self) -> String {
        self.d.diff()
    }

    /// Changes the running config, if the result is valid as a whole.
    pub fn change<T>(&self, what: &str, f: impl FnOnce(&mut Config) -> Result<T>) -> Result<T> {
        self.d.change(what, f)
    }

    pub fn replace_config(&self, config: Config) -> Result<()> {
        self.d.replace_config(config)
    }

    /// Writes the running config to the startup file.
    pub fn save(&self) -> Result<()> {
        self.d.save()
    }

    /// Drops running changes: the startup file again.
    pub fn reload(&self) -> Result<()> {
        self.d.reload()
    }

    // Sessions.

    pub fn session(&self, id: &str) -> Result<Arc<Session>> {
        self.d.session(id)
    }

    pub fn sessions(&self) -> Vec<Arc<Session>> {
        self.d.sessions.read().unwrap().values().cloned().collect()
    }

    /// The sessions with a label set to a value, e.g. `firm` = `acme`.
    pub fn sessions_labelled(&self, key: &str, value: &str) -> Vec<Arc<Session>> {
        self.sessions()
            .into_iter()
            .filter(|s| s.label(key) == Some(value))
            .collect()
    }

    /// Starts a session; returns once its guest is ready.
    pub async fn create_session(&self, new: NewSession) -> Result<Arc<Session>> {
        session::create(self.d.clone(), new).await
    }

    /// Stops a session; it stays listed, with its audit log.
    pub async fn stop_session(&self, s: &Arc<Session>, reason: &str) {
        session::stop(&self.d, s, reason).await
    }

    /// Takes a stopped session off the list; a session still starting or
    /// running stays, as nothing would manage its VM.
    pub fn forget_session(&self, id: &str) -> Result<()> {
        let mut sessions = self.d.sessions.write().unwrap();
        if let Some(s) = sessions.get(id) {
            if matches!(
                *s.status.borrow(),
                session::Status::Starting | session::Status::Running
            ) {
                return Err(anyhow!("session {id} is still running; stop it first"));
            }
        }
        sessions.remove(id);
        Ok(())
    }

    /// A session registered as `new` describes, without a VM: for tests of
    /// what sits above sessions. `running` marks it running.
    #[cfg(any(test, feature = "test-util"))]
    pub fn test_session(&self, new: NewSession, running: bool) -> Result<Arc<Session>> {
        let s = session::register(&self.d, &new)?;
        if running {
            s.status.send_replace(session::Status::Running);
        }
        Ok(s)
    }

    /// Registers a service; its id, for removing it.
    pub fn add_service(&self, spec: ServiceSpec) -> Result<u64> {
        let id = self.d.services.add(spec)?;
        self.service_event("added", id);
        Ok(id)
    }

    /// Makes `domain` a zone for the sessions `sessions` picks: its names
    /// resolve only to services, never on the host. Its id, for removing it.
    pub fn add_zone(
        &self,
        domain: &str,
        sessions: BTreeMap<String, String>,
        owner: &str,
    ) -> Result<u64> {
        let id = self.d.services.add_zone(domain, sessions, owner)?;
        self.service_event("added", id);
        Ok(id)
    }

    /// Removes a service or zone; what it was, if it was there.
    pub fn remove_service(&self, id: u64) -> Option<ServiceInfo> {
        let info = self.d.services.remove(id)?;
        self.d.emit(
            None,
            EventBody::Service {
                change: "removed".into(),
                service: info.clone(),
            },
        );
        Some(info)
    }

    /// Makes `decider` decide the `ask`s of the sessions `sessions` picks
    /// (by label; every session when empty), instead of the operator: they
    /// are refused at once and referred to it. The earliest registered for
    /// a session decides. Its id, for removing it.
    pub fn add_decider(
        &self,
        sessions: BTreeMap<String, String>,
        decider: Arc<dyn Decider>,
    ) -> u64 {
        self.d.deciders.add(sessions, decider)
    }

    /// Serves `server` as the MCP server `name` to the sessions `sessions`
    /// picks (by label; every session when empty), through the gateway on
    /// their MCP vsock port. Its id, for removing it.
    pub fn add_mcp_server(
        &self,
        name: &str,
        sessions: BTreeMap<String, String>,
        server: Arc<dyn McpServer>,
    ) -> Result<u64> {
        self.d.gateway.add(name, sessions, server)
    }

    /// Stops serving a registered MCP server.
    pub fn remove_mcp_server(&self, id: u64) -> bool {
        self.d.gateway.remove(id)
    }

    /// Hands a decider's sessions' `ask`s back to the operator.
    pub fn remove_decider(&self, id: u64) -> bool {
        self.d.deciders.remove(id)
    }

    /// Services and zones.
    pub fn services(&self) -> Vec<ServiceInfo> {
        self.d.services.list()
    }

    fn service_event(&self, change: &str, id: u64) {
        if let Some(service) = self.d.services.info(id) {
            self.d.emit(
                None,
                EventBody::Service {
                    change: change.into(),
                    service,
                },
            );
        }
    }

    /// A session's info, saying so when the driver is off.
    pub fn session_info(&self, s: &Session) -> SessionInfo {
        let mut i = s.info();
        if !self.d.config.read().unwrap().daemon.driver {
            i.driver = json!("disabled");
        }
        i
    }

    /// Adds a rule to a session's own layer.
    pub fn add_session_rule(
        &self,
        s: &Arc<Session>,
        rule: AnyRule,
        position: Option<usize>,
    ) -> AnyRule {
        let rule = s.add_rule(rule, position);
        self.d.emit(
            Some(s),
            EventBody::Config {
                change: format!(
                    "rule {} added to {}",
                    rule.id().unwrap_or("?"),
                    Scope::Session(s.id.clone())
                ),
            },
        );
        rule
    }

    /// A session's audit log: kept by the session while it is listed, else
    /// from its directory.
    pub fn audit_path(&self, id: &str) -> Result<PathBuf> {
        match self.d.session(id) {
            Ok(s) => Ok(s.audit_path()),
            Err(_) => {
                // The id goes into a path: accept only the form session ids
                // have.
                if !daemon::is_session_id(id) {
                    return Err(anyhow!("{id:?} is not a session id"));
                }
                Ok(daemon::record::view(
                    &self
                        .d
                        .paths
                        .state
                        .join("sessions")
                        .join(id)
                        .join("audit.jsonl"),
                ))
            }
        }
    }

    /// Pumps an attached client's frames to and from a terminal opened with
    /// `Session::open_shell`, recording it.
    pub async fn pump_shell<C>(
        &self,
        s: Arc<Session>,
        client: C,
        guest: UnixStream,
        id: u64,
        argv: Vec<String>,
        tty: Option<(u16, u16)>,
    ) where
        C: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        session::pump_shell(self.d.clone(), s, client, guest, id, argv, tty).await
    }

    // Policy.

    /// Every rule layer: global, profiles, sessions and firms' roles.
    pub fn rule_layers(&self) -> Vec<(Scope, Policy)> {
        let c = self.d.config.read().unwrap();
        let mut layers: Vec<(Scope, Policy)> = vec![(Scope::Global, c.policy.clone())];
        layers.extend(
            c.profiles
                .iter()
                .map(|(n, p)| (Scope::Profile(n.clone()), p.policy.clone())),
        );
        for (id, s) in self.d.sessions.read().unwrap().iter() {
            layers.push((Scope::Session(id.clone()), s.policy.read().unwrap().clone()));
        }
        for k in self.d.clients.rule_keepers.read().unwrap().iter() {
            layers.extend(k.layers());
        }
        layers
    }

    /// Removes a rule from whichever layer has it.
    pub fn remove_rule(&self, id: &str) -> Result<RuleRemoval> {
        // A client's own rule (a firm officer's standing approval): removed
        // through it, so its records agree.
        let keepers = self.d.clients.rule_keepers.read().unwrap().clone();
        for k in keepers {
            if let Some(r) = k.remove(id) {
                r?;
                return Ok(RuleRemoval::Revoked);
            }
        }
        if id.starts_with("s-") {
            for s in self.d.sessions.read().unwrap().values() {
                if let Some(r) = s.policy.write().unwrap().remove(id) {
                    return Ok(RuleRemoval::Removed(
                        Scope::Session(s.id.clone()),
                        Box::new(r),
                    ));
                }
            }
            return Err(anyhow!("no rule {id}"));
        }
        let (scope, rule) = self.d.change(&format!("rule {id} removed"), |c| {
            let scope = c.find_rule(id).ok_or_else(|| anyhow!("no rule {id}"))?;
            let rule = c.policy_mut(&scope).unwrap().remove(id).unwrap();
            Ok((scope, rule))
        })?;
        Ok(RuleRemoval::Removed(scope, Box::new(rule)))
    }

    /// The layers a request would be decided by: a session's, a profile's
    /// with the global layer, or the global layer alone.
    pub fn layers_for(
        &self,
        session: Option<&str>,
        profile: Option<&str>,
    ) -> Result<Vec<(Scope, Policy)>> {
        let c = self.d.config.read().unwrap();
        Ok(match (session, profile) {
            (Some(id), _) => self.d.session(id)?.layers(&c),
            (None, Some(p)) => {
                let mut l = c.resolve(p)?.layers;
                l.push((Scope::Global, c.policy.clone()));
                l
            }
            (None, None) => vec![(Scope::Global, c.policy.clone())],
        })
    }

    // Approvals and events.

    pub fn approvals(&self) -> Vec<Pending> {
        self.d.approvals()
    }

    pub fn answer(&self, id: &str, allow: bool, remember: Option<Remember>) -> Result<()> {
        self.d.answer(id, allow, remember)
    }

    /// Recent events, of one session or all.
    pub fn recent_events(&self, session: Option<&str>) -> Vec<Event> {
        self.d.recent_events(session)
    }

    /// Events as they happen.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.d.events.subscribe()
    }

    /// Who keeps the record, and whether it is being written.
    pub fn record_status(&self) -> Value {
        daemon::record::status()
    }

    // The UI and the state directory.

    /// The token the UI's first request carries.
    pub fn ui_token(&self) -> &str {
        &self.d.ui_token
    }

    pub fn state_dir(&self) -> &Path {
        &self.d.paths.state
    }

    /// Runs the clients' shutdown hooks (the office stops its firms), then
    /// stops sessions: the daemon is going away.
    pub async fn shutdown(&self) {
        let hooks = self.d.clients.shutdown.read().unwrap().clone();
        for hook in hooks {
            hook(self.clone()).await;
        }
        for s in self.sessions() {
            session::stop(&self.d, &s, "daemon shutdown").await;
        }
    }
}

#[cfg(test)]
mod tests;
