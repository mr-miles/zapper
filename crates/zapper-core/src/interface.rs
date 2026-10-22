//! `Core`: what the daemon offers its clients (the REST API and UI server,
//! the binary, and in time the office), and nothing of its internals.
//! Clients hold a `Core` and call it; the daemon's state stays behind it.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Result};
use axum::Router;
use serde_json::{json, Value};
use tokio::net::UnixStream;
use tokio::sync::broadcast;
use zapper_policy::{AnyRule, Config, Policy, Scope};

use crate::daemon::{self, session, Daemon, EventBody};

pub use crate::daemon::session::{NewSession, Session, SessionInfo, TermChunk, TerminalInfo};
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
    /// It was an officer's standing approval in a firm: revoked there, so
    /// the firm's records agree.
    Revoked,
}

impl Core {
    /// Loads the startup config and starts the daemon's state.
    pub fn start(startup_path: PathBuf) -> Result<Core> {
        Ok(Core {
            d: Daemon::new(startup_path)?,
        })
    }

    // The config.

    /// The running config.
    pub fn config(&self) -> Config {
        self.d.config.read().unwrap().clone()
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

    /// Starts a session; returns once its guest is ready.
    pub async fn create_session(&self, new: NewSession) -> Result<Arc<Session>> {
        session::create(self.d.clone(), new).await
    }

    /// Stops a session; it stays listed, with its audit log.
    pub async fn stop_session(&self, s: &Arc<Session>, reason: &str) {
        session::stop(&self.d, s, reason).await
    }

    /// Takes a stopped session off the list.
    pub fn forget_session(&self, id: &str) {
        self.d.sessions.write().unwrap().remove(id);
    }

    /// A session registered as `new` describes, without a VM: for tests of
    /// what sits above sessions. `running` marks it running.
    #[cfg(any(test, feature = "test-util"))]
    pub fn test_session(&self, new: NewSession, running: bool) -> Result<Arc<Session>> {
        session::test_session(&self.d, &new, running)
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
        for run in self.d.office.runs.read().unwrap().values() {
            for (role, p) in &run.roles {
                layers.push((
                    Scope::Role(format!("{}/{role}", run.firm)),
                    p.read().unwrap().clone(),
                ));
            }
        }
        layers
    }

    /// Removes a rule from whichever layer has it.
    pub fn remove_rule(&self, id: &str) -> Result<RuleRemoval> {
        // An officer's standing approval: revoked, so the firm's records agree.
        let runs: Vec<_> = self
            .d
            .office
            .runs
            .read()
            .unwrap()
            .values()
            .cloned()
            .collect();
        for run in runs {
            if run.security.lock().unwrap().standing.contains_key(id) {
                daemon::office::security::revoke(
                    &run,
                    zapper_policy::BOARD,
                    id,
                    "removed by the operator",
                )?;
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

    // The UI, the state directory and the office.

    /// The token the UI's first request carries.
    pub fn ui_token(&self) -> &str {
        &self.d.ui_token
    }

    pub fn state_dir(&self) -> &Path {
        &self.d.paths.state
    }

    /// The office's API routes (firms, mail, security, the drive), for the
    /// API and the UI to serve; none without the office.
    pub fn office_routes(&self) -> Router {
        daemon::office::http::read_routes()
            .merge(daemon::office::http::board_routes())
            .with_state(self.d.clone())
    }

    /// The office's page, the UI's front page when there is an office.
    pub fn office_page(&self) -> Option<&'static str> {
        #[cfg(feature = "office")]
        return Some(include_str!("daemon/office.html"));
        #[cfg(not(feature = "office"))]
        None
    }

    /// Stops firms, then sessions: the daemon is going away.
    pub async fn shutdown(&self) {
        let runs: Vec<_> = self
            .d
            .office
            .runs
            .read()
            .unwrap()
            .values()
            .cloned()
            .collect();
        for r in runs {
            daemon::office::down(&self.d, &r, "daemon shutdown").await;
        }
        for s in self.sessions() {
            session::stop(&self.d, &s, "daemon shutdown").await;
        }
    }
}
