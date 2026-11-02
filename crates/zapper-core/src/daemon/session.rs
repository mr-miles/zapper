//! One sandbox: a microVM, its guest channel, policy layer and audit trail.
//! The session's state and queries live here; the rest is in a module per
//! job: making it (`create`), running and stopping it (`run`), its VM
//! (`launch`), the guest's control channel (`control`), exec decisions
//! (`exec`), its disks (`disks`), what it keeps (`persist`), what an earlier
//! daemon left (`adopt`), its terminals (`terminal`), and its store,
//! directory, rules and runtime.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};

use anyhow::{bail, Result};
use serde::Serialize;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use zapper_policy::proto::{Forward, Frame, GuestFile, ShellRequest, SHELL_PORT};
use zapper_policy::{AnyRule, Config, Policy, ResolvedProfile, Scope};

use super::{tls, Daemon, Event, EventBody};

mod adopt;
mod control;
mod create;
mod dir;
pub(crate) mod disks;
mod exec;
mod launch;
mod persist;
mod rules;
mod run;
mod runtime;
mod spec;
mod store;
mod terminal;
#[cfg(test)]
mod tests;

pub(crate) use dir::SessionDir;
pub(crate) use rules::Rules;
pub use runtime::Status;
pub(crate) use runtime::{GuestRuntime, MAX_GUEST_CONNECTIONS};
pub(crate) use store::SessionStore;

pub(crate) use adopt::restore;
#[cfg(any(test, feature = "fuzz"))]
pub(crate) use control::read_guest_line;
pub(crate) use create::create;
pub(crate) use create::has_room;
#[cfg(any(test, feature = "test-util"))]
pub(crate) use create::register;
pub(crate) use run::{start, stop};
pub(crate) use terminal::pump_shell;
pub use terminal::{read_frame, TermChunk, Terminal, TerminalInfo};

/// Records a `session` event of `s`: its new `state`, and why.
fn session_event(daemon: &Daemon, s: &Session, state: &str, detail: impl Into<String>) {
    daemon.emit(
        Some(s),
        EventBody::Session {
            state: state.into(),
            detail: Some(detail.into()),
        },
    );
}

/// `detail`, and a warning after it if there is one.
fn with_warning(detail: String, warning: &Option<String>) -> String {
    match warning {
        Some(w) => format!("{detail}; {w}"),
        None => detail,
    }
}

/// Opens the session's vsock at `path` and asks the guest for `port`,
/// returning the stream once the guest answers `OK `. A connect/write failure
/// propagates; a refusal is reported as `refused: <the guest's line>`.
async fn connect_vsock(path: PathBuf, port: u32, refused: &str) -> Result<UnixStream> {
    let mut stream = UnixStream::connect(&path).await?;
    stream
        .write_all(format!("CONNECT {port}\n").as_bytes())
        .await?;
    let line = super::read_line_unbuffered(&mut stream, 64)
        .await?
        .unwrap_or_default();
    if !line.starts_with(b"OK ") {
        bail!("{refused}: {}", String::from_utf8_lossy(&line));
    }
    Ok(stream)
}

/// A terminal channel opened to the guest (`Session::open_shell`): the
/// stream, and the request as it was sent, its terminal id given and its
/// command filled in; and who opened it, for its events.
pub struct ShellOpened {
    pub stream: UnixStream,
    pub request: ShellRequest,
    /// The client that opened it, as it named itself (`Session::open_shell`
    /// leaves it unset).
    pub client: Option<String>,
    /// Opened through the SSH gateway (`Session::open_ssh_shell`): closed
    /// if the session's SSH is turned off.
    pub ssh: bool,
}

#[derive(Default, Serialize)]
pub struct Counters {
    pub net_allowed: AtomicU64,
    pub net_denied: AtomicU64,
    pub exec_allowed: AtomicU64,
    pub exec_denied: AtomicU64,
    pub exits: AtomicU64,
    pub shells: AtomicU64,
}

pub struct Session {
    pub id: String,
    pub(crate) profile: String,
    pub(crate) created: f64,
    /// When its `ttl_secs` ends this run: from its start, as it may be
    /// started again (`persistent`).
    expires: Mutex<Option<f64>>,
    /// Its disks stay when it stops, and it can be started again; they go
    /// when it is forgotten (`zapper rm`).
    pub(crate) persistent: bool,
    /// How many times it has booted: a run's timers end with it.
    boots: AtomicU64,
    /// Held across a persistent session's save, from the snapshot of its
    /// rules to the write, so an older snapshot can't be written last.
    saving: Mutex<()>,
    /// Held by a stop through its teardown, and by a start until it has
    /// checked the disks: neither runs while the other is on the disks.
    lifecycle: tokio::sync::Mutex<()>,
    /// Make a persistent session from its image even if the image changed
    /// since it was built (recorded); not kept.
    accept_changed_image: bool,
    pub resolved: ResolvedProfile,
    pub(crate) workdir: PathBuf,
    /// The session's rules: its own layer and any shared layers. Not saved.
    rules: Rules,
    /// Secrets only this session's rules can use: a credential scoped to
    /// one agent, which a client puts with `put_secret` (no client does
    /// today; it is a requirement, so keep it). Not in the config, so
    /// `config save` never writes them and no other session can name them.
    /// Not saved: a persistent session's are put again after a restart.
    pub(crate) secrets: RwLock<BTreeMap<String, zapper_policy::Secret>>,
    /// The live VM, its control channel and lifecycle status while it runs.
    pub(crate) runtime: GuestRuntime,
    pub(crate) dir: SessionDir,
    /// The session's CA and the guest-facing server configs minted from it.
    pub(crate) tls: tls::SessionTls,
    pub(crate) counters: Counters,
    usage: Mutex<Option<zapper_policy::proto::GuestEvent>>,
    /// The session's resolver state: synthetic names, rate limit, lookup cap.
    pub(crate) dns: super::dns::SessionDns,
    /// The session's HTTP traffic, for inspecting it from the UI.
    pub(crate) requests: Arc<Mutex<super::httplog::RequestLog>>,
    /// The session's terminals, for watching them from the UI.
    terminals: Mutex<std::collections::BTreeMap<u64, Arc<Terminal>>>,
    audit: Mutex<Option<super::record::Writer>>,
    /// What the session is to whoever started it, e.g. `{firm: acme,
    /// member: pat}`: shown in its info and events, and how a client finds
    /// its own sessions.
    pub labels: BTreeMap<String, String>,
    /// The driver, for a driven agent (`agent.driven`) outside a firm.
    pub driver: Option<Arc<super::driver::Driver>>,
    /// Its daemon, for the events of what clients change in it.
    daemon: Weak<Daemon>,
    extra_files: Vec<GuestFile>,
    forwards: Vec<Forward>,
    /// Whether the SSH gateway may open terminals in it: turned on and off
    /// while it runs (`set_ssh`); its SSH terminals watch it, to close when
    /// it goes off.
    ssh: tokio::sync::watch::Sender<bool>,
    /// The persistent terminal SSH shells attach to, while its program runs:
    /// one per session, the agent, kept through dropped connections.
    ssh_terminal: Mutex<Option<u64>>,
    /// Held while an SSH shell is opened, so two at once don't start two.
    ssh_opening: tokio::sync::Mutex<()>,
}

/// A session as the API reports it. Its serialised shape is the API
/// contract, so the type lives in `zapper-api-types` (the clients
/// deserialise it); the core builds it in [`Session::info`].
pub use zapper_api_types::SessionInfo;

impl Session {
    pub(crate) fn info(&self) -> SessionInfo {
        SessionInfo {
            id: self.id.clone(),
            profile: self.profile.clone(),
            status: self.runtime.current(),
            created: self.created,
            expires: *self.expires.lock().unwrap(),
            persistent: self.persistent,
            ssh: self.ssh_enabled(),
            disk_bytes: self.persistent.then(|| self.dir.disk_bytes()),
            image: self.resolved.engine.image.clone(),
            vcpus: self.resolved.engine.vcpus,
            memory_mib: self.resolved.engine.memory_mib,
            kernel: self.resolved.engine.kernel.clone(),
            network: self.resolved.engine.network,
            user: self.resolved.agent.user.clone(),
            root: self.resolved.agent.root,
            workdir: self.workdir.display().to_string(),
            counters: serde_json::to_value(&self.counters).unwrap_or_default(),
            usage: self.usage.lock().unwrap().clone(),
            rules: self.rules.list().len(),
            firm: self.label("firm").map(str::to_string),
            member: self.label("member").map(str::to_string),
            labels: self.labels.clone(),
            driver: self.driver.as_ref().map(|d| d.info()).unwrap_or_default(),
        }
    }

    /// The socket of the session's VM's vsock, for connecting to a port in
    /// the guest (`CONNECT <port>`).
    pub(crate) fn vsock_path(&self) -> PathBuf {
        self.dir.vsock()
    }

    /// Opens a host-initiated connection to a port of the session's VM,
    /// over vsock.
    pub async fn connect_port(&self, port: u32) -> Result<UnixStream> {
        connect_vsock(
            self.vsock_path(),
            port,
            &format!("the VM refused port {port}"),
        )
        .await
    }

    /// One of the session's labels.
    pub fn label(&self, key: &str) -> Option<&str> {
        self.labels.get(key).map(String::as_str)
    }

    pub fn is_running(&self) -> bool {
        matches!(self.runtime.current(), Status::Running)
    }

    /// Whether the session has ended, stopped or failed.
    pub fn ended(&self) -> bool {
        !self.runtime.current().is_live()
    }

    /// The session's HTTP requests and connections, oldest first.
    pub fn requests(&self) -> Vec<serde_json::Value> {
        self.requests.lock().unwrap().list()
    }

    /// One request with its headers and bodies.
    pub fn request(&self, id: u64) -> Option<serde_json::Value> {
        self.requests.lock().unwrap().get(id)
    }

    pub fn terminals(&self) -> Vec<TerminalInfo> {
        self.terminals
            .lock()
            .unwrap()
            .values()
            .map(|t| t.info())
            .collect()
    }

    pub fn terminal(&self, id: u64) -> Option<Arc<Terminal>> {
        self.terminals.lock().unwrap().get(&id).cloned()
    }

    pub(crate) fn record(&self, event: &Event) {
        if let Some(f) = self.audit.lock().unwrap().as_mut() {
            let line = serde_json::to_vec(event).unwrap_or_default();
            let _ = f.line(&line);
        }
    }

    /// Where the audit log can be read (the keeper's copy, if it keeps it).
    pub fn audit_path(&self) -> PathBuf {
        super::record::view(&self.dir.audit())
    }

    /// Policy layers, most specific first. Uses the running config, so rule
    /// changes apply to running sessions at once; a profile removed from the
    /// running config falls back to its layers at session start.
    pub(crate) fn layers(&self, config: &Config) -> Vec<(Scope, Policy)> {
        self.rules.layers(&self.profile, &self.resolved, config)
    }

    /// A clone of the session's own policy layer.
    pub(crate) fn own_policy(&self) -> Policy {
        self.rules.own()
    }

    /// `f` over the session's layers as the running config has them now, by
    /// reference in evaluation order, as the policy evaluators
    /// (`evaluate_net`, `evaluate_exec`, `needs_inspection`, `resolvable`)
    /// take them: the config read, the layers cloned out from behind their
    /// locks, then `f`, with no lock held.
    pub(crate) fn with_layers<T>(
        &self,
        daemon: &Daemon,
        f: impl FnOnce(&[(Scope, &Policy)]) -> T,
    ) -> T {
        let layers = self.layers(&daemon.config.read().unwrap());
        let layers: Vec<(Scope, &Policy)> = layers.iter().map(|(s, p)| (s.clone(), p)).collect();
        f(&layers)
    }

    /// `req` as this session asks it: with its ID and labels, for
    /// `{session}` and `{label:KEY}` in rules' paths.
    pub fn caller_of(&self, req: &zapper_policy::NetRequest) -> zapper_policy::NetRequest {
        zapper_policy::NetRequest {
            session: Some(self.id.clone()),
            labels: self.labels.clone(),
            ..req.clone()
        }
    }

    /// Adds a rule to the session layer under a fresh `s-<session>-<kind>-<n>`
    /// id, so session rule ids clash neither with config ones nor with
    /// another session's.
    pub(crate) fn add_rule(&self, rule: AnyRule, position: Option<usize>) -> AnyRule {
        let rule = self.rules.add(rule, position);
        self.rules_changed();
        self.rule_event("added", &rule);
        rule
    }

    /// A `rule` event of the session's, for a rule of its own layer.
    fn rule_event(&self, state: &str, rule: &AnyRule) {
        if let Some(d) = self.daemon.upgrade() {
            d.emit(
                Some(self),
                EventBody::Rule {
                    state: state.into(),
                    id: rule.id().unwrap_or("?").to_string(),
                    scope: Scope::Session(self.id.clone()).to_string(),
                    kind: rule.kind().to_string(),
                },
            );
        }
    }

    /// A persistent session's rules are kept, for a daemon that adopts it.
    fn rules_changed(&self) {
        if self.persistent {
            if let Err(e) = persist::save(self) {
                tracing::warn!(session = %self.id, "keeping its rules: {e:#}");
            }
        }
    }

    /// The rules of the session's own layer.
    pub fn rules(&self) -> Vec<AnyRule> {
        self.rules.list()
    }

    /// Puts a client's rule first in the session's own layer under the id
    /// it has, replacing a rule with that id (an officer's approval, the
    /// network a credential grant needs). It is in the session's events.
    pub fn put_rule(&self, rule: AnyRule) {
        self.rules.put(rule.clone());
        self.rules_changed();
        self.rule_event("added", &rule);
    }

    /// Removes a rule from the session's own layer: it, if it was there,
    /// and an event of the session's.
    pub fn remove_rule(&self, id: &str) -> Option<AnyRule> {
        let rule = self.rules.remove(id)?;
        self.rules_changed();
        self.rule_event("removed", &rule);
        Some(rule)
    }

    /// Registers a `proxy` secret for this session's rules alone: the value
    /// stays on the host and goes only to the secret's `hosts`, in requests
    /// a rule of this session lists it for. Replaces a session secret of the
    /// same name. A name the config already has is refused, so a session can
    /// never stand in for a global secret.
    pub fn put_secret(&self, name: &str, secret: zapper_policy::Secret) -> Result<()> {
        let valid = !name.is_empty()
            && name.len() <= 64
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if !valid {
            bail!("secret name {name:?}: lowercase letters, digits and dashes");
        }
        if secret.mode != zapper_policy::SecretMode::Proxy || secret.hosts.is_empty() {
            bail!("a session secret is a proxy secret with hosts it may be sent to");
        }
        if let Some(d) = self.daemon.upgrade() {
            if d.config.read().unwrap().secrets.contains_key(name) {
                bail!("secret {name} is in the config; a session secret can't take its name");
            }
        }
        let old = self
            .secrets
            .write()
            .unwrap()
            .insert(name.to_string(), secret.clone());
        // A replaced command is not run again for its old value.
        if let Some(spec) = old
            .filter(|o| o.source != secret.source)
            .and_then(|o| o.source.mint_spec())
        {
            crate::mint::forget(&spec);
        }
        self.config_event(format!("secret {name} set for session {}", self.id));
        Ok(())
    }

    /// Removes a session secret, and what was minted for it.
    pub fn remove_secret(&self, name: &str) -> bool {
        let Some(old) = self.secrets.write().unwrap().remove(name) else {
            return false;
        };
        if let Some(spec) = old.source.mint_spec() {
            crate::mint::forget(&spec);
        }
        self.config_event(format!("secret {name} removed from session {}", self.id));
        true
    }

    /// The names of the session's own secrets (never their values).
    pub fn secret_names(&self) -> Vec<String> {
        self.secrets.read().unwrap().keys().cloned().collect()
    }

    fn config_event(&self, change: String) {
        if let Some(d) = self.daemon.upgrade() {
            d.emit(Some(self), EventBody::Config { change });
        }
    }

    /// Opens a terminal in the guest.
    pub async fn open_shell(&self, mut req: ShellRequest) -> Result<ShellOpened> {
        if !self.is_running() {
            bail!("session {} is not running", self.id);
        }
        if req.persist && req.tty.is_none() {
            bail!("only a terminal (tty) can be persistent");
        }
        let id = match req.attach {
            // A persistent terminal of this session, still running.
            Some(n) => {
                let open = self
                    .terminals
                    .lock()
                    .unwrap()
                    .get(&n)
                    .is_some_and(|t| t.persistent && t.info().open);
                if !open {
                    bail!("session {} has no persistent terminal {n} open", self.id);
                }
                n
            }
            None => {
                if req.argv.is_empty() {
                    req.argv = self.resolved.agent.command.clone();
                }
                self.runtime.next_shell()
            }
        };
        req.id = id;
        let mut stream =
            connect_vsock(self.vsock_path(), SHELL_PORT, "guest refused terminal").await?;
        let mut json = serde_json::to_vec(&req)?;
        json.push(b'\n');
        stream.write_all(&json).await?;
        self.counters.shells.fetch_add(1, Ordering::Relaxed);
        Ok(ShellOpened {
            stream,
            request: req,
            client: None,
            ssh: false,
        })
    }

    /// What the session was made with besides its profile, for tests: its
    /// extra files and forwards, and its mounts and agent as resolved.
    #[cfg(any(test, feature = "test-util"))]
    pub fn made_with(&self) -> serde_json::Value {
        serde_json::json!({
            "files": self.extra_files,
            "forwards": self.forwards,
            "mounts": self.resolved.mounts,
            "agent": self.resolved.agent,
        })
    }

    /// Whether the SSH gateway may open terminals in the session.
    pub fn ssh_enabled(&self) -> bool {
        *self.ssh.borrow()
    }

    /// Turns the session's SSH on or off, recorded; off also closes the
    /// terminals the gateway has open (a persistent one is detached, and
    /// keeps running). A persistent session keeps the setting.
    pub fn set_ssh(&self, enabled: bool) -> Result<()> {
        if self.ssh.send_replace(enabled) == enabled {
            return Ok(());
        }
        if let Some(d) = self.daemon.upgrade() {
            d.emit(
                Some(self),
                EventBody::Ssh {
                    state: if enabled { "enabled" } else { "disabled" }.into(),
                    client: None,
                    reason: None,
                },
            );
        }
        if self.persistent {
            persist::save(self)?;
        }
        Ok(())
    }

    /// Opens a terminal for `client` through the SSH gateway, as
    /// `open_shell`, if the session's SSH is on; refused and recorded if not.
    pub async fn open_ssh_shell(
        &self,
        req: ShellRequest,
        client: Option<String>,
    ) -> Result<ShellOpened> {
        if !self.ssh_enabled() {
            if let Some(d) = self.daemon.upgrade() {
                d.emit(
                    Some(self),
                    EventBody::Ssh {
                        state: "refused".into(),
                        client: client.clone(),
                        reason: Some("SSH is off for this session".into()),
                    },
                );
            }
            return Err(crate::errors::forbidden(format!(
                "SSH is off for session {} (`zapper ssh on {}`)",
                self.id, self.id
            )));
        }
        // A shell (a terminal running the profile's command) is the
        // session's one SSH terminal: attached to again while its program
        // runs, taking over from a connection attached already; started,
        // persistent, when there is none. Anything else is a terminal of its
        // own, ended with its connection.
        if req.tty.is_none() || !req.argv.is_empty() {
            let mut opened = self.open_shell(req).await?;
            opened.client = client;
            opened.ssh = true;
            return Ok(opened);
        }
        let _one_at_a_time = self.ssh_opening.lock().await;
        let held = self.ssh_terminal();
        let running = held.filter(|n| {
            self.terminals
                .lock()
                .unwrap()
                .get(n)
                .is_some_and(|t| t.persistent && t.info().open)
        });
        let req = match running {
            Some(n) => ShellRequest {
                tty: req.tty,
                ..ShellRequest::attach(n)
            },
            None => ShellRequest {
                persist: true,
                ..req
            },
        };
        let mut opened = self.open_shell(req).await?;
        if held != Some(opened.request.id) {
            *self.ssh_terminal.lock().unwrap() = Some(opened.request.id);
            if self.persistent {
                persist::save(self)?;
            }
        }
        opened.client = client;
        opened.ssh = true;
        Ok(opened)
    }

    /// The session's SSH terminal's number, if it has had one.
    pub(crate) fn ssh_terminal(&self) -> Option<u64> {
        *self.ssh_terminal.lock().unwrap()
    }

    /// Ends persistent terminal `n` on purpose: its program gets `SIGHUP`,
    /// and a client attached to it is closed. Its exit code, if it said one
    /// in time.
    pub async fn hangup(&self, n: u64) -> Result<Option<i32>> {
        let req = ShellRequest::attach(n);
        let mut stream = self.open_shell(req).await?.stream;
        stream.write_all(&Frame::Hangup.encode()).await?;
        let code = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while let Ok(Some(frame)) = terminal::read_frame(&mut stream).await {
                if let Frame::Exit(c) = frame {
                    return Some(c);
                }
            }
            None
        })
        .await
        .ok()
        .flatten();
        if let Some(t) = self.terminals.lock().unwrap().get(&n) {
            t.end(code);
        }
        Ok(code)
    }
}

/// What the core needs to create a session. Shared with the API's clients
/// (The Office builds one and passes it to `create_session`), so it lives in
/// `zapper-api-types`; the core re-exports it.
pub use zapper_api_types::NewSession;

/// What a persistent session is, kept in its `session.json`: enough to list
/// it again after the daemon restarts and to start it from its disks. Its
/// own secrets aren't kept (`Session::secrets`): put them again.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub(crate) struct Saved {
    pub id: String,
    pub profile: String,
    pub created: f64,
    pub workdir: PathBuf,
    pub resolved: ResolvedProfile,
    pub labels: BTreeMap<String, String>,
    /// Its own layer's rules, as of their last change (saved on each).
    pub rules: Policy,
    pub files: Vec<GuestFile>,
    pub forwards: Vec<Forward>,
    pub driven: bool,
    /// SSH on (`Session::set_ssh`); off for one saved before there was SSH.
    #[serde(default)]
    pub ssh: bool,
    /// The persistent terminal SSH shells attach to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_terminal: Option<u64>,
}
