//! One sandbox: a microVM, its guest channel, policy layer and audit trail.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::Child;
use tokio::sync::{mpsc, watch, Semaphore};
use zapper_policy::proto::{
    Forward, Frame, GuestDisk, GuestFile, GuestMsg, HostMsg, ShellRequest, CONTROL_PORT, MAX_FRAME,
    MAX_LINE, MCP_PORT, PROXY_PORT, SHELL_PORT,
};
use zapper_policy::{
    evaluate_exec, Action, AnyRule, Config, ExecRequest, ExecRule, Kind, MountMode, Policy,
    ResolvedProfile, Scope,
};

use super::{now, proxy, spec, tls, vm, Daemon, Event, EventBody};

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "state", content = "detail", rename_all = "lowercase")]
pub enum Status {
    Starting,
    Running,
    Failed(String),
    Stopped(String),
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

/// Creates a session and boots its VM; returns once the guest is ready.
/// The guest ports the host connects to over vsock: those the session was
/// created with, then the profile's `engine.ports` not among them.
fn forwards(engine: &zapper_policy::Engine, given: &[Forward]) -> Vec<Forward> {
    let mut v = given.to_vec();
    for p in &engine.ports {
        if !v.iter().any(|f| f.vsock_port == *p as u32) {
            v.push(Forward {
                vsock_port: *p as u32,
                tcp_port: *p,
                nic: engine.network == zapper_policy::NetworkMode::Nic,
            });
        }
    }
    v
}

/// Creates a session without booting it: its profile resolved, its
/// directory and audit log, its rules; listed, as starting.
pub(crate) fn register(daemon: &Arc<Daemon>, new: &NewSession) -> Result<Arc<Session>> {
    let (mut resolved, max) = {
        let c = daemon.config.read().unwrap();
        (c.resolve(&new.profile)?, c.daemon.max_sessions)
    };
    if let Some(overlay) = &new.agent_overlay {
        let mut agent = serde_json::to_value(&resolved.agent)?;
        zapper_policy::merge(&mut agent, overlay);
        resolved.agent = serde_json::from_value(agent).context("agent overlay")?;
    }
    resolved.mounts.extend(new.mounts.iter().cloned());
    // The agent's skills and configuration directories, where its harness
    // reads them.
    if let Some(h) = super::harness::get(&resolved.agent.kind) {
        let home = format!("/home/{}", resolved.agent.user);
        let extra = h.mounts(&resolved.agent, &home);
        resolved.mounts.extend(extra);
    }
    let running = daemon
        .sessions
        .read()
        .unwrap()
        .values()
        .filter(|s| matches!(*s.status.borrow(), Status::Starting | Status::Running))
        .count();
    if running >= max {
        bail!("{running} sessions running; daemon.max_sessions is {max}");
    }
    let id = super::random_id(6);
    let dir = daemon.paths.state.join("sessions").join(&id);
    std::fs::create_dir_all(&dir)?;
    let audit = super::record::Writer::open(&dir.join("audit.jsonl"))?;
    // A firm's workers are driven by the office's foreman.
    let driven = resolved.agent.driven
        && !new.labels.contains_key("firm")
        && daemon.config.read().unwrap().daemon.driver;
    let created = now();
    let ttl = resolved.engine.ttl_secs;
    let session = Arc::new(Session {
        id: id.clone(),
        profile: new.profile.clone(),
        created,
        expires: (ttl > 0).then_some(created + ttl as f64),
        workdir: new.workdir.clone(),
        policy: RwLock::new(Policy::default()),
        status: watch::channel(Status::Starting).0,
        ca: tls::SessionCa::new(&id)?,
        dir: dir.clone(),
        resolved,
        counters: Counters::default(),
        usage: Mutex::new(None),
        dns: Mutex::new(Default::default()),
        dns_rate: Mutex::new(Default::default()),
        dns_lookups: Arc::new(Semaphore::new(8)),
        terminals: Mutex::new(Default::default()),
        requests: Default::default(),
        guest: Mutex::new(None),
        vm: tokio::sync::Mutex::new(None),
        audit: Mutex::new(Some(audit)),
        next_shell: AtomicU64::new(1),
        conns: Arc::new(Semaphore::new(256)),
        exec_checks: Arc::new(Semaphore::new(64)),
        labels: new.labels.clone(),
        shared_layers: new.shared_layers.clone(),
        driver: driven.then(super::driver::Driver::new),
        daemon: Arc::downgrade(daemon),
        extra_files: new.files.clone(),
        forwards: new.forwards.clone(),
    });
    for rule in new.policy.rules() {
        session.add_rule(rule, None);
    }
    daemon
        .sessions
        .write()
        .unwrap()
        .insert(id.clone(), session.clone());
    daemon.emit(
        Some(&session),
        EventBody::Session {
            state: "starting".into(),
            detail: Some(format!("profile {}", new.profile)),
        },
    );
    Ok(session)
}

pub async fn create(daemon: Arc<Daemon>, new: NewSession) -> Result<Arc<Session>> {
    let session = register(&daemon, &new)?;
    let (id, dir) = (session.id.clone(), session.dir.clone());
    let ttl = session.resolved.engine.ttl_secs;

    let started = Instant::now();
    if let Err(e) = boot(&daemon, &session).await {
        let msg = format!("{e:#}");
        session.status.send_replace(Status::Failed(msg.clone()));
        daemon.emit(
            Some(&session),
            EventBody::Session {
                state: "failed".into(),
                detail: Some(msg.clone()),
            },
        );
        teardown(&daemon, &session, None).await;
        return Err(anyhow!(msg));
    }
    let mut status = session.status.subscribe();
    let ready = tokio::time::timeout(
        Duration::from_secs(120),
        status.wait_for(|s| !matches!(s, Status::Starting)),
    )
    .await
    .is_ok();
    let state = session.status.borrow().clone();
    match (ready, state) {
        (true, Status::Running) => {
            let ms = started.elapsed().as_millis();
            daemon.emit(
                Some(&session),
                EventBody::Session {
                    state: "running".into(),
                    detail: Some(format!("ready in {ms} ms")),
                },
            );
            super::driver::drive(&daemon, &session);
        }
        (_, state) => {
            let msg = match state {
                Status::Failed(m) => m,
                _ => "guest did not become ready in 120s".into(),
            };
            stop(&daemon, &session, &msg).await;
            return Err(anyhow!(
                "session {id}: {msg}; see {}",
                dir.join("console.log").display()
            ));
        }
    }
    if ttl > 0 {
        let (d, s) = (daemon.clone(), session.clone());
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(ttl)).await;
            stop(&d, &s, "ttl expired").await;
        });
    }
    Ok(session)
}

async fn boot(daemon: &Arc<Daemon>, session: &Arc<Session>) -> Result<()> {
    let dir = &session.dir;
    let vmdir = vm::vm_dir(dir);
    std::fs::create_dir_all(&vmdir)?;
    let engine = &session.resolved.engine;
    let root = vmdir.join("root.ext4");
    vm::root_disk(
        &daemon.paths.images.join(format!("{}.ext4", engine.image)),
        &root,
        vm::parse_size(&engine.disk)?,
    )
    .await
    .context("root disk")?;
    let mut disks = Vec::new();
    let mut guest_disks = Vec::new();
    for (i, m) in session.resolved.mounts.iter().enumerate() {
        let path = vmdir.join(format!("disk{i}.ext4"));
        let host = vm::mount_disk(m, &session.workdir, &path)
            .await
            .with_context(|| format!("mount {}", m.host))?;
        let ro = m.mode == MountMode::Ro;
        disks.push((path, ro));
        let device = format!("/dev/vd{}", (b'b' + i as u8) as char);
        guest_disks.push(GuestDisk {
            device,
            path: m.guest.clone(),
            read_only: ro,
        });
        tracing::debug!(session = %session.id, "mount {} -> {}", host.display(), m.guest);
    }
    let spec = {
        let c = daemon.config.read().unwrap();
        spec::build(
            &c,
            &session.resolved,
            &session.id,
            &session.ca.pem,
            guest_disks,
            session.extra_files.clone(),
            forwards(&session.resolved.engine, &session.forwards),
        )?
    };

    // Guest-initiated vsock connections arrive on `<uds>_<port>`.
    let vsock = vm::vsock_path(dir);
    let control_path = PathBuf::from(format!("{}_{CONTROL_PORT}", vsock.display()));
    let proxy_path = PathBuf::from(format!("{}_{PROXY_PORT}", vsock.display()));
    let control = UnixListener::bind(&control_path)?;
    let proxy = UnixListener::bind(&proxy_path)?;
    // The MCP gateway, to the servers registered for the session.
    let mcp = UnixListener::bind(PathBuf::from(format!("{}_{MCP_PORT}", vsock.display())))?;
    tokio::spawn(super::gateway::serve(daemon.clone(), session.clone(), mcp));
    let nic = engine.network == zapper_policy::NetworkMode::Nic;
    if nic {
        let listener = UnixListener::bind(vmdir.join(crate::netns::SOCKET))?;
        tokio::spawn(super::nic::serve(daemon.clone(), session.clone(), listener));
    }

    let child = vm::start(vm::VmSpec {
        firecracker: &daemon.paths.firecracker,
        kernel: daemon.paths.kernels.join(&engine.kernel),
        initrd: &daemon.paths.initrd,
        root,
        disks,
        engine,
        dir,
        id: &session.id,
        nic,
    })
    .await?;
    *session.vm.lock().await = Some(child);

    {
        let (d, s) = (daemon.clone(), session.clone());
        tokio::spawn(async move {
            if let Err(e) = guest_control(&d, &s, control, control_path, spec).await {
                tracing::warn!(session = %s.id, "guest control: {e:#}");
                if matches!(*s.status.borrow(), Status::Starting) {
                    s.status.send_replace(Status::Failed(format!("{e:#}")));
                }
            }
        });
    }
    {
        let (d, s) = (daemon.clone(), session.clone());
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = proxy.accept().await else {
                    break;
                };
                let Ok(permit) = s.conns.clone().try_acquire_owned() else {
                    drop(stream);
                    continue;
                };
                let (d, s) = (d.clone(), s.clone());
                tokio::spawn(async move {
                    proxy::serve(d, s, stream).await;
                    drop(permit);
                });
            }
        });
    }
    {
        // VM exit watcher.
        let (d, s) = (daemon.clone(), session.clone());
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(500)).await;
                let mut vm = s.vm.lock().await;
                let Some(child) = vm.as_mut() else { return };
                if let Ok(Some(status)) = child.try_wait() {
                    *vm = None;
                    drop(vm);
                    let reason = format!("vm exited ({status})");
                    if matches!(*s.status.borrow(), Status::Starting) {
                        s.status.send_replace(Status::Failed(reason.clone()));
                    }
                    stop(&d, &s, &reason).await;
                    return;
                }
            }
        });
    }
    Ok(())
}

async fn guest_control(
    daemon: &Arc<Daemon>,
    session: &Arc<Session>,
    listener: UnixListener,
    path: PathBuf,
    spec: zapper_policy::proto::GuestSpec,
) -> Result<()> {
    let (stream, _) = tokio::time::timeout(Duration::from_secs(60), listener.accept())
        .await
        .context("guest did not connect")??;
    // Only the first connection is the agent: anything later could be the
    // workload. Remove the socket so further guest connections are refused.
    drop(listener);
    let _ = std::fs::remove_file(&path);

    let (r, mut w) = stream.into_split();
    let (tx, mut rx) = mpsc::channel::<HostMsg>(256);
    tx.send(HostMsg::Init {
        spec: Box::new(spec),
    })
    .await?;
    *session.guest.lock().unwrap() = Some(tx.clone());
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            let mut line = serde_json::to_vec(&msg).unwrap();
            line.push(b'\n');
            if w.write_all(&line).await.is_err() {
                break;
            }
        }
    });

    let mut reader = BufReader::new(r);
    loop {
        let mut line = Vec::new();
        let n = (&mut reader)
            .take(MAX_LINE as u64 + 1)
            .read_until(b'\n', &mut line)
            .await?;
        if n == 0 {
            break;
        }
        if n > MAX_LINE {
            bail!("guest sent an oversized message");
        }
        let msg: GuestMsg = match serde_json::from_slice(&line) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(session = %session.id, "bad guest message: {e}");
                continue;
            }
        };
        match msg {
            GuestMsg::Ready => {
                session.status.send_replace(Status::Running);
            }
            GuestMsg::Failed { error } => {
                session
                    .status
                    .send_replace(Status::Failed(format!("guest setup: {error}")));
            }
            GuestMsg::ExecCheck { id, pid, uid, req } => {
                let Ok(permit) = session.exec_checks.clone().try_acquire_owned() else {
                    let _ = tx.try_send(HostMsg::ExecReply { id, allow: false });
                    continue;
                };
                let (d, s, tx) = (daemon.clone(), session.clone(), tx.clone());
                tokio::spawn(async move {
                    let allow = decide_exec(&d, &s, pid, uid, req).await;
                    let _ = tx.send(HostMsg::ExecReply { id, allow }).await;
                    drop(permit);
                });
            }
            GuestMsg::Event { event } => {
                use zapper_policy::proto::GuestEvent;
                match &event {
                    GuestEvent::Usage { .. } => {
                        *session.usage.lock().unwrap() = Some(event);
                        continue;
                    }
                    GuestEvent::Exit { .. } => {
                        session.counters.exits.fetch_add(1, Ordering::Relaxed);
                    }
                    _ => {}
                }
                daemon.emit(Some(session), EventBody::Guest { event });
            }
        }
    }
    Ok(())
}

async fn decide_exec(
    daemon: &Arc<Daemon>,
    session: &Arc<Session>,
    pid: i32,
    uid: u32,
    req: ExecRequest,
) -> bool {
    let started = Instant::now();
    let decision = {
        let c = daemon.config.read().unwrap();
        let layers = session.layers(&c);
        evaluate_exec(&spec::layers_of(&layers), &req)
    };
    let allow = match decision.action {
        Action::Allow => true,
        Action::Deny => false,
        Action::Ask => {
            let suggestion = AnyRule::Exec(ExecRule {
                id: None,
                action: Action::Allow,
                path: req.path.clone(),
                args: None,
                final_: false,
                comment: Some("approved".into()),
            });
            daemon
                .ask(
                    session,
                    Kind::Exec,
                    format!("exec {}", req.argv.join(" ")),
                    suggestion,
                )
                .await
        }
    };
    let counter = if allow {
        &session.counters.exec_allowed
    } else {
        &session.counters.exec_denied
    };
    counter.fetch_add(1, Ordering::Relaxed);
    daemon.emit(
        Some(session),
        EventBody::Exec {
            pid,
            uid,
            path: req.path,
            argv: req.argv,
            cwd: req.cwd,
            action: if allow { Action::Allow } else { Action::Deny },
            rule: decision.rule,
            scope: decision.scope.map(|s| s.to_string()),
            wait_ms: started.elapsed().as_secs_f64() * 1000.0,
        },
    );
    allow
}

/// Stops a session's VM and removes its disks; the audit log, console log and
/// recordings stay.
pub async fn stop(daemon: &Arc<Daemon>, session: &Arc<Session>, reason: &str) {
    {
        let current = session.status.borrow().clone();
        if matches!(current, Status::Stopped(_)) {
            return;
        }
        if !matches!(current, Status::Failed(_)) {
            session.status.send_replace(Status::Stopped(reason.into()));
        }
    }
    session.send(HostMsg::Shutdown);
    teardown(daemon, session, Some(reason)).await;
}

async fn teardown(daemon: &Arc<Daemon>, session: &Arc<Session>, reason: Option<&str>) {
    if let Some(driver) = &session.driver {
        driver.stop();
    }
    if let Some(mut child) = session.vm.lock().await.take() {
        if tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .is_err()
        {
            let _ = child.kill().await;
        }
    }
    *session.guest.lock().unwrap() = None;
    if let Some(reason) = reason {
        daemon.emit(
            Some(session),
            EventBody::Session {
                state: "stopped".into(),
                detail: Some(reason.into()),
            },
        );
    }
    // The VMM's directory: disks and sockets go; its config and metrics are
    // kept next to the audit log.
    let vmdir = vm::vm_dir(&session.dir);
    for f in ["vm.json", "metrics.json"] {
        let _ = std::fs::rename(vmdir.join(f), session.dir.join(f));
    }
    let _ = std::fs::remove_dir_all(vmdir);
    *session.audit.lock().unwrap() = None;
}

/// Pumps frames between a client and a guest terminal, recording the
/// terminal as an asciicast v2 file in the session directory.
pub async fn pump_shell<C>(
    daemon: Arc<Daemon>,
    session: Arc<Session>,
    client: C,
    guest: UnixStream,
    id: u64,
    argv: Vec<String>,
    tty: Option<(u16, u16)>,
) where
    C: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    daemon.emit(
        Some(&session),
        EventBody::Shell {
            id,
            state: "open".into(),
            argv: Some(argv.clone()),
            exit_code: None,
        },
    );
    let cast = Arc::new(Mutex::new(Recorder::new(&session.dir, id, tty, &argv)));
    let term = Arc::new(Terminal::new(id, argv.clone(), tty.unwrap_or((24, 80))));
    session.terminals.lock().unwrap().insert(id, term.clone());
    let watch = term.clone();
    let (mut cr, mut cw) = tokio::io::split(client);
    let (mut gr, mut gw) = guest.into_split();
    let rec = cast.clone();
    let upstream = tokio::spawn(async move {
        while let Ok(Some(frame)) = read_frame(&mut cr).await {
            match &frame {
                Frame::Data(d) => rec.lock().unwrap().write("i", d),
                Frame::Resize(r, c) => {
                    rec.lock()
                        .unwrap()
                        .write("r", format!("{c}x{r}").as_bytes());
                    watch.push(TermChunk::Resize(*r, *c));
                }
                _ => {}
            }
            let mut buf = Vec::new();
            frame.write_to(&mut buf).ok();
            if gw.write_all(&buf).await.is_err() {
                break;
            }
        }
        let _ = gw.shutdown().await;
    });
    let mut exit_code = None;
    while let Ok(Some(frame)) = read_frame(&mut gr).await {
        match &frame {
            Frame::Data(d) | Frame::Stderr(d) => {
                cast.lock().unwrap().write("o", d);
                term.push(TermChunk::Output(d.clone()));
            }
            Frame::Exit(c) => exit_code = Some(*c),
            _ => {}
        }
        let mut buf = Vec::new();
        frame.write_to(&mut buf).ok();
        if cw.write_all(&buf).await.is_err() || exit_code.is_some() {
            break;
        }
    }
    let _ = cw.shutdown().await;
    upstream.abort();
    term.push(TermChunk::End(exit_code));
    daemon.emit(
        Some(&session),
        EventBody::Shell {
            id,
            state: "closed".into(),
            argv: None,
            exit_code,
        },
    );
}

/// What a watcher of a terminal receives.
#[derive(Debug, Clone)]
pub enum TermChunk {
    Output(Vec<u8>),
    /// Rows, columns.
    Resize(u16, u16),
    /// The terminal closed, with the command's exit code if known.
    End(Option<i32>),
}

/// A terminal's output so far and its live stream, for read-only watchers.
pub struct Terminal {
    pub id: u64,
    pub argv: Vec<String>,
    state: Mutex<TermState>,
    tx: tokio::sync::broadcast::Sender<TermChunk>,
}

struct TermState {
    /// Current size.
    size: (u16, u16),
    /// Size in effect at the start of `history`.
    base: (u16, u16),
    open: bool,
    history: std::collections::VecDeque<TermChunk>,
    bytes: usize,
}

/// Output kept per terminal for watchers who join late.
const TERMINAL_HISTORY: usize = 2 << 20;

#[derive(Serialize)]
pub struct TerminalInfo {
    pub id: u64,
    pub argv: Vec<String>,
    pub rows: u16,
    pub cols: u16,
    pub open: bool,
}

impl Terminal {
    fn new(id: u64, argv: Vec<String>, size: (u16, u16)) -> Self {
        let state = TermState {
            size,
            base: size,
            open: true,
            history: Default::default(),
            bytes: 0,
        };
        Terminal {
            id,
            argv,
            state: Mutex::new(state),
            tx: tokio::sync::broadcast::channel(1024).0,
        }
    }

    fn push(&self, chunk: TermChunk) {
        let mut st = self.state.lock().unwrap();
        match &chunk {
            TermChunk::Output(d) => st.bytes += d.len(),
            TermChunk::Resize(r, c) => st.size = (*r, *c),
            TermChunk::End(_) => st.open = false,
        }
        st.history.push_back(chunk.clone());
        while st.bytes > TERMINAL_HISTORY {
            match st.history.pop_front() {
                Some(TermChunk::Output(d)) => st.bytes -= d.len(),
                Some(TermChunk::Resize(r, c)) => st.base = (r, c),
                Some(_) => {}
                None => break,
            }
        }
        // Sent under the lock, so a watcher subscribing in `watch` sees each
        // chunk exactly once.
        let _ = self.tx.send(chunk);
    }

    /// The kept output, starting with the size it was written at, and a
    /// receiver for what comes after.
    pub fn watch(&self) -> (Vec<TermChunk>, tokio::sync::broadcast::Receiver<TermChunk>) {
        let st = self.state.lock().unwrap();
        let rx = self.tx.subscribe();
        let mut snapshot = Vec::with_capacity(st.history.len() + 1);
        snapshot.push(TermChunk::Resize(st.base.0, st.base.1));
        snapshot.extend(st.history.iter().cloned());
        (snapshot, rx)
    }

    pub fn info(&self) -> TerminalInfo {
        let st = self.state.lock().unwrap();
        TerminalInfo {
            id: self.id,
            argv: self.argv.clone(),
            rows: st.size.0,
            cols: st.size.1,
            open: st.open,
        }
    }
}

pub async fn read_frame(
    r: &mut (impl tokio::io::AsyncRead + Unpin),
) -> std::io::Result<Option<Frame>> {
    let mut header = [0u8; 5];
    match r.read_exact(&mut header).await {
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        other => other?,
    };
    let len = u32::from_be_bytes(header[1..].try_into().unwrap()) as usize;
    if len > MAX_FRAME {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut body = vec![0; len];
    r.read_exact(&mut body).await?;
    Frame::decode(header[0], body).map(Some)
}

/// asciicast v2 writer.
struct Recorder {
    file: Option<super::record::Writer>,
    start: Instant,
}

impl Recorder {
    fn new(dir: &Path, id: u64, tty: Option<(u16, u16)>, argv: &[String]) -> Self {
        let file = super::record::Writer::open(&dir.join(format!("shell-{id}.cast"))).ok();
        let (rows, cols) = tty.unwrap_or((24, 80));
        let mut r = Recorder {
            file,
            start: Instant::now(),
        };
        let header = serde_json::json!({ "version": 2, "width": cols, "height": rows, "timestamp": now() as u64, "command": argv.join(" ") });
        if let Some(f) = r.file.as_mut() {
            let _ = f.line(header.to_string().as_bytes());
        }
        r
    }

    fn write(&mut self, kind: &str, data: &[u8]) {
        if let Some(f) = self.file.as_mut() {
            let t = self.start.elapsed().as_secs_f64();
            let line = serde_json::json!([t, kind, String::from_utf8_lossy(data)]);
            let _ = f.line(line.to_string().as_bytes());
        }
    }
}

#[cfg(test)]
mod forward_tests {
    use zapper_policy::proto::Forward;
    use zapper_policy::{Engine, NetworkMode};

    #[test]
    fn a_profiles_ports_join_the_sessions_forwards_once() {
        let given = [Forward {
            vsock_port: 6443,
            tcp_port: 6443,
            nic: true,
        }];
        let engine = Engine {
            ports: vec![6443, 8080],
            ..Engine::default()
        };
        let f = super::forwards(&engine, &given);
        let ports: Vec<(u32, u16, bool)> = f
            .iter()
            .map(|f| (f.vsock_port, f.tcp_port, f.nic))
            .collect();
        assert_eq!(ports, [(6443, 6443, true), (8080, 8080, true)]);
        // Without a NIC, to the guest's loopback.
        let vsock = Engine {
            ports: vec![8080],
            network: NetworkMode::Vsock,
            ..Engine::default()
        };
        assert!(!super::forwards(&vsock, &[])[0].nic);
    }
}
