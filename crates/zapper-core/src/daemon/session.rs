//! One sandbox: a microVM, its guest channel, policy layer and audit trail.
//! The session's state and queries live here; starting and stopping it is in
//! `lifecycle`, and its interactive terminals in `terminal`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
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

use super::{tls, Daemon, Event, EventBody};

mod boot;
mod lifecycle;
mod spec;
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

/// Opens the session's vsock at `path` and asks the guest for `port`,
/// returning the stream once the guest answers `OK `. A connect/write failure
/// propagates; a refusal is reported as `refused: <the guest's line>`.
async fn connect_vsock(path: PathBuf, port: &str, refused: &str) -> Result<UnixStream> {
    let mut stream = UnixStream::connect(&path).await?;
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
        bail!("{refused}: {}", String::from_utf8_lossy(&line));
    }
    Ok(stream)
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
            .filter(|s| {
                matches!(
                    *s.runtime.status().borrow(),
                    Status::Starting | Status::Running
                )
            })
            .count()
    }

    /// Take a stopped session off the list; a starting or running one stays,
    /// as nothing would manage its VM.
    pub(crate) fn forget(&self, id: &str) -> Result<()> {
        let mut sessions = self.0.write().unwrap();
        if let Some(s) = sessions.get(id) {
            if matches!(
                *s.runtime.status().borrow(),
                Status::Starting | Status::Running
            ) {
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

/// A session's directory and the files and sockets laid out under it: the
/// single owner of that layout. `vm` (the VMM process), the record and the
/// driver are handed paths from here rather than joining their own, so the
/// structure lives in one place.
#[derive(Clone)]
pub(crate) struct SessionDir {
    root: PathBuf,
}

impl SessionDir {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
    /// The session directory itself: console log, recordings and the audit
    /// log live here, outside the part the VMM sees.
    pub fn root(&self) -> &Path {
        &self.root
    }
    /// The part the VMM sees: disks, its config, metrics and sockets.
    pub fn vm(&self) -> PathBuf {
        self.root.join("vm")
    }
    pub fn root_disk(&self) -> PathBuf {
        self.vm().join("root.ext4")
    }
    pub fn data_disk(&self, i: usize) -> PathBuf {
        self.vm().join(format!("disk{i}.ext4"))
    }
    /// The VM's vsock socket; guest-initiated connections arrive on
    /// `<vsock>_<port>`.
    pub fn vsock(&self) -> PathBuf {
        self.vm().join("v.sock")
    }
    pub fn vsock_port(&self, port: u32) -> PathBuf {
        PathBuf::from(format!("{}_{port}", self.vsock().display()))
    }
    /// The daemon's socket for the guest's captured traffic (`netns`).
    pub fn nic_socket(&self) -> PathBuf {
        self.vm().join(crate::netns::SOCKET)
    }
    /// Firecracker's own config and metrics, under the VMM's directory.
    pub fn vm_config(&self) -> PathBuf {
        self.vm().join("vm.json")
    }
    pub fn metrics(&self) -> PathBuf {
        self.vm().join("metrics.json")
    }
    pub fn console(&self) -> PathBuf {
        self.root.join("console.log")
    }
    pub fn audit(&self) -> PathBuf {
        self.root.join("audit.jsonl")
    }
    pub fn driver_log(&self) -> PathBuf {
        self.root.join("driver.jsonl")
    }
}

/// A session's rule state: its own policy layer, and the shared layers a
/// client keeps for it (a firm role, between the session's layer and its
/// profile's). The owner of the session's rules; `Session` wraps these with
/// the identity and events they need.
pub(crate) struct Rules {
    own: RwLock<Policy>,
    shared: Vec<(Scope, Arc<RwLock<Policy>>)>,
}

impl Rules {
    pub fn new(shared: Vec<(Scope, Arc<RwLock<Policy>>)>) -> Self {
        Self {
            own: RwLock::new(Policy::default()),
            shared,
        }
    }

    /// A clone of the session's own layer.
    pub fn own(&self) -> Policy {
        self.own.read().unwrap().clone()
    }

    /// The rules of the session's own layer.
    pub fn list(&self) -> Vec<AnyRule> {
        self.own.read().unwrap().rules()
    }

    /// Adds a rule to the own layer under a fresh `s-<kind>-<n>` id, so
    /// session rule ids never clash with config ones.
    pub fn add(&self, mut rule: AnyRule, position: Option<usize>) -> AnyRule {
        let mut policy = self.own.write().unwrap();
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

    /// Replaces the rule with `rule`'s id and puts it first; returns that id.
    /// Callers always pass an id'd rule (an officer's approval, a grant); the
    /// `"?"` fallback never fires.
    pub fn put(&self, rule: AnyRule) -> String {
        let id = rule.id().unwrap_or("?").to_string();
        let mut p = self.own.write().unwrap();
        p.remove(&id);
        p.insert(rule, Some(0));
        id
    }

    /// Removes a rule from the own layer, returning it if it was there.
    pub fn remove(&self, id: &str) -> Option<AnyRule> {
        self.own.write().unwrap().remove(id)
    }

    /// The session's policy layers, most specific first: own, shared, the
    /// profile's (from the running config, falling back to those resolved at
    /// start), then global.
    pub fn layers(
        &self,
        id: &str,
        profile: &str,
        resolved: &ResolvedProfile,
        config: &Config,
    ) -> Vec<(Scope, Policy)> {
        let mut v = vec![(Scope::Session(id.to_string()), self.own())];
        for (scope, policy) in &self.shared {
            v.push((scope.clone(), policy.read().unwrap().clone()));
        }
        match config.resolve(profile) {
            Ok(p) => v.extend(p.layers),
            Err(_) => v.extend(resolved.layers.iter().cloned()),
        }
        v.push((Scope::Global, config.policy.clone()));
        v
    }
}

/// A session's live machinery: its lifecycle status, the Firecracker
/// process, the control channel to the guest agent, and the counters that
/// gate shells and exec decisions. All of it is created when the VM boots
/// and dropped when it stops; the owner of "the session while it runs".
pub(crate) struct GuestRuntime {
    status: watch::Sender<Status>,
    vm: tokio::sync::Mutex<Option<Child>>,
    /// The control channel to the guest agent, once it connects.
    guest: Mutex<Option<mpsc::Sender<HostMsg>>>,
    next_shell: AtomicU64,
    /// Bounds concurrent exec decisions.
    exec_checks: Arc<Semaphore>,
    /// Bounds concurrent proxy connections from the guest.
    conns: Arc<Semaphore>,
}

impl GuestRuntime {
    pub fn new() -> Self {
        Self {
            status: watch::channel(Status::Starting).0,
            vm: tokio::sync::Mutex::new(None),
            guest: Mutex::new(None),
            next_shell: AtomicU64::new(1),
            exec_checks: Arc::new(Semaphore::new(64)),
            conns: Arc::new(Semaphore::new(256)),
        }
    }

    /// The lifecycle-status channel: subscribe to it, borrow it, or send a
    /// transition through it.
    pub fn status(&self) -> &watch::Sender<Status> {
        &self.status
    }

    /// The Firecracker process handle: set at boot, taken at teardown.
    pub fn vm(&self) -> &tokio::sync::Mutex<Option<Child>> {
        &self.vm
    }

    /// Records the guest's control channel once it connects.
    pub fn set_guest(&self, tx: mpsc::Sender<HostMsg>) {
        *self.guest.lock().unwrap() = Some(tx);
    }

    /// Drops the control channel (teardown).
    pub fn clear_guest(&self) {
        *self.guest.lock().unwrap() = None;
    }

    /// Sends a message to the guest agent, if the channel is up.
    pub fn send(&self, msg: HostMsg) {
        if let Some(tx) = self.guest.lock().unwrap().as_ref() {
            let _ = tx.try_send(msg);
        }
    }

    /// The next terminal id.
    pub fn next_shell(&self) -> u64 {
        self.next_shell.fetch_add(1, Ordering::Relaxed)
    }

    /// A permit for one concurrent exec decision, or `None` over the cap.
    pub fn exec_permit(&self) -> Option<tokio::sync::OwnedSemaphorePermit> {
        self.exec_checks.clone().try_acquire_owned().ok()
    }

    /// The semaphore bounding concurrent proxy connections, for `accept_loop`.
    pub fn conns(&self) -> Arc<Semaphore> {
        self.conns.clone()
    }
}

pub struct Session {
    pub id: String,
    pub profile: String,
    pub created: f64,
    pub expires: Option<f64>,
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
    pub(crate) usage: Mutex<Option<zapper_policy::proto::GuestEvent>>,
    /// The session's resolver state: synthetic names, rate limit, lookup cap.
    pub(crate) dns: super::dns::SessionDns,
    /// The session's HTTP traffic, for inspecting it from the UI.
    pub(crate) requests: Arc<Mutex<super::httplog::RequestLog>>,
    /// The session's terminals, for watching them from the UI.
    pub(crate) terminals: Mutex<std::collections::BTreeMap<u64, Arc<Terminal>>>,
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
            status: self.runtime.status().borrow().clone(),
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
        matches!(*self.runtime.status().borrow(), Status::Running)
    }

    /// Whether the session has ended, stopped or failed.
    pub fn ended(&self) -> bool {
        matches!(
            *self.runtime.status().borrow(),
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

    /// Adds a rule to the session layer under a fresh `s-<kind>-<n>` id, so
    /// session rule ids never clash with config ones.
    pub fn add_rule(&self, rule: AnyRule, position: Option<usize>) -> AnyRule {
        self.rules.add(rule, position)
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
        self.config_event(format!(
            "rule {id} added to {}",
            Scope::Session(self.id.clone())
        ));
    }

    /// Removes a rule from the session's own layer: it, if it was there,
    /// and an event of the session's.
    pub fn remove_rule(&self, id: &str) -> Option<AnyRule> {
        let rule = self.rules.remove(id)?;
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
            zapper_policy::mint::forget(&spec);
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
            zapper_policy::mint::forget(&spec);
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
        if req.argv.is_empty() {
            req.argv = self.resolved.agent.command.clone();
        }
        let id = self.runtime.next_shell();
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

#[cfg(test)]
mod tests {
    use super::*;

    fn net_rule() -> AnyRule {
        AnyRule::Net(
            serde_json::from_value(serde_json::json!({ "action": "allow", "host": "x.example" }))
                .unwrap(),
        )
    }

    fn exec_rule() -> AnyRule {
        AnyRule::Exec(
            serde_json::from_value(serde_json::json!({ "action": "allow", "path": "/bin/ls" }))
                .unwrap(),
        )
    }

    #[test]
    fn session_dir_lays_the_vm_files_apart_from_the_logs() {
        let d = SessionDir::new(PathBuf::from("/s"));
        assert_eq!(d.root(), Path::new("/s"));
        // Under the VMM's `vm` subdirectory: disks, sockets, config, metrics.
        assert_eq!(d.vm(), Path::new("/s/vm"));
        assert_eq!(d.root_disk(), Path::new("/s/vm/root.ext4"));
        assert_eq!(d.data_disk(0), Path::new("/s/vm/disk0.ext4"));
        assert_eq!(d.vsock(), Path::new("/s/vm/v.sock"));
        assert_eq!(d.vsock_port(5), Path::new("/s/vm/v.sock_5"));
        assert_eq!(d.nic_socket(), Path::new("/s/vm/nic.sock"));
        assert_eq!(d.vm_config(), Path::new("/s/vm/vm.json"));
        assert_eq!(d.metrics(), Path::new("/s/vm/metrics.json"));
        // Logs, recordings and the audit log stay outside it.
        assert_eq!(d.console(), Path::new("/s/console.log"));
        assert_eq!(d.audit(), Path::new("/s/audit.jsonl"));
        assert_eq!(d.driver_log(), Path::new("/s/driver.jsonl"));
    }

    #[test]
    fn rules_mint_per_kind_ids_and_reuse_a_freed_one() {
        let r = Rules::new(vec![]);
        assert_eq!(r.add(net_rule(), None).id(), Some("s-net-1"));
        assert_eq!(r.add(net_rule(), None).id(), Some("s-net-2"));
        // Each kind has its own sequence.
        assert_eq!(r.add(exec_rule(), None).id(), Some("s-exec-1"));
        // The lowest free id is reused once one is removed.
        assert!(r.remove("s-net-1").is_some());
        assert_eq!(r.add(net_rule(), None).id(), Some("s-net-1"));
        // Removing it again finds nothing.
        assert!(r.remove("s-net-1").is_some());
        assert!(r.remove("s-net-1").is_none());
    }

    #[test]
    fn put_replaces_a_rule_by_id_and_puts_it_first() {
        let r = Rules::new(vec![]);
        r.add(net_rule(), None); // s-net-1
        r.add(net_rule(), None); // s-net-2
        let replacement: AnyRule = AnyRule::Net(
            serde_json::from_value(
                serde_json::json!({ "id": "s-net-1", "action": "deny", "host": "y.example" }),
            )
            .unwrap(),
        );
        r.put(replacement);
        let rules = r.list();
        assert_eq!(rules.len(), 2, "replaced, not added");
        assert_eq!(rules[0].id(), Some("s-net-1"), "the replacement is first");
        assert!(
            matches!(&rules[0], AnyRule::Net(n) if n.action == zapper_policy::Action::Deny),
            "it is the new rule"
        );
    }

    #[test]
    fn layers_fall_back_to_the_resolved_profile_when_it_leaves_the_config() {
        let c1 = Config::from_yaml(
            "profiles:\n  web:\n    policy:\n      net:\n        - { action: allow, host: a.test }\n",
        )
        .unwrap();
        let resolved = c1.resolve("web").unwrap();
        // A running config that no longer has `web`.
        let c2 = Config::from_yaml("profiles:\n  other: {}\n").unwrap();
        let layers = Rules::new(vec![]).layers("sid", "web", &resolved, &c2);
        let scopes: Vec<Scope> = layers.iter().map(|(s, _)| s.clone()).collect();
        assert_eq!(scopes.first(), Some(&Scope::Session("sid".into())));
        assert_eq!(scopes.last(), Some(&Scope::Global));
        // The profile layer came from the resolved snapshot, not from c2.
        assert!(
            layers
                .iter()
                .any(|(s, p)| matches!(s, Scope::Profile(n) if n == "web") && !p.net.is_empty()),
            "fell back to the resolved web layers: {scopes:?}"
        );
    }
}
