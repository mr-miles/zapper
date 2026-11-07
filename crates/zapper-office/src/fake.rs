//! The office's tests without a gateway: [`Fake`], an in-memory
//! [`OfficeClient`] that keeps sessions, the role layers and each session's
//! rules, and plays a session's commands as a test scripts them
//! ([`shell_vm`]). [`TestCore`] sets an office up on it, from a config the
//! way production reads one; [`mcp`] calls the office's tools as a worker's
//! harness would.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail, Result};
use futures_util::future::BoxFuture;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader, DuplexStream};
use tokio::io::{ReadHalf, WriteHalf};
use zapper_api_types::{NewSession, ServiceSpec, SessionInfo, Status};
use zapper_driver::process::DriveStream;
use zapper_policy::facility::{McpCall, McpMethod};
use zapper_policy::proto::{Frame, ShellRequest};
use zapper_policy::{AnyRule, Config, NetRule, Policy, ResolvedProfile, Scope};

use crate::client::OfficeClient;
use crate::Office;

/// Session labels, or a selector of sessions by them.
type Labels = BTreeMap<String, String>;

/// A temporary config directory with an office started on it.
pub struct TestCore {
    pub core: Core,
    pub dir: PathBuf,
}

impl TestCore {
    /// An office whose zapper config is `yaml` (profiles, policy), with
    /// `daemon` added to its `daemon` section (one `key: value`
    /// per line).
    pub fn with_daemon(daemon: &str, yaml: &str) -> TestCore {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "zapper-office-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("work")).unwrap();
        let state = dir.join("state");
        let mut daemon_yaml = format!("daemon:\n  state: {}\n", state.display());
        for line in daemon.lines().filter(|l| !l.trim().is_empty()) {
            daemon_yaml.push_str(&format!("  {}\n", line.trim()));
        }
        let config_path = dir.join("zapper.yaml");
        std::fs::write(&config_path, format!("{daemon_yaml}{yaml}")).unwrap();
        let config = Config::from_yaml(&std::fs::read_to_string(&config_path).unwrap())
            .expect("the test config");
        std::fs::create_dir_all(&state).unwrap();
        let core = Core(Arc::new(Inner {
            config_path,
            state,
            config: Mutex::new(config),
            office: Arc::new(Office::default()),
            sessions: Default::default(),
            layers: Default::default(),
            mcp: Default::default(),
            next: AtomicU64::new(1),
        }));
        TestCore { core, dir }
    }

    /// The zapper config file.
    pub fn startup(&self) -> PathBuf {
        self.core.startup_path()
    }

    /// An empty directory for a session's work.
    pub fn workdir(&self) -> PathBuf {
        self.dir.join("work")
    }
}

impl Drop for TestCore {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The office's backend in the tests. Cloning it is cheap: it is shared.
#[derive(Clone)]
pub struct Core(Arc<Inner>);

struct Inner {
    config_path: PathBuf,
    state: PathBuf,
    config: Mutex<Config>,
    office: Arc<Office>,
    sessions: Mutex<BTreeMap<String, Arc<Session>>>,
    layers: Mutex<BTreeMap<String, Policy>>,
    /// The MCP servers registered: their names and the sessions they serve.
    mcp: Mutex<BTreeMap<u64, (String, Labels)>>,
    next: AtomicU64,
}

/// A session of the fake: what it was made from, its own rules, and the
/// test playing its VM, if one does.
pub struct Session {
    pub id: String,
    pub resolved: ResolvedProfile,
    pub labels: BTreeMap<String, String>,
    rules: Mutex<Policy>,
    status: Mutex<Status>,
    vm: Mutex<Option<tokio::sync::mpsc::UnboundedSender<Shell>>>,
    /// Its traffic, as a test makes it up.
    requests: Mutex<Vec<Value>>,
}

impl Session {
    /// Puts `rule` on the session's own layer, in place of one with its id.
    pub fn put_rule(&self, rule: AnyRule) {
        let mut rules = self.rules.lock().unwrap();
        if let Some(id) = rule.id() {
            rules.remove(id);
        }
        rules.insert(rule, Some(0));
    }

    /// Takes rule `id` off the session's own layer.
    pub fn remove_rule(&self, id: &str) -> Option<AnyRule> {
        self.rules.lock().unwrap().remove(id)
    }

    /// The rules of the session's own layer.
    pub fn rules(&self) -> Vec<AnyRule> {
        self.policy().rules()
    }

    /// The session's own layer.
    pub fn policy(&self) -> Policy {
        self.rules.lock().unwrap().clone()
    }
}

impl Core {
    /// The office this backend serves, started on it on first use.
    pub fn office(&self) -> Arc<Office> {
        let office = self.0.office.clone();
        if office.client.get().is_none() {
            let _ = office.client.set(Arc::new(Fake(self.clone())));
            let _ = office.state_dir.set(self.0.state.clone());
            let _ = office.socket_dir.set(self.socket_dir());
        }
        office
    }

    pub fn config(&self) -> Config {
        self.0.config.lock().unwrap().clone()
    }

    /// A new backend on the same config and state, with an office of its
    /// own and no sessions: as after a restart.
    pub fn reopen(&self) -> Core {
        Core(Arc::new(Inner {
            config_path: self.0.config_path.clone(),
            state: self.0.state.clone(),
            config: Mutex::new(self.config()),
            office: Arc::new(Office::default()),
            sessions: Default::default(),
            layers: Default::default(),
            mcp: Default::default(),
            next: AtomicU64::new(1),
        }))
    }

    /// Replaces the config (as an edit to `zapper.yaml` would).
    pub fn set_config(&self, config: Config) {
        *self.0.config.lock().unwrap() = config;
    }

    pub fn startup_path(&self) -> PathBuf {
        self.0.config_path.clone()
    }

    pub fn state_dir(&self) -> &Path {
        &self.0.state
    }

    pub fn socket_dir(&self) -> PathBuf {
        let d = self.0.state.join("services");
        let _ = std::fs::create_dir_all(&d);
        d
    }

    /// A session made from `new`, ready at once (no VM boots).
    pub fn test_session(&self, new: NewSession, _ready: bool) -> Result<Arc<Session>> {
        let resolved = self.config().resolve(&new.profile)?;
        let id = format!("s{:05}", self.0.next.fetch_add(1, Ordering::Relaxed));
        let s = Arc::new(Session {
            id: id.clone(),
            resolved,
            labels: new.labels,
            rules: Mutex::new(new.policy),
            status: Mutex::new(Status::Running),
            vm: Mutex::new(None),
            requests: Mutex::new(Vec::new()),
        });
        self.0.sessions.lock().unwrap().insert(id, s.clone());
        Ok(s)
    }

    pub fn session(&self, id: &str) -> Option<Arc<Session>> {
        self.0.sessions.lock().unwrap().get(id).cloned()
    }

    pub fn sessions(&self) -> Vec<Arc<Session>> {
        self.0.sessions.lock().unwrap().values().cloned().collect()
    }

    pub fn session_info(&self, s: &Session) -> SessionInfo {
        let p = &s.resolved;
        SessionInfo {
            id: s.id.clone(),
            profile: p.name.clone(),
            status: s.status.lock().unwrap().clone(),
            created: zapper_util::now(),
            expires: None,
            persistent: false,
            ssh: false,
            disk_bytes: None,
            image: p.engine.image.clone(),
            vcpus: p.engine.vcpus,
            memory_mib: p.engine.memory_mib,
            kernel: "fake".into(),
            network: p.engine.network,
            user: p.agent.user.clone(),
            root: false,
            workdir: p.agent.workdir.clone(),
            counters: json!({}),
            usage: None,
            rules: s.rules().len(),
            firm: s.labels.get("firm").cloned(),
            member: s.labels.get("member").cloned(),
            labels: s.labels.clone(),
            driver: Value::Null,
        }
    }

    /// Stops session `s` and forgets it.
    pub async fn stop_session(&self, s: &Session, _reason: &str) {
        let _ = self.stop(&s.id);
    }

    /// Stops session `id` and forgets it.
    pub fn stop(&self, id: &str) -> Result<()> {
        let s = self
            .0
            .sessions
            .lock()
            .unwrap()
            .remove(id)
            .ok_or_else(|| anyhow!("no session {id}"))?;
        *s.status.lock().unwrap() = Status::Stopped("stopped".into());
        s.vm.lock().unwrap().take();
        Ok(())
    }

    /// A role's shared layer `name` (`firm/role`).
    pub fn layer(&self, name: &str) -> Policy {
        self.0
            .layers
            .lock()
            .unwrap()
            .get(name)
            .cloned()
            .unwrap_or_default()
    }

    /// Session `id`'s layers, most specific first: its own, its role's, its
    /// profile's, the global one.
    pub fn layers_for(
        &self,
        id: Option<&str>,
        _profile: Option<&str>,
    ) -> Result<Vec<(Scope, Policy)>> {
        let id = id.ok_or_else(|| anyhow!("no session"))?;
        let s = self.session(id).ok_or_else(|| anyhow!("no session {id}"))?;
        let mut layers = vec![(Scope::Session(id.to_string()), s.policy())];
        if let Some(role) = s.labels.get("firm-role") {
            layers.push((Scope::Role(role.clone()), self.layer(role)));
        }
        layers.extend(s.resolved.layers.clone());
        layers.push((Scope::Global, self.config().policy));
        Ok(layers)
    }

    /// Whether an MCP server `name` is registered for session `s`.
    fn serves(&self, name: &str, s: &Session) -> bool {
        self.0
            .mcp
            .lock()
            .unwrap()
            .values()
            .any(|(n, sel)| n == name && sel.iter().all(|(k, v)| s.labels.get(k) == Some(v)))
    }

    /// Takes the office's runs down, as a shutdown would.
    pub async fn shutdown(&self) {
        let runs: Vec<_> = self
            .office()
            .runs
            .read()
            .unwrap()
            .values()
            .cloned()
            .collect();
        for r in runs {
            crate::down(&r, "office shutdown").await;
        }
    }
}

/// The fake as the office's client.
pub struct Fake(pub Core);

impl OfficeClient for Fake {
    fn add_layer_rule(&self, name: &str, rule: AnyRule) -> AnyRule {
        self.0
             .0
            .layers
            .lock()
            .unwrap()
            .entry(name.to_string())
            .or_default()
            .insert(rule.clone(), Some(0));
        rule
    }

    fn remove_layer_rule(&self, name: &str, id: &str) -> bool {
        self.0
             .0
            .layers
            .lock()
            .unwrap()
            .get_mut(name)
            .and_then(|p| p.remove(id))
            .is_some()
    }

    fn reset_layer(&self, name: &str) {
        self.0
             .0
            .layers
            .lock()
            .unwrap()
            .insert(name.to_string(), Policy::default());
    }

    fn record_writable(&self) -> bool {
        true
    }

    fn config(&self) -> Config {
        self.0.config()
    }

    fn session_count(&self) -> usize {
        self.0 .0.sessions.lock().unwrap().len()
    }

    fn admit(&self, profile: &str) -> Result<Config> {
        let config = self.0.config();
        config.resolve(profile)?;
        Ok(config)
    }

    fn promote_rule(&self, _what: &str, scope: &Scope, mut rule: NetRule) -> Result<AnyRule> {
        let mut config = self.0.config();
        let n = config.policy.net.len() + 1;
        rule.id = Some(format!("net-{n}"));
        match scope {
            Scope::Global => config.policy.net.insert(0, rule.clone()),
            Scope::Profile(p) => config
                .profiles
                .get_mut(p)
                .ok_or_else(|| anyhow!("no profile {p}"))?
                .policy
                .net
                .insert(0, rule.clone()),
            other => bail!("promoting to {other:?} isn't supported"),
        }
        self.0.set_config(config);
        Ok(AnyRule::Net(rule))
    }

    fn create_session<'a>(&'a self, new: NewSession) -> BoxFuture<'a, Result<SessionInfo>> {
        Box::pin(async move {
            let s = self.0.test_session(new, true)?;
            Ok(self.0.session_info(&s))
        })
    }

    fn stop_session<'a>(&'a self, id: &'a str, _reason: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { self.0.stop(id) })
    }

    fn session(&self, id: &str) -> Result<SessionInfo> {
        let s = self
            .0
            .session(id)
            .ok_or_else(|| anyhow!("no session {id}"))?;
        Ok(self.0.session_info(&s))
    }

    fn attach<'a>(
        &'a self,
        id: &'a str,
        req: ShellRequest,
    ) -> BoxFuture<'a, Result<Box<dyn DriveStream>>> {
        Box::pin(async move {
            let s = self
                .0
                .session(id)
                .ok_or_else(|| anyhow!("no session {id}"))?;
            let vm = s.vm.lock().unwrap().clone();
            let Some(vm) = vm else {
                bail!("session {id} has no VM to run it");
            };
            let (ours, theirs) = tokio::io::duplex(256 * 1024);
            let (read, write) = tokio::io::split(theirs);
            vm.send(Shell {
                request: req,
                read,
                write,
                buf: Vec::new(),
            })
            .map_err(|_| anyhow!("session {id}'s VM is gone"))?;
            Ok(Box::new(ours) as Box<dyn DriveStream>)
        })
    }

    fn connect_port<'a>(
        &'a self,
        id: &'a str,
        port: u32,
    ) -> BoxFuture<'a, Result<Box<dyn DriveStream>>> {
        Box::pin(async move { bail!("session {id} serves nothing on port {port}") })
    }

    fn add_mcp_socket(
        &self,
        name: &str,
        sessions: BTreeMap<String, String>,
        _socket: &str,
        _instructions: Option<String>,
        _owner: &str,
    ) -> Result<u64> {
        let id = self.0 .0.next.fetch_add(1, Ordering::Relaxed);
        self.0
             .0
            .mcp
            .lock()
            .unwrap()
            .insert(id, (name.to_string(), sessions));
        Ok(id)
    }

    fn remove_mcp_server(&self, id: u64) -> bool {
        self.0 .0.mcp.lock().unwrap().remove(&id).is_some()
    }

    fn add_decider_socket(
        &self,
        _sessions: BTreeMap<String, String>,
        _socket: &str,
    ) -> Result<u64> {
        Ok(self.0 .0.next.fetch_add(1, Ordering::Relaxed))
    }

    fn remove_decider(&self, _id: u64) -> bool {
        true
    }

    fn add_service(&self, _spec: ServiceSpec) -> Result<u64> {
        Ok(self.0 .0.next.fetch_add(1, Ordering::Relaxed))
    }

    fn add_zone(
        &self,
        _domain: &str,
        _sessions: BTreeMap<String, String>,
        _owner: &str,
    ) -> Result<u64> {
        Ok(self.0 .0.next.fetch_add(1, Ordering::Relaxed))
    }

    fn remove_service(&self, _id: u64) {}

    fn requests(&self, id: &str) -> Vec<Value> {
        self.0
            .session(id)
            .map(|s| s.requests.lock().unwrap().clone())
            .unwrap_or_default()
    }

    fn put_session_rule(&self, id: &str, rule: AnyRule) {
        if let Some(s) = self.0.session(id) {
            s.put_rule(rule);
        }
    }

    fn remove_session_rule(&self, id: &str, rule_id: &str) -> bool {
        self.0
            .session(id)
            .is_some_and(|s| s.remove_rule(rule_id).is_some())
    }
}

/// A command the office started in a session, as the test playing its VM
/// sees it: what was asked for, and its stdin and stdout.
pub struct Shell {
    pub request: ShellRequest,
    read: ReadHalf<DuplexStream>,
    write: WriteHalf<DuplexStream>,
    buf: Vec<u8>,
}

impl Shell {
    /// The next line of its stdin, without the newline; none once the office
    /// closed it.
    pub async fn line(&mut self) -> Option<String> {
        loop {
            if let Some(i) = self.buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = self.buf.drain(..=i).collect();
                return Some(String::from_utf8_lossy(&line[..i]).into_owned());
            }
            match zapper_driver::read_frame(&mut self.read).await {
                Ok(Some(Frame::Data(d))) => self.buf.extend(d),
                Ok(Some(Frame::Resize(..))) => {}
                _ => return None,
            }
        }
    }

    /// Writes `line` and a newline to its stdout.
    pub async fn say(&mut self, line: &str) {
        self.frame(Frame::Data(format!("{line}\n").into_bytes()))
            .await;
    }

    /// All of its stdin, up to the office's end of it.
    pub async fn input(&mut self) -> Vec<u8> {
        let mut all = std::mem::take(&mut self.buf);
        while let Ok(Some(Frame::Data(d))) = zapper_driver::read_frame(&mut self.read).await {
            all.extend(d);
        }
        all
    }

    /// Writes `bytes` to its stdout as they are.
    pub async fn output(&mut self, bytes: &[u8]) {
        for chunk in bytes.chunks(60 * 1024) {
            self.frame(Frame::Data(chunk.to_vec())).await;
        }
    }

    /// The command exits with `code`.
    pub async fn exit(mut self, code: i32) {
        self.frame(Frame::Exit(code)).await;
    }

    async fn frame(&mut self, f: Frame) {
        let _ = self.write.write_all(&f.encode()).await;
    }
}

/// Plays the VM of session `s`: each command the office starts in it, as it
/// is started.
pub fn shell_vm(s: &Session) -> tokio::sync::mpsc::UnboundedReceiver<Shell> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    *s.vm.lock().unwrap() = Some(tx);
    rx
}

/// Serves one MCP connection from session `s`, as its harness would make
/// one to the office: the server's name on the first line, then JSON-RPC,
/// a message a line. The caller is `s` as the office made it, never what
/// the stream says; the server must be registered for it.
pub async fn mcp<S>(core: &Core, s: &Arc<Session>, stream: S) -> Result<()>
where
    S: tokio::io::AsyncRead + AsyncWrite + Unpin,
{
    let (read, mut write) = tokio::io::split(stream);
    let mut lines = BufReader::new(read).lines();
    let server = lines.next_line().await?.unwrap_or_default();
    let server = server.trim().to_string();
    let office = core.office();
    while let Some(line) = lines.next_line().await? {
        let too_long = line.len() > zapper_policy::proto::MAX_MCP_LINE;
        let reply = if too_long {
            rpc_error(
                Value::Null,
                -32600,
                format!(
                    "request longer than {} bytes",
                    zapper_policy::proto::MAX_MCP_LINE
                ),
            )
        } else {
            match serde_json::from_str::<Value>(&line) {
                Err(e) => rpc_error(Value::Null, -32700, format!("bad request: {e}")),
                Ok(msg) => match answer(core, &office, s, &server, &msg).await {
                    None => continue,
                    Some(r) => r,
                },
            }
        };
        write.write_all(format!("{reply}\n").as_bytes()).await?;
        if too_long {
            break;
        }
    }
    Ok(())
}

/// The answer to one JSON-RPC message from `s` to MCP server `server`;
/// `None` for a notification.
async fn answer(
    core: &Core,
    office: &Arc<Office>,
    s: &Arc<Session>,
    server: &str,
    msg: &Value,
) -> Option<Value> {
    let id = msg.get("id").cloned()?;
    if !core.serves(server, s) {
        return Some(rpc_error(
            id,
            -32601,
            format!("no MCP server {server} for this session"),
        ));
    }
    let method = msg["method"].as_str().unwrap_or("");
    let call = |m: McpMethod, tool: Option<String>, args: Option<Value>| McpCall {
        session: s.id.clone(),
        labels: s.labels.clone(),
        server: server.to_string(),
        method: m,
        tool,
        arguments: args,
    };
    let result = match method {
        "initialize" => json!({
            "protocolVersion": msg["params"]["protocolVersion"].as_str().unwrap_or("2025-06-18"),
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": { "name": server, "version": "0" },
            "instructions": crate::ops::instructions(),
        }),
        "ping" => json!({}),
        "tools/list" => {
            let c = call(McpMethod::ToolsList, None, None);
            match crate::ops::answer_verified(office, &c, "tools", json!({})).await {
                Ok(tools) => json!({ "tools": tools }),
                Err(e) => return Some(rpc_error(id, -32603, format!("{e:#}"))),
            }
        }
        "tools/call" => {
            let tool = msg["params"]["name"].as_str().unwrap_or("").to_string();
            let args = msg["params"].get("arguments").cloned().unwrap_or(json!({}));
            let c = call(McpMethod::ToolsCall, Some(tool.clone()), Some(args.clone()));
            let (text, is_error) = match crate::ops::answer_verified(office, &c, &tool, args).await
            {
                Ok(v) => (serde_json::to_string_pretty(&v).unwrap_or_default(), false),
                Err(e) => (format!("{e:#}"), true),
            };
            json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })
        }
        other => return Some(rpc_error(id, -32601, format!("method {other} not found"))),
    };
    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

fn rpc_error(id: Value, code: i64, message: String) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}
