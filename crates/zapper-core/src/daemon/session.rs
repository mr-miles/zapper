//! One sandbox: a microVM, its guest channel, policy layer and audit trail.
//! The session's state and queries live here; starting and stopping it is in
//! `lifecycle`, and its interactive terminals in `terminal`.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};

use anyhow::{bail, Result};
use serde::Serialize;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::process::Child;
use tokio::sync::{mpsc, watch, Semaphore};
use zapper_policy::proto::{Forward, GuestFile, HostMsg, ShellRequest, MAX_LINE, SHELL_PORT};
use zapper_policy::{AnyRule, Config, Policy, ResolvedProfile, Scope};

use super::{tls, vm, Daemon, Event, EventBody};

mod boot;
mod lifecycle;
mod terminal;

#[cfg(any(test, feature = "test-util"))]
pub(crate) use lifecycle::register;
pub(crate) use lifecycle::{create, stop};
pub(crate) use terminal::pump_shell;
pub use terminal::{read_frame, TermChunk, Terminal, TerminalInfo};

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "state", content = "detail", rename_all = "lowercase")]
pub enum Status {
    Starting,
    Running,
    Failed(String),
    Stopped(String),
}

/// The daemon's live sessions, by id. Behind one lock; readers take a cloned
/// snapshot so the lock is never held across other work.
pub(crate) struct SessionStore(RwLock<BTreeMap<String, Arc<Session>>>);

impl SessionStore {
    pub(crate) fn new() -> SessionStore {
        SessionStore(RwLock::new(BTreeMap::new()))
    }

    pub(crate) fn get(&self, id: &str) -> Option<Arc<Session>> {
        self.0.read().unwrap().get(id).cloned()
    }

    pub(crate) fn all(&self) -> Vec<Arc<Session>> {
        self.0.read().unwrap().values().cloned().collect()
    }

    pub(crate) fn insert(&self, session: Arc<Session>) {
        self.0.write().unwrap().insert(session.id.clone(), session);
    }

    /// How many sessions are starting or running, for the session limit.
    pub(crate) fn running(&self) -> usize {
        self.0
            .read()
            .unwrap()
            .values()
            .filter(|s| matches!(*s.status.borrow(), Status::Starting | Status::Running))
            .count()
    }

    /// Take a stopped session off the list; a starting or running one stays,
    /// as nothing would manage its VM.
    pub(crate) fn forget(&self, id: &str) -> Result<()> {
        let mut sessions = self.0.write().unwrap();
        if let Some(s) = sessions.get(id) {
            if matches!(*s.status.borrow(), Status::Starting | Status::Running) {
                bail!("session {id} is still running; stop it first");
            }
        }
        sessions.remove(id);
        Ok(())
    }
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
    pub expires: Option<f64>,
    pub resolved: ResolvedProfile,
    pub workdir: PathBuf,
    /// The session's own policy layer, above its profile. Not saved.
    pub(crate) policy: RwLock<Policy>,
    pub(crate) status: watch::Sender<Status>,
    pub dir: PathBuf,
    pub(crate) ca: tls::SessionCa,
    /// Per-host rustls server configs for inspected connections, minted from
    /// `ca` on first use (the proxy's guest-facing TLS; `tls::server_config`).
    tls_configs: Mutex<HashMap<String, Arc<rustls::ServerConfig>>>,
    pub(crate) counters: Counters,
    pub(crate) usage: Mutex<Option<zapper_policy::proto::GuestEvent>>,
    /// Names given out by the session's DNS, by synthetic address.
    pub(crate) dns: Mutex<super::dns::FakeDns>,
    pub(crate) dns_rate: Mutex<super::dns::RateLimit>,
    /// Bounds the session's concurrent lookups on the host.
    pub(crate) dns_lookups: Arc<Semaphore>,
    /// The session's HTTP traffic, for inspecting it from the UI.
    pub(crate) requests: Arc<Mutex<super::httplog::RequestLog>>,
    /// The session's terminals, for watching them from the UI.
    pub(crate) terminals: Mutex<std::collections::BTreeMap<u64, Arc<Terminal>>>,
    guest: Mutex<Option<mpsc::Sender<HostMsg>>>,
    vm: tokio::sync::Mutex<Option<Child>>,
    audit: Mutex<Option<super::record::Writer>>,
    next_shell: AtomicU64,
    /// Bounds concurrent proxy connections from the guest.
    pub(crate) conns: Arc<Semaphore>,
    exec_checks: Arc<Semaphore>,
    /// What the session is to whoever started it, e.g. `{firm: acme,
    /// member: pat}`: shown in its info and events, and how a client finds
    /// its own sessions.
    pub labels: BTreeMap<String, String>,
    /// Policy layers a client keeps for several sessions (a firm's role),
    /// between the session's own layer and its profile's.
    pub(crate) shared_layers: Vec<(Scope, Arc<RwLock<Policy>>)>,
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
    /// The rustls server config for terminating an inspected connection to
    /// `host`: a leaf minted from the session's CA and assembled into a
    /// server config on first use, then cached.
    pub(crate) fn server_config(&self, host: &str) -> Result<Arc<rustls::ServerConfig>> {
        let host = host.to_ascii_lowercase();
        if let Some(c) = self.tls_configs.lock().unwrap().get(&host) {
            return Ok(c.clone());
        }
        let config = tls::server_config(self.ca.leaf(&host)?)?;
        self.tls_configs
            .lock()
            .unwrap()
            .insert(host, config.clone());
        Ok(config)
    }

    pub fn info(&self) -> SessionInfo {
        let c = &self.counters;
        let get = |a: &AtomicU64| a.load(Ordering::Relaxed);
        SessionInfo {
            id: self.id.clone(),
            profile: self.profile.clone(),
            status: self.status.borrow().clone(),
            created: self.created,
            expires: self.expires,
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
            rules: self.policy.read().unwrap().rules().len(),
            firm: self.label("firm").map(str::to_string),
            member: self.label("member").map(str::to_string),
            labels: self.labels.clone(),
            driver: self.driver.as_ref().map(|d| d.info()).unwrap_or_default(),
        }
    }

    /// The socket of the session's VM's vsock, for connecting to a port in
    /// the guest (`CONNECT <port>`).
    pub fn vsock_path(&self) -> PathBuf {
        vm::vsock_path(&self.dir)
    }

    /// Opens a host-initiated connection to a port of the session's VM,
    /// over vsock.
    pub async fn connect_port(&self, port: u32) -> Result<UnixStream> {
        let mut stream = UnixStream::connect(self.vsock_path()).await?;
        stream
            .write_all(format!("CONNECT {port}\n").as_bytes())
            .await?;
        let mut line = Vec::new();
        let mut byte = [0u8; 1];
        while line.len() < 64 {
            stream.read_exact(&mut byte).await?;
            if byte[0] == b'\n' {
                break;
            }
            line.push(byte[0]);
        }
        if !line.starts_with(b"OK ") {
            bail!("the VM refused port {port}");
        }
        Ok(stream)
    }

    /// One of the session's labels.
    pub fn label(&self, key: &str) -> Option<&str> {
        self.labels.get(key).map(String::as_str)
    }

    pub fn is_running(&self) -> bool {
        matches!(*self.status.borrow(), Status::Running)
    }

    /// Whether the session has ended, stopped or failed.
    pub fn ended(&self) -> bool {
        matches!(
            *self.status.borrow(),
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
        super::record::view(&self.dir.join("audit.jsonl"))
    }

    /// Policy layers, most specific first. Uses the running config, so rule
    /// changes apply to running sessions at once; a profile removed from the
    /// running config falls back to its layers at session start.
    pub fn layers(&self, config: &Config) -> Vec<(Scope, Policy)> {
        let mut v = vec![(
            Scope::Session(self.id.clone()),
            self.policy.read().unwrap().clone(),
        )];
        for (scope, policy) in &self.shared_layers {
            v.push((scope.clone(), policy.read().unwrap().clone()));
        }
        match config.resolve(&self.profile) {
            Ok(p) => v.extend(p.layers),
            Err(_) => v.extend(self.resolved.layers.iter().cloned()),
        }
        v.push((Scope::Global, config.policy.clone()));
        v
    }

    /// Adds a rule to the session layer under a fresh `s-<kind>-<n>` id, so
    /// session rule ids never clash with config ones.
    pub fn add_rule(&self, mut rule: AnyRule, position: Option<usize>) -> AnyRule {
        let mut policy = self.policy.write().unwrap();
        let taken: Vec<String> = policy
            .rules()
            .iter()
            .filter_map(|r| r.id().map(str::to_string))
            .collect();
        let kind = rule.kind();
        let id = (1..)
            .map(|n| format!("s-{kind}-{n}"))
            .find(|i| !taken.contains(i))
            .unwrap();
        rule.set_id(id);
        policy.insert(rule.clone(), position);
        rule
    }

    /// The rules of the session's own layer.
    pub fn rules(&self) -> Vec<AnyRule> {
        self.policy.read().unwrap().rules()
    }

    /// Puts a client's rule first in the session's own layer under the id
    /// it has, replacing a rule with that id (an officer's approval, the
    /// network a credential grant needs). It is in the session's events.
    pub fn put_rule(&self, rule: AnyRule) {
        let id = rule.id().unwrap_or("?").to_string();
        {
            let mut p = self.policy.write().unwrap();
            p.remove(&id);
            p.insert(rule, Some(0));
        }
        self.config_event(format!(
            "rule {id} added to {}",
            Scope::Session(self.id.clone())
        ));
    }

    /// Removes a rule from the session's own layer: it, if it was there,
    /// and an event of the session's.
    pub fn remove_rule(&self, id: &str) -> Option<AnyRule> {
        let rule = self.policy.write().unwrap().remove(id)?;
        self.config_event(format!(
            "rule {id} removed from {}",
            Scope::Session(self.id.clone())
        ));
        Some(rule)
    }

    fn config_event(&self, change: String) {
        if let Some(d) = self.daemon.upgrade() {
            d.emit(Some(self), EventBody::Config { change });
        }
    }

    fn send(&self, msg: HostMsg) {
        if let Some(tx) = self.guest.lock().unwrap().as_ref() {
            let _ = tx.try_send(msg);
        }
    }

    /// Opens a terminal in the guest.
    pub async fn open_shell(&self, mut req: ShellRequest) -> Result<(UnixStream, u64)> {
        if !self.is_running() {
            bail!("session {} is not running", self.id);
        }
        if req.argv.is_empty() {
            req.argv = self.resolved.agent.command.clone();
        }
        let id = self.next_shell.fetch_add(1, Ordering::Relaxed);
        req.id = id;
        let mut stream = UnixStream::connect(vm::vsock_path(&self.dir)).await?;
        stream
            .write_all(format!("CONNECT {SHELL_PORT}\n").as_bytes())
            .await?;
        let mut line = Vec::new();
        let mut byte = [0u8; 1];
        while line.len() < 64 {
            stream.read_exact(&mut byte).await?;
            if byte[0] == b'\n' {
                break;
            }
            line.push(byte[0]);
        }
        if !line.starts_with(b"OK ") {
            bail!("guest refused terminal: {}", String::from_utf8_lossy(&line));
        }
        let mut json = serde_json::to_vec(&req)?;
        json.push(b'\n');
        stream.write_all(&json).await?;
        self.counters.shells.fetch_add(1, Ordering::Relaxed);
        Ok((stream, id))
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
        }
    }
}

/// One line from the guest's control channel, at most `MAX_LINE` bytes:
/// `None` at the end, an error for a longer one. Shared by the guest-control
/// loop (`lifecycle`) and nothing else.
pub(crate) async fn read_guest_line(
    reader: &mut (impl tokio::io::AsyncBufRead + Unpin),
) -> Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    let n = reader
        .take(MAX_LINE as u64 + 1)
        .read_until(b'\n', &mut line)
        .await?;
    if n == 0 {
        return Ok(None);
    }
    if n > MAX_LINE {
        bail!("guest sent an oversized message");
    }
    Ok(Some(line))
}
