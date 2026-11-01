//! The zapper daemon: sessions, policy, the egress proxy, DNS, services, the
//! MCP gateway, approvals, the driver and the record (its API and UI are
//! zapper-api's, over `Core`).
//!
//! Configuration works like a router's: changes through the API apply to the
//! running config at once; `POST /v1/config/save` writes it to the startup
//! file, `POST /v1/config/reload` throws the running config away for the
//! startup file, and `GET /v1/config/diff` shows what save would change.

pub(crate) mod addr;
pub(crate) mod api_error;
pub(crate) mod approvals;
pub(crate) mod builds;
pub(crate) mod clients;
pub(crate) mod dbwire;
pub(crate) mod deciders;
pub(crate) mod dns;
pub mod driver;
pub(crate) mod events;
pub(crate) mod gateway;
pub use zapper_driver::harness;
pub(crate) mod httplog;
pub(crate) mod nic;
pub(crate) mod proxy;
pub mod record;
pub(crate) mod registry;
pub(crate) mod secrets;
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
pub use events::{Catchup, Cursor, Event, EventBody};

// Generic helpers live in `crate::util` (the clients' shared helpers);
// re-exported here so the daemon's modules reach them as `super::now` etc.
pub(crate) use crate::util::{is_session_id, now, random_id, write_private};

pub struct Paths {
    pub(crate) state: PathBuf,
    pub(crate) images: PathBuf,
    pub(crate) kernels: PathBuf,
    pub(crate) firecracker: PathBuf,
    pub(crate) initrd: PathBuf,
}

impl Paths {
    /// Session `id`'s directory in the state.
    pub(crate) fn session_dir(&self, id: &str) -> session::SessionDir {
        session::SessionDir::new(self.state.join("sessions").join(id))
    }
}

// `Daemon` is never re-exported — `Core` (interface.rs) is the only public
// handle — so its fields are `pub(crate)`, not `pub`.
pub struct Daemon {
    pub(crate) config: RwLock<Config>,
    /// Held across a change, from the clone to the swap, so two at once
    /// can't each start from the config before the other.
    changing: Mutex<()>,
    pub(crate) startup_path: PathBuf,
    pub(crate) paths: Paths,
    pub(crate) addrs: RwLock<addr::AddressPolicy>,
    pub(crate) sessions: session::SessionStore,
    pub(crate) events: events::EventBus,
    approvals: approvals::Approvals,
    pub(crate) ui_token: String,
    pub(crate) clients: clients::Clients,
    pub(crate) services: services::Services,
    pub(crate) deciders: deciders::Deciders,
    pub(crate) gateway: gateway::Gateway,
    pub(crate) builds: builds::Builds,
    /// Shared policy layers by name: a session attaches one by a label (a
    /// firm role, say), and a client adds and removes its rules, so every
    /// session with that label sees them. The office's role layers.
    pub(crate) named_layers:
        RwLock<std::collections::BTreeMap<String, Arc<RwLock<zapper_policy::Policy>>>>,
}

/// One line from `io`, read a byte at a time so nothing after it is taken
/// from the stream (what follows is another reader's): without its `\n`, or
/// `None` if it runs past `max` bytes. An error at the end of the stream.
pub(crate) async fn read_line_unbuffered(
    io: &mut (impl tokio::io::AsyncRead + Unpin),
    max: usize,
) -> std::io::Result<Option<Vec<u8>>> {
    use tokio::io::AsyncReadExt as _;
    let mut line = Vec::new();
    loop {
        match io.read_u8().await? {
            b'\n' => return Ok(Some(line)),
            _ if line.len() >= max => return Ok(None),
            b => line.push(b),
        }
    }
}

/// Accepts connections on `listener` and serves each with `handle`, running at
/// most `limit` permits' worth at once; a connection over the limit is dropped
/// rather than queued. The permit is held until the spawned task ends.
pub(crate) async fn accept_loop<F, Fut>(
    listener: tokio::net::UnixListener,
    limit: Arc<tokio::sync::Semaphore>,
    handle: F,
) where
    F: Fn(tokio::net::UnixStream) -> Fut,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            break;
        };
        let Ok(permit) = limit.clone().try_acquire_owned() else {
            continue;
        };
        let fut = handle(stream);
        tokio::spawn(async move {
            fut.await;
            drop(permit);
        });
    }
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

/// Loads the startup config (empty when the file is absent), warning if the
/// file is world-readable and might hold inline secrets.
fn load_startup_config(path: &Path) -> Result<Config> {
    let config = match read_startup(path)? {
        Some(c) => c,
        None => {
            tracing::warn!(
                "{} does not exist; starting with an empty config",
                path.display()
            );
            Config::default()
        }
    };
    if let Ok(meta) = std::fs::metadata(path) {
        if meta.permissions().mode() & 0o077 != 0 && config.has_inline_secrets() {
            tracing::warn!(
                "{} is readable by others; it may hold secrets (chmod 600)",
                path.display()
            );
        }
    }
    Ok(config)
}

/// The startup file's config: `None` if there is no file; an error for one
/// that can't be read (not taken as empty) or doesn't load.
fn read_startup(path: &Path) -> Result<Option<Config>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(anyhow::Error::new(e).context(format!("reading {}", path.display()))),
    };
    Config::from_yaml(&text)
        .with_context(|| format!("loading {}", path.display()))
        .map(Some)
}

/// Resolves and prepares the state directory: creates it (0700) with its
/// `sessions` subdirectory and the `services` one for socket services'
/// sockets, and clears the VMM directories a previous daemon left behind
/// (audit logs and recordings stay), but for persistent sessions', whose
/// disks are theirs until they are removed.
fn prepare_state_dir(state: &str) -> Result<PathBuf> {
    let state = PathBuf::from(expand_home(state));
    std::fs::create_dir_all(state.join("sessions"))?;
    std::fs::create_dir_all(state.join(services::SOCKETS_DIR))?;
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o700))?;
    for dir in std::fs::read_dir(state.join("sessions"))?.flatten() {
        let sd = session::SessionDir::new(dir.path());
        if !sd.saved().exists() {
            let _ = std::fs::remove_dir_all(sd.vm());
        }
    }
    Ok(state)
}

impl Daemon {
    pub fn new(startup_path: PathBuf) -> Result<Arc<Self>> {
        let config = load_startup_config(&startup_path)?;
        let d = &config.daemon;
        let state = prepare_state_dir(&d.state)?;
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
        let daemon = Arc::new(Daemon {
            config: RwLock::new(config),
            changing: Mutex::new(()),
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
            builds: builds::Builds::default(),
            named_layers: RwLock::new(std::collections::BTreeMap::new()),
        });
        session::restore(&daemon);
        Ok(daemon)
    }

    /// The shared policy layer named `name`, created empty if new. A session
    /// attaches it by a label (see `session::create`); a client adds and
    /// removes its rules, and every session carrying that label sees them.
    pub(crate) fn named_layer(&self, name: &str) -> Arc<RwLock<zapper_policy::Policy>> {
        self.named_layers
            .write()
            .unwrap()
            .entry(name.to_string())
            .or_default()
            .clone()
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
            .ok_or_else(|| crate::errors::not_found(format!("no session {id}")))
    }

    /// Applies a change to the running config if the result is valid.
    pub fn change<T>(&self, what: &str, f: impl FnOnce(&mut Config) -> Result<T>) -> Result<T> {
        let _changing = self.changing.lock().unwrap_or_else(|e| e.into_inner());
        let current = self.config.read().unwrap().clone();
        let before = config_rules(&current);
        let (next, out) = self.candidate(current.clone(), f)?;
        // Derived state: rebuild the address policy only when this change
        // touched the private networks (compared against the clone basis, not
        // a second lock read).
        if next.daemon.private_networks != current.daemon.private_networks {
            *self.addrs.write().unwrap() = addr::AddressPolicy::new(&next.daemon.private_networks)?;
        }
        let after = config_rules(&next);
        *self.config.write().unwrap() = next;
        self.emit(
            None,
            EventBody::Config {
                change: what.to_string(),
            },
        );
        for (state, gone, kept) in [("removed", &before, &after), ("added", &after, &before)] {
            for ((scope, id), (kind, rule)) in gone {
                if kept.get(&(scope.clone(), id.clone())).map(|(_, r)| r) != Some(rule) {
                    self.emit(
                        None,
                        EventBody::Rule {
                            state: state.into(),
                            id: id.clone(),
                            scope: scope.clone(),
                            kind: kind.clone(),
                        },
                    );
                }
            }
        }
        Ok(out)
    }

    /// `f` applied to `config` (a copy of the running one), and checked as
    /// every change is: normalised, validated as a whole, the driver toggle
    /// and clients' checks passed, its private networks parsed. Not applied.
    fn candidate<T>(
        &self,
        mut next: Config,
        f: impl FnOnce(&mut Config) -> Result<T>,
    ) -> Result<(Config, T)> {
        let out = f(&mut next)?;
        next.normalize();
        next.validate()?;
        self.ensure_driver_toggle_ok(&next)?;
        self.clients.check(&next)?;
        addr::AddressPolicy::new(&next.daemon.private_networks)?;
        Ok((next, out))
    }

    /// Turning the driver off is refused while driven sessions are running.
    fn ensure_driver_toggle_ok(&self, next: &Config) -> Result<()> {
        if next.daemon.driver {
            return Ok(());
        }
        let driven: Vec<String> = self
            .sessions
            .all()
            .into_iter()
            .filter(|s| s.driver.is_some() && s.is_running())
            .map(|s| s.id.clone())
            .collect();
        if !driven.is_empty() {
            bail!(
                "daemon.driver: driven sessions are running ({}); stop them first",
                driven.join(", ")
            );
        }
        Ok(())
    }

    /// The running config with inline secret values hidden.
    pub fn redacted_config(&self) -> Config {
        redact(self.config.read().unwrap().clone())
    }

    /// Replaces the running config; redacted inline secrets keep their values.
    /// The values come from the config it replaces, in the same change.
    pub fn replace_config(&self, mut new: Config) -> Result<()> {
        self.change("config replaced", |old| {
            keep_redacted(old, &mut new)?;
            *old = new;
            Ok(())
        })
    }

    /// What replacing the running config with `new` would change, checked
    /// as `replace_config` checks it but not applied: a unified diff of the
    /// two, values redacted (empty when nothing would).
    pub fn check_replace(&self, mut new: Config) -> Result<String> {
        let current = self.config.read().unwrap().clone();
        let (next, ()) = self.candidate(current.clone(), |old| {
            keep_redacted(old, &mut new)?;
            *old = new;
            Ok(())
        })?;
        let (was, would) = (redact(current).to_yaml(), redact(next).to_yaml());
        if was == would {
            return Ok(String::new());
        }
        Ok(similar::TextDiff::from_lines(&was, &would)
            .unified_diff()
            .header("running", "proposed")
            .to_string())
    }

    fn running_yaml(&self) -> String {
        self.config.read().unwrap().to_yaml()
    }

    /// The startup config, normalised and redacted.
    pub fn startup_config(&self) -> Result<Config> {
        Ok(redact(
            read_startup(&self.startup_path)?.unwrap_or_default(),
        ))
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

    /// Replaces the running config with the startup file, validated and
    /// applied atomically. Without a file there is nothing to reload: an
    /// error, not an empty config.
    pub fn reload(&self) -> Result<()> {
        let path = &self.startup_path;
        let config =
            read_startup(path)?.ok_or_else(|| anyhow!("no startup file at {}", path.display()))?;
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
        // Unlisted however the wait ends, also when the request waiting on
        // it is dropped (its connection gone).
        struct Unlist<'a>(&'a approvals::Approvals, String);
        impl Drop for Unlist<'_> {
            fn drop(&mut self) {
                self.0.take(&self.1);
            }
        }
        let _unlist = Unlist(&self.approvals, id.clone());
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
            self.emit_answer(session, id, false, Some("timeout"));
        }
        allow
    }

    pub fn approvals(&self) -> Vec<Pending> {
        self.approvals.list()
    }

    /// Refuses and unlists a stopped session's outstanding approvals, each
    /// answer in its events.
    pub(crate) fn drop_approvals(&self, session: &Session) {
        for p in self.approvals.take_session(&session.id) {
            self.emit_answer(session, p.id, false, Some("session stopped"));
        }
    }

    /// Records an approval's answer in its session's events: `remember`
    /// what it is remembered for, or why it was refused unasked.
    fn emit_answer(&self, session: &Session, id: String, allow: bool, remember: Option<&str>) {
        self.emit(
            Some(session),
            EventBody::Answer {
                id,
                allow,
                remember: remember.map(str::to_string),
            },
        );
    }

    pub fn answer(&self, id: &str, allow: bool, remember: Option<Remember>) -> Result<()> {
        let pending = self
            .approvals
            .take(id)
            .ok_or_else(|| crate::errors::not_found(format!("no pending approval {id}")))?;
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
        pending.fire(allow);
        self.emit_answer(&session, id.into(), allow, remember.map(Remember::as_str));
        Ok(())
    }
}

// Redacting inline secret values is pure config work, so it lives in
// `zapper-policy`; re-exported here (and through `util`) for the core's
// callers and the API clients.
pub use zapper_policy::{redact, REDACTED};

#[cfg(test)]
mod limits_tests;

/// Inline secret values `new` has redacted get the values `old` has.
fn keep_redacted(old: &Config, new: &mut Config) -> Result<()> {
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
    Ok(())
}

/// The config's rules, by layer and id: each one's kind, and the rule as it
/// is written, to tell one changed under its id.
fn config_rules(c: &Config) -> std::collections::BTreeMap<(String, String), (String, String)> {
    c.layers()
        .flat_map(|(scope, p)| {
            p.rules().into_iter().map(move |r| {
                let id = r.id().unwrap_or("?").to_string();
                let written = serde_json::to_string(&r).unwrap_or_default();
                ((scope.to_string(), id), (r.kind().to_string(), written))
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A startup file that can't be read is an error, not an empty config;
    /// without one, reloading is an error too, rather than emptying the
    /// running config.
    #[test]
    fn a_startup_file_that_cant_be_read_isnt_empty() {
        use std::os::unix::fs::PermissionsExt;
        let t = crate::testing::TestCore::new("profiles:\n  web: {}\n");
        let d = t.core.daemon();
        std::fs::write(&d.startup_path, "profiles:\n  web: {}\n").unwrap();
        std::fs::set_permissions(&d.startup_path, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read(&d.startup_path).is_err() {
            assert!(d.startup_config().is_err());
            assert!(d.reload().is_err());
            assert!(
                d.config.read().unwrap().profiles.contains_key("web"),
                "kept"
            );
        }
        std::fs::remove_file(&d.startup_path).unwrap();
        let e = d.reload().unwrap_err().to_string();
        assert!(e.contains("no startup file"), "{e}");
        assert!(
            d.config.read().unwrap().profiles.contains_key("web"),
            "kept"
        );
        assert!(d.startup_config().unwrap().profiles.is_empty());
    }

    /// A line is taken up to its `\n` and no further; one longer than the
    /// limit isn't; the stream ending before the line does is an error.
    #[tokio::test]
    async fn a_line_read_unbuffered_leaves_the_rest() {
        use tokio::io::AsyncReadExt as _;
        let mut io: &[u8] = b"OK 1\nafter";
        assert_eq!(
            read_line_unbuffered(&mut io, 4).await.unwrap(),
            Some(b"OK 1".to_vec())
        );
        let mut rest = String::new();
        io.read_to_string(&mut rest).await.unwrap();
        assert_eq!(rest, "after", "nothing after the line taken");

        let mut long: &[u8] = b"OK 12\n";
        assert_eq!(read_line_unbuffered(&mut long, 4).await.unwrap(), None);
        let mut cut: &[u8] = b"OK";
        assert!(read_line_unbuffered(&mut cut, 4).await.is_err());
    }

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
