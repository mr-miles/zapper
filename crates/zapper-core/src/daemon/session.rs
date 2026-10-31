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
use zapper_policy::proto::{Forward, Frame, GuestFile, HostMsg, ShellRequest, SHELL_PORT};
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
pub(crate) use runtime::GuestRuntime;
pub use runtime::Status;
pub(crate) use store::SessionStore;

pub(crate) use adopt::restore;
#[cfg(any(test, feature = "fuzz"))]
pub(crate) use control::read_guest_line;
pub(crate) use create::create;
#[cfg(any(test, feature = "test-util"))]
pub(crate) use create::register;
pub(crate) use run::{start, stop};
pub(crate) use terminal::pump_shell;
pub use terminal::{read_frame, TermChunk, Terminal, TerminalInfo};

/// Opens the session's vsock at `path` and asks the guest for `port`,
/// returning the stream once the guest answers `OK `. A connect/write failure
/// propagates; a refusal is reported as `refused: <the guest's line>`.
async fn connect_vsock(path: PathBuf, port: &str, refused: &str) -> Result<UnixStream> {
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
    pub profile: String,
    pub created: f64,
    /// When its `ttl_secs` ends this run: from its start, as it may be
    /// started again (`persistent`).
    expires: Mutex<Option<f64>>,
    /// Its disks stay when it stops, and it can be started again; they go
    /// when it is forgotten (`zapper rm`).
    pub persistent: bool,
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
    pub(crate) accept_changed_image: bool,
    pub resolved: ResolvedProfile,
    pub workdir: PathBuf,
    /// The session's rules: its own layer and any shared layers. Not saved.
    pub(crate) rules: Rules,
    /// Secrets only this session's rules can use (a firm worker's GitHub
    /// token). Not in the config, so `config save` never writes them and no
    /// other session can name them. Not saved.
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
}

#[derive(Serialize)]
pub struct SessionInfo {
    pub id: String,
    pub profile: String,
    #[serde(flatten)]
    pub status: Status,
    pub created: f64,
    pub expires: Option<f64>,
    pub persistent: bool,
    /// For a persistent session: what its disks take on the host.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk_bytes: Option<u64>,
    pub image: String,
    pub vcpus: u8,
    pub memory_mib: u32,
    pub kernel: String,
    /// `nic` or `vsock`.
    pub network: zapper_policy::NetworkMode,
    pub user: String,
    pub root: bool,
    pub workdir: String,
    pub counters: serde_json::Value,
    pub usage: Option<zapper_policy::proto::GuestEvent>,
    pub rules: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub firm: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub member: Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    /// The driver's state for a driven session; `"disabled"` with the
    /// driver off (`daemon.driver: false`).
    #[serde(skip_serializing_if = "serde_json::Value::is_null")]
    pub driver: serde_json::Value,
}

impl Session {
    pub fn info(&self) -> SessionInfo {
        let c = &self.counters;
        let get = |a: &AtomicU64| a.load(Ordering::Relaxed);
        SessionInfo {
            id: self.id.clone(),
            profile: self.profile.clone(),
            status: self.runtime.current(),
            created: self.created,
            expires: *self.expires.lock().unwrap(),
            persistent: self.persistent,
            disk_bytes: self.persistent.then(|| self.dir.disk_bytes()),
            image: self.resolved.engine.image.clone(),
            vcpus: self.resolved.engine.vcpus,
            memory_mib: self.resolved.engine.memory_mib,
            kernel: self.resolved.engine.kernel.clone(),
            network: self.resolved.engine.network,
            user: self.resolved.agent.user.clone(),
            root: self.resolved.agent.root,
            workdir: self.workdir.display().to_string(),
            counters: serde_json::json!({
                "net_allowed": get(&c.net_allowed), "net_denied": get(&c.net_denied),
                "exec_allowed": get(&c.exec_allowed), "exec_denied": get(&c.exec_denied),
                "exits": get(&c.exits), "shells": get(&c.shells),
            }),
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
    pub fn vsock_path(&self) -> PathBuf {
        self.dir.vsock()
    }

    /// Opens a host-initiated connection to a port of the session's VM,
    /// over vsock.
    pub async fn connect_port(&self, port: u32) -> Result<UnixStream> {
        connect_vsock(
            self.vsock_path(),
            &port.to_string(),
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
        matches!(
            self.runtime.current(),
            Status::Stopped(_) | Status::Failed(_)
        )
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

    pub fn record(&self, event: &Event) {
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
    pub fn layers(&self, config: &Config) -> Vec<(Scope, Policy)> {
        self.rules
            .layers(&self.id, &self.profile, &self.resolved, config)
    }

    /// A clone of the session's own policy layer.
    pub fn own_policy(&self) -> Policy {
        self.rules.own()
    }

    /// The same layers by reference, in evaluation order, as the policy
    /// evaluators (`evaluate_net`, `evaluate_exec`, `needs_inspection`,
    /// `resolvable`) take them. Paired with [`Session::layers`], which owns
    /// them — the policies are cloned out from behind their locks, so the
    /// borrowed view is a separate step.
    pub(crate) fn layers_of(layers: &[(Scope, Policy)]) -> Vec<(Scope, &Policy)> {
        layers.iter().map(|(s, p)| (s.clone(), p)).collect()
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

    /// Adds a rule to the session layer under a fresh `s-<kind>-<n>` id, so
    /// session rule ids never clash with config ones.
    pub fn add_rule(&self, rule: AnyRule, position: Option<usize>) -> AnyRule {
        let rule = self.rules.add(rule, position);
        self.rules_changed();
        rule
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
        let id = self.rules.put(rule);
        self.rules_changed();
        self.config_event(format!(
            "rule {id} added to {}",
            Scope::Session(self.id.clone())
        ));
    }

    /// Removes a rule from the session's own layer: it, if it was there,
    /// and an event of the session's.
    pub fn remove_rule(&self, id: &str) -> Option<AnyRule> {
        let rule = self.rules.remove(id)?;
        self.rules_changed();
        self.config_event(format!(
            "rule {id} removed from {}",
            Scope::Session(self.id.clone())
        ));
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

    fn send(&self, msg: HostMsg) {
        self.runtime.send(msg);
    }

    /// Opens a terminal in the guest.
    pub async fn open_shell(&self, mut req: ShellRequest) -> Result<(UnixStream, u64)> {
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
        let mut stream = connect_vsock(
            self.dir.vsock(),
            &SHELL_PORT.to_string(),
            "guest refused terminal",
        )
        .await?;
        let mut json = serde_json::to_vec(&req)?;
        json.push(b'\n');
        stream.write_all(&json).await?;
        self.counters.shells.fetch_add(1, Ordering::Relaxed);
        Ok((stream, id))
    }
}

impl Session {
    /// Ends persistent terminal `n` on purpose: its program gets `SIGHUP`,
    /// and a client attached to it is closed. Its exit code, if it said one
    /// in time.
    pub async fn hangup(&self, n: u64) -> Result<Option<i32>> {
        let req = ShellRequest::attach(n);
        let (mut stream, _) = self.open_shell(req).await?;
        let mut buf = Vec::new();
        Frame::Hangup.write_to(&mut buf)?;
        stream.write_all(&buf).await?;
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

pub struct NewSession {
    pub profile: String,
    pub workdir: PathBuf,
    pub policy: Policy,
    /// The session's labels (`Session::labels`).
    pub labels: BTreeMap<String, String>,
    /// Policy layers shared with other sessions (`Session::shared_layers`).
    pub shared_layers: Vec<(Scope, Arc<RwLock<Policy>>)>,
    /// Files written in the guest besides the profile's.
    pub files: Vec<GuestFile>,
    /// Disks besides the profile's mounts.
    pub mounts: Vec<zapper_policy::Mount>,
    /// Merged over the profile's `agent`.
    pub agent_overlay: Option<serde_json::Value>,
    pub forwards: Vec<Forward>,
    /// Keep its disks when it stops, so it can be started again
    /// (`Session::persistent`).
    pub persistent: bool,
    /// For a persistent session: make it from its image even if the image
    /// changed since it was built, which is recorded.
    pub accept_changed_image: bool,
}

impl NewSession {
    pub fn plain(profile: String, workdir: PathBuf, policy: Policy) -> Self {
        NewSession {
            profile,
            workdir,
            policy,
            labels: BTreeMap::new(),
            shared_layers: vec![],
            files: vec![],
            mounts: vec![],
            agent_overlay: None,
            forwards: vec![],
            persistent: false,
            accept_changed_image: false,
        }
    }
}

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
    /// Its own layer's rules, as of its last stop.
    pub rules: Policy,
    pub files: Vec<GuestFile>,
    pub forwards: Vec<Forward>,
    pub driven: bool,
}
