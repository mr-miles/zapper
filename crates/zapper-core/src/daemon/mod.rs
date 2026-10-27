//! The zapper daemon: sessions, policy, egress proxy, API.
//!
//! Configuration works like a router's: changes through the API apply to the
//! running config at once; `POST /v1/config/save` writes it to the startup
//! file, `POST /v1/config/reload` throws the running config away for the
//! startup file, and `GET /v1/config/diff` shows what save would change.

pub(crate) mod addr;
pub(crate) mod api_error;
pub(crate) mod approvals;
pub(crate) mod clients;
pub(crate) mod deciders;
pub(crate) mod dns;
pub mod driver;
pub(crate) mod events;
pub(crate) mod gateway;
pub mod harness;
pub(crate) mod httplog;
pub(crate) mod nic;
pub(crate) mod proxy;
pub mod record;
pub(crate) mod registry;
pub(crate) mod services;
pub(crate) mod session;
mod summary;
pub(crate) mod tls;
pub(crate) mod vm;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use tokio::sync::oneshot;
use zapper_policy::{expand_home, Action, AnyRule, Config, Kind, SecretSource};

use session::Session;

// The event model and the stream live in `events`; the approval model and
// the outstanding approvals in `approvals`.
pub use approvals::{Pending, Remember};
pub use events::{Event, EventBody};

// Generic helpers live in `crate::util` (the clients' shared helpers);
// re-exported here so the daemon's modules reach them as `super::now` etc.
pub(crate) use crate::util::{is_session_id, now, random_id, write_private};

pub struct Paths {
    pub state: PathBuf,
    pub images: PathBuf,
    pub kernels: PathBuf,
    pub firecracker: PathBuf,
    pub initrd: PathBuf,
}

pub struct Daemon {
    pub config: RwLock<Config>,
    pub startup_path: PathBuf,
    pub paths: Paths,
    pub addrs: RwLock<addr::AddressPolicy>,
    pub(crate) sessions: session::SessionStore,
    pub(crate) events: events::EventBus,
    approvals: approvals::Approvals,
    pub ui_token: String,
    pub clients: clients::Clients,
    pub services: services::Services,
    pub deciders: deciders::Deciders,
    pub gateway: gateway::Gateway,
}

/// Finds the guest agent binary.
fn find_agent(configured: &str) -> Result<PathBuf> {
    if !configured.is_empty() {
        return Ok(PathBuf::from(expand_home(configured)));
    }
    let exe = std::env::current_exe()?;
    let dir = exe.parent().unwrap_or(Path::new("."));
    let candidates = [
        dir.join("zapper-agent"),
        // Development: `cargo build` next to a musl build of the agent.
        dir.join("../x86_64-unknown-linux-musl/release/zapper-agent"),
        dir.join("../../x86_64-unknown-linux-musl/release/zapper-agent"),
    ];
    candidates.into_iter().find(|p| p.is_file()).ok_or_else(|| {
        anyhow!(
            "zapper-agent not found next to {}; set daemon.agent",
            exe.display()
        )
    })
}

impl Daemon {
    pub fn new(startup_path: PathBuf) -> Result<Arc<Self>> {
        let config = if startup_path.exists() {
            let text = std::fs::read_to_string(&startup_path)
                .with_context(|| format!("reading {}", startup_path.display()))?;
            Config::from_yaml(&text)
                .with_context(|| format!("loading {}", startup_path.display()))?
        } else {
            tracing::warn!(
                "{} does not exist; starting with an empty config",
                startup_path.display()
            );
            Config::default()
        };
        if let Ok(meta) = std::fs::metadata(&startup_path) {
            if meta.permissions().mode() & 0o077 != 0 && config.has_inline_secrets() {
                tracing::warn!(
                    "{} is readable by others; it may hold secrets (chmod 600)",
                    startup_path.display()
                );
            }
        }
        let d = &config.daemon;
        let state = PathBuf::from(expand_home(&d.state));
        std::fs::create_dir_all(state.join("sessions"))?;
        std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o700))?;
        // VMM directories (disks, sockets) of sessions a previous daemon left
        // behind; audit logs and recordings stay.
        for dir in std::fs::read_dir(state.join("sessions"))?.flatten() {
            let _ = std::fs::remove_dir_all(session::SessionDir::new(dir.path()).vm());
        }
        // The record: through the keeper, if configured; it must answer.
        record::init(&state, d.keeper.as_ref())?;
        let initrd = state.join("initrd.cpio");
        vm::build_initrd(&find_agent(&d.agent)?, &initrd)?;
        let paths = Paths {
            state,
            images: PathBuf::from(expand_home(&d.images)),
            kernels: PathBuf::from(expand_home(&d.kernels)),
            firecracker: PathBuf::from(expand_home(&d.firecracker)),
            initrd,
        };
        if !paths.firecracker.is_file() {
            bail!("firecracker not found at {}", paths.firecracker.display());
        }
        let addrs = addr::AddressPolicy::new(&d.private_networks)?;
        Ok(Arc::new(Daemon {
            config: RwLock::new(config),
            startup_path,
            paths,
            addrs: RwLock::new(addrs),
            sessions: session::SessionStore::new(),
            events: events::EventBus::new(),
            approvals: approvals::Approvals::new(),
            ui_token: random_id(24),
            clients: clients::Clients::default(),
            services: services::Services::default(),
            deciders: deciders::Deciders::default(),
            gateway: gateway::Gateway::default(),
        }))
    }

    pub fn emit(&self, session: Option<&Session>, body: EventBody) {
        self.events.emit(session, body);
    }

    pub fn recent_events(&self, session: Option<&str>) -> Vec<Event> {
        self.events.recent(session)
    }

    pub fn session(&self, id: &str) -> Result<Arc<Session>> {
        self.sessions
            .get(id)
            .ok_or_else(|| anyhow!("no session {id}"))
    }

    /// Applies a change to the running config if the result is valid.
    pub fn change<T>(&self, what: &str, f: impl FnOnce(&mut Config) -> Result<T>) -> Result<T> {
        let mut next = self.config.read().unwrap().clone();
        let out = f(&mut next)?;
        next.normalize();
        next.validate()?;
        if !next.daemon.driver {
            let driven: Vec<String> = self
                .sessions
                .all()
                .into_iter()
                .filter(|s| s.driver.is_some() && s.is_running())
                .map(|s| s.id.clone())
                .collect();
            if !driven.is_empty() {
                anyhow::bail!(
                    "daemon.driver: driven sessions are running ({}); stop them first",
                    driven.join(", ")
                );
            }
        }
        self.clients.check(&next)?;
        if next.daemon.private_networks != self.config.read().unwrap().daemon.private_networks {
            *self.addrs.write().unwrap() = addr::AddressPolicy::new(&next.daemon.private_networks)?;
        }
        *self.config.write().unwrap() = next;
        self.emit(
            None,
            EventBody::Config {
                change: what.to_string(),
            },
        );
        Ok(out)
    }

    /// The running config with inline secret values hidden.
    pub fn redacted_config(&self) -> Config {
        redact(self.config.read().unwrap().clone())
    }

    /// Replaces the running config; redacted inline secrets keep their values.
    pub fn replace_config(&self, mut new: Config) -> Result<()> {
        let old = self.config.read().unwrap().clone();
        for (name, s) in new.secrets.iter_mut() {
            if let SecretSource::Value { value } = &mut s.source {
                if value == REDACTED {
                    match old.secrets.get(name).map(|o| &o.source) {
                        Some(SecretSource::Value { value: v }) => *value = v.clone(),
                        _ => bail!("secret {name}: value is redacted and there is no old value"),
                    }
                }
            }
        }
        self.change("config replaced", |c| {
            *c = new;
            Ok(())
        })
    }

    fn startup_yaml(&self) -> String {
        std::fs::read_to_string(&self.startup_path).unwrap_or_default()
    }

    fn running_yaml(&self) -> String {
        self.config.read().unwrap().to_yaml()
    }

    /// The startup config, normalised and redacted.
    pub fn startup_config(&self) -> Result<Config> {
        if !self.startup_path.exists() {
            return Ok(Config::default());
        }
        Ok(redact(Config::from_yaml(&self.startup_yaml())?))
    }

    /// Unified diff from the startup file to the running config, redacted.
    pub fn diff(&self) -> String {
        let startup = match self.startup_config() {
            Ok(c) => c.to_yaml(),
            Err(e) => format!("# startup file does not load: {e:#}\n"),
        };
        let running = self.redacted_config().to_yaml();
        similar::TextDiff::from_lines(&startup, &running)
            .unified_diff()
            .header("startup", "running")
            .to_string()
    }

    /// Writes the running config to the startup file.
    pub fn save(&self) -> Result<()> {
        let yaml = self.running_yaml();
        if self.startup_path.exists() {
            let _ = std::fs::copy(
                &self.startup_path,
                self.startup_path.with_extension("yaml.bak"),
            );
        }
        if let Some(dir) = self.startup_path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let header = "# zapper startup config, written by `zapper config save`.\n";
        write_private(&self.startup_path, format!("{header}{yaml}").as_bytes())?;
        self.emit(
            None,
            EventBody::Config {
                change: format!("saved to {}", self.startup_path.display()),
            },
        );
        Ok(())
    }

    /// Replaces the running config with the startup file.
    pub fn reload(&self) -> Result<()> {
        let config = Config::from_yaml(&self.startup_yaml())?;
        self.change("reloaded from startup", |c| {
            *c = config;
            Ok(())
        })
    }

    /// Waits for an operator's answer to an `ask`; denies on timeout.
    pub async fn ask(
        &self,
        session: &Session,
        kind: Kind,
        subject: String,
        suggestion: AnyRule,
    ) -> bool {
        let id = random_id(8);
        let (tx, rx) = oneshot::channel();
        let pending = Pending {
            id: id.clone(),
            session: session.id.clone(),
            kind,
            subject: subject.clone(),
            created: now(),
            suggestion,
            tx: Arc::new(Mutex::new(Some(tx))),
        };
        // A session cannot flood the operator (the store caps how many it may
        // have outstanding).
        if !self.approvals.register(pending) {
            return false;
        }
        self.emit(
            Some(session),
            EventBody::Ask {
                id: id.clone(),
                kind,
                subject,
            },
        );
        let timeout = Duration::from_secs(self.config.read().unwrap().daemon.ask_timeout_secs);
        let allow = tokio::time::timeout(timeout, rx)
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or(false);
        if self.approvals.take(&id).is_some() {
            self.emit(
                Some(session),
                EventBody::Answer {
                    id,
                    allow: false,
                    remember: Some("timeout".into()),
                },
            );
        }
        allow
    }

    pub fn approvals(&self) -> Vec<Pending> {
        self.approvals.list()
    }

    pub fn answer(&self, id: &str, allow: bool, remember: Option<Remember>) -> Result<()> {
        let pending = self
            .approvals
            .take(id)
            .ok_or_else(|| anyhow!("no pending approval {id}"))?;
        let session = self.session(&pending.session)?;
        if let Some(r) = remember {
            let mut rule = pending.suggestion.clone();
            let action = if allow { Action::Allow } else { Action::Deny };
            match &mut rule {
                AnyRule::Net(n) => n.action = action,
                AnyRule::Exec(e) => e.action = action,
                AnyRule::Fs(_) => {}
            }
            match r {
                Remember::Session => {
                    session.add_rule(rule, Some(0));
                }
            }
        }
        if let Some(tx) = pending.tx.lock().unwrap().take() {
            let _ = tx.send(allow);
        }
        let remember = remember.map(|r| format!("{r:?}").to_lowercase());
        self.emit(
            Some(&session),
            EventBody::Answer {
                id: id.into(),
                allow,
                remember,
            },
        );
        Ok(())
    }
}

pub const REDACTED: &str = "<redacted>";

pub fn redact(mut c: Config) -> Config {
    for s in c.secrets.values_mut() {
        if let SecretSource::Value { value } = &mut s.source {
            *value = REDACTED.into();
        }
    }
    c
}

#[cfg(test)]
mod limits_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacted_config_hides_inline_secret_values() {
        let c = Config::from_yaml("secrets:\n  api: { value: s3cr3t-inline, hosts: [api.example.com] }\n  tok: { file: ~/.config/zapper/claude-token, hosts: [api.anthropic.com] }\n").unwrap();
        let yaml = redact(c).to_yaml();
        assert!(!yaml.contains("s3cr3t-inline"), "{yaml}");
        assert!(yaml.contains(REDACTED));
        assert!(
            yaml.contains("claude-token"),
            "file sources are kept: they are paths, not values"
        );
    }
}
