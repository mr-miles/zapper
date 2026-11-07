//! The office on OpenShell (phase 1 of `docs/openshell-migration.md`): an
//! [`OfficeClient`] over an OpenShell gateway (v0.1.7) and its compute
//! driver (openshell-firecracker), in place of the zapper daemon. The firm
//! logic above the trait is unchanged.
//!
//! What this phase covers:
//!
//! - **Sessions.** A zapper profile becomes a sandbox template (image, CPU,
//!   memory), made once per office process; a session is a sandbox created
//!   from it, with the gateway's providers attached (the Claude token is the
//!   `anthropic` provider's, injected outside the VM). Once it is ready the
//!   office writes what zapper's guest agent wrote at boot: the harness's
//!   files (instructions, settings, MCP servers, turn-end hook), the
//!   profile's and the session's files, and copies of the directories it
//!   mounted (skills, the work directory), then runs the profile's `setup`.
//! - **Driving.** `attach` is `ExecSandboxInteractive`, translated to and
//!   from the frames the foreman and the drive's tar copies speak, so both
//!   work over it unchanged; `connect_port` is `ForwardTcp`.
//! - **Host room.** `admit` keeps the session limit and the memory check.
//! - **The office's tools and git** (phase 2) are the office edge's
//!   (`crate::edge`), which knows each sandbox by the UUID recorded here:
//!   the facilities the daemon would have connected register as ids only,
//!   Claude Code's `office` MCP server is the edge's Streamable HTTP, and
//!   every command gets `ZAPPER_OFFICE_URL` for the `office` command.
//!
//! What it does not do yet (later phases): rules are kept, and `allowed`
//! judges by them, but nothing enforces them at the network (phase 3: the
//! sandboxes' policy through `UpdateConfig`); the traffic log is empty
//! (phase 3: `GetSandboxLogs`); the officers' decider is served on its
//! socket but nothing asks it (phase 3: drafts).
//!
//! Differences from zapper a firm can see: commands run as the sandbox user,
//! never root (`ShellRequest::system` is ignored), so files the office writes
//! outside the user's reach must already be in the image with the same
//! content (`scripts/openshell-image.sh` puts them there); secrets are the
//! gateway's providers, so `{{secret:…}}` values in a profile's env and
//! files are left out; and there is no per-exec approval or terminal
//! recording.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, bail, Context, Result};
use futures_util::future::BoxFuture;
use futures_util::stream::{Stream, StreamExt};
use openshell_sdk::raw::proto;
use openshell_sdk::{
    ClientConfig, ClientIdentity, DeleteOptions, ListOptions, OpenShellClient, SandboxPhase,
    SandboxTemplateCreateSpec, SdkError,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream};
use tokio::sync::mpsc;
use zapper_api_types::{NewSession, ServiceSpec, SessionInfo, Status};
use zapper_driver::process::DriveStream;
use zapper_policy::proto::{Frame, GuestFile, ShellRequest};
use zapper_policy::{
    expand_home, secret_refs, AnyRule, Config, Mount, NetRequest, NetRule, Policy, ResolvedProfile,
    Scope,
};

use crate::client::{block, OfficeClient};

/// The gateway workspace the office's sandboxes and templates live in.
const WORKSPACE: &str = "default";
/// The most stdout or stderr one frame carries (under `MAX_FRAME`).
const CHUNK: usize = 60 * 1024;
/// The buffer between a command's gRPC stream and the office's end of it.
const PIPE: usize = 256 * 1024;
/// How long a sandbox may take to become ready, by default.
const READY_TIMEOUT: Duration = Duration::from_secs(300);
/// The most a setup command may print.
const SETUP_OUTPUT: u64 = 1 << 20;

/// Writes stdin to `$1` with mode `$2`, as the sandbox user. A path the user
/// can't write (outside its home, the work directory and `/tmp`) is accepted
/// only if the image already has it with this content.
const WRITE_FILE: &str = r#"p=$1; m=$2; d=${p%/*}; mkdir -p "$d" 2>/dev/null
if { [ -e "$p" ] && [ -w "$p" ]; } || { [ ! -e "$p" ] && [ -w "$d" ]; }; then
  cat > "$p" && chmod "$m" "$p"
else
  cmp -s - "$p" || { echo "$p: $(id -un) can't write it, and the image doesn't have it with this content" >&2; exit 1; }
fi"#;

/// Unpacks a tar on stdin into the directory `$1`.
const UNPACK: &str = r#"mkdir -p "$1" && tar -C "$1" -xf -"#;

/// The office's OpenShell settings (`openshell.yaml`, beside `office.yaml`):
/// which gateway, how to reach it, and what to give its sandboxes.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// The gateway's URL, e.g. `https://127.0.0.1:8080`.
    pub gateway: String,
    /// The gateway's CA certificate (PEM), for a private CA.
    pub ca_cert: Option<String>,
    /// The office's client certificate and key (PEM), for mutual TLS.
    pub client_cert: Option<String>,
    pub client_key: Option<String>,
    /// Skip verifying the gateway's certificate (development only).
    pub insecure_skip_verify: bool,
    /// The OCI image for each zapper image a profile names; by default
    /// `localhost/zapper-openshell-<image>` (`scripts/openshell-image.sh`).
    pub images: BTreeMap<String, String>,
    /// Gateway providers attached to every sandbox (e.g. `anthropic`).
    pub providers: Vec<String>,
    /// The most sandboxes at once; by default `daemon.max_sessions` of the
    /// zapper config.
    pub max_sessions: Option<usize>,
    /// How long a sandbox may take to become ready; by default 300.
    pub ready_timeout_secs: Option<u64>,
    /// The office edge: where the firm's workers reach the office's tools
    /// and git, and how it knows them (`crate::edge`).
    pub edge: EdgeSettings,
}

/// The office edge's settings (`edge:` in `openshell.yaml`). The defaults
/// suit a gateway with the Firecracker driver on this host, with supervisor
/// identity on and the `office-edge` provider (`config/openshell-office-edge.yaml`)
/// attached to the sandboxes.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EdgeSettings {
    /// Where the edge listens: on loopback, as the supervisors dial
    /// `host.openshell.internal` there, and the token endpoint must be
    /// loopback to be plain HTTP.
    pub listen: std::net::SocketAddr,
    /// The edge's URL as the workers use it.
    pub url: String,
    /// The token endpoint's URL as the provider profile names it: the
    /// audience the supervisors' SVIDs carry.
    pub token_url: String,
    /// The driver's JWKS, whose keys sign the SVIDs.
    pub jwks: String,
    /// The SVIDs' issuer.
    pub issuer: String,
    /// The SPIFFE trust domain of the sandboxes' IDs.
    pub trust_domain: String,
}

impl Default for EdgeSettings {
    fn default() -> Self {
        EdgeSettings {
            listen: "127.0.0.1:8970".parse().unwrap(),
            url: "http://host.openshell.internal:8970".into(),
            token_url: "http://127.0.0.1:8970/oauth/token".into(),
            jwks: "http://127.0.0.1:8961/.well-known/jwks.json".into(),
            issuer: "http://127.0.0.1:8961".into(),
            trust_domain: "openshell.local".into(),
        }
    }
}

impl Settings {
    pub fn from_yaml(text: &str) -> Result<Settings> {
        let s: Settings = serde_yaml::from_str(text)?;
        if s.gateway.is_empty() {
            bail!("gateway: give the OpenShell gateway's URL");
        }
        if s.client_cert.is_some() != s.client_key.is_some() {
            bail!("client_cert and client_key go together");
        }
        Ok(s)
    }

    pub fn load(path: &Path) -> Result<Settings> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Settings::from_yaml(&text).with_context(|| format!("loading {}", path.display()))
    }

    /// The OCI image for zapper image `name`.
    pub fn image(&self, name: &str) -> String {
        self.images
            .get(name)
            .cloned()
            .unwrap_or_else(|| format!("localhost/zapper-openshell-{name}"))
    }

    fn ready_timeout(&self) -> Duration {
        self.ready_timeout_secs
            .map(Duration::from_secs)
            .unwrap_or(READY_TIMEOUT)
    }
}

/// A sandbox the office made: what the session surface reports and what each
/// command in it needs.
struct Sandbox {
    info: SessionInfo,
    /// The gateway's id for it (a UUID): what its supervisor's SPIFFE ID
    /// names, and so how the edge knows the caller.
    uuid: String,
    /// The environment every command gets (the guest's, in zapper).
    env: HashMap<String, String>,
    /// Where the agent's commands start.
    workdir: String,
    /// The profile's command, for a shell that names none.
    command: Vec<String>,
    /// The session's own rule layer (kept, not yet enforced: phase 3).
    rules: Policy,
}

/// The office's client of an OpenShell gateway.
pub struct OpenShell {
    sdk: OpenShellClient,
    settings: Settings,
    /// The zapper config the profiles come from, and when it was read.
    config_path: PathBuf,
    config: Mutex<(Option<SystemTime>, Config)>,
    /// Templates made by this process: name → what they were made from.
    templates: tokio::sync::Mutex<BTreeMap<String, String>>,
    sandboxes: Mutex<BTreeMap<String, Sandbox>>,
    /// Firm roles' shared layers, by name (kept, not yet enforced: phase 3).
    layers: Mutex<BTreeMap<String, Policy>>,
    /// Ids for the registrations phase 2 replaces.
    next_id: AtomicU64,
    warned_rules: AtomicBool,
}

impl OpenShell {
    /// Connects to the gateway `settings` names, with profiles from the zapper
    /// config at `config_path`.
    pub async fn connect(settings: Settings, config_path: PathBuf) -> Result<OpenShell> {
        let read = |p: &str| {
            let p = expand_home(p);
            std::fs::read(&p).with_context(|| format!("reading {p}"))
        };
        let mut cc = ClientConfig::new(settings.gateway.clone());
        if let Some(p) = &settings.ca_cert {
            cc.ca_cert = Some(read(p)?);
        }
        if let (Some(c), Some(k)) = (&settings.client_cert, &settings.client_key) {
            cc.client_identity = Some(ClientIdentity::new(read(c)?, read(k)?));
        }
        cc.insecure_skip_verify = settings.insecure_skip_verify;
        let sdk = OpenShellClient::connect(cc).await.with_context(|| {
            format!(
                "connecting to the OpenShell gateway at {}",
                settings.gateway
            )
        })?;
        let health = sdk
            .health()
            .await
            .with_context(|| format!("the OpenShell gateway at {}", settings.gateway))?;
        tracing::info!(gateway = %settings.gateway, version = %health.version, "connected to OpenShell");
        let config = load_config(&config_path)?;
        Ok(OpenShell {
            sdk,
            settings,
            config: Mutex::new((modified(&config_path), config)),
            config_path,
            templates: Default::default(),
            sandboxes: Default::default(),
            layers: Default::default(),
            next_id: AtomicU64::new(1),
            warned_rules: AtomicBool::new(false),
        })
    }

    /// The office's sandboxes, as the VMs view lists them.
    pub fn list(&self) -> Vec<Value> {
        self.sandboxes
            .lock()
            .unwrap()
            .values()
            .map(|s| {
                let mut v = serde_json::to_value(&s.info).unwrap_or_default();
                v["terminals"] = json!([]);
                v
            })
            .collect()
    }

    async fn running(&self) -> usize {
        match self.sdk.list_all_sandboxes(ListOptions::default()).await {
            Ok(all) => all.len(),
            Err(e) => {
                tracing::debug!("listing sandboxes: {e}");
                self.sandboxes.lock().unwrap().len()
            }
        }
    }

    /// The session limit and the host's memory, as the daemon checks them.
    async fn has_room(&self, profile: &str) -> Result<Config> {
        let config = self.config();
        let resolved = config.resolve(profile)?;
        let max = self
            .settings
            .max_sessions
            .unwrap_or(config.daemon.max_sessions);
        let running = self.running().await;
        if running >= max {
            bail!("{running} sandboxes running; max_sessions is {max}");
        }
        let given: u64 = self
            .sandboxes
            .lock()
            .unwrap()
            .values()
            .filter(|s| s.info.status.is_live())
            .map(|s| s.info.memory_mib as u64)
            .sum::<u64>()
            + resolved.engine.memory_mib as u64;
        if let (Some(ratio), Some(total)) =
            (config.daemon.memory_overcommit, host_memory_mib("MemTotal"))
        {
            let limit = (total as f64 * ratio) as u64;
            if given > limit {
                bail!(
                    "the sandboxes would be given {given} MiB of memory ({} MiB for this one), \
                     past daemon.memory_overcommit: {ratio} of the host's {total} MiB ({limit} MiB)",
                    resolved.engine.memory_mib
                );
            }
        }
        Ok(config)
    }

    /// The template for `profile`, made (again) if this process hasn't made
    /// it from the profile as it is now.
    async fn template(&self, profile: &ResolvedProfile) -> Result<String> {
        let name = template_name(&profile.name);
        let image = self.settings.image(&profile.engine.image);
        let from = format!(
            "{image} {} {}",
            profile.engine.vcpus, profile.engine.memory_mib
        );
        let mut made = self.templates.lock().await;
        if made.get(&name) == Some(&from) {
            return Ok(name);
        }
        let template = proto::SandboxWorkloadTemplate {
            metadata: Some(proto::ObjectMeta {
                name: name.clone(),
                labels: HashMap::from([("zapper-office".to_string(), "1".to_string())]),
                ..Default::default()
            }),
            spec: Some(proto::SandboxWorkloadTemplateSpec {
                workload: Some(proto::SandboxWorkloadConfig {
                    image,
                    resources: Some(proto::SandboxResources {
                        cpu: profile.engine.vcpus.to_string(),
                        memory: format!("{}Mi", profile.engine.memory_mib),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
        };
        match self.sdk.create_sandbox_template(template.clone()).await {
            Ok(_) => {}
            // Made by an earlier office, perhaps from the profile as it was:
            // made again from it as it is.
            Err(SdkError::AlreadyExists { .. }) => {
                self.sdk
                    .delete_sandbox_template(
                        &name,
                        DeleteOptions {
                            allow_missing: true,
                        },
                    )
                    .await
                    .with_context(|| format!("replacing template {name}"))?;
                self.sdk
                    .create_sandbox_template(template)
                    .await
                    .with_context(|| format!("replacing template {name}"))?;
            }
            Err(e) => return Err(anyhow::Error::from(e).context(format!("making template {name}"))),
        }
        made.insert(name.clone(), from);
        Ok(name)
    }

    /// Waits for sandbox `name` and sets it up as zapper's guest agent would
    /// have: the environment, files, directories and setup commands of
    /// `profile`. The environment and work directory its commands get.
    async fn boot(
        &self,
        name: &str,
        profile: &ResolvedProfile,
        new: &NewSession,
    ) -> Result<(HashMap<String, String>, String)> {
        self.sdk
            .wait_ready(name, self.settings.ready_timeout())
            .await
            .with_context(|| format!("waiting for sandbox {name}"))?;
        let agent = &profile.agent;
        let home = format!("/home/{}", agent.user);
        let harness = zapper_driver::harness::get(&agent.kind)
            .ok_or_else(|| anyhow!("agent.kind {:?} is not a harness zapper knows", agent.kind))?;

        // The environment: the profile's, then the harness's.
        let mut left_out = Vec::new();
        let mut env = BTreeMap::new();
        for (k, v) in &agent.env {
            if secret_refs(v).is_empty() {
                env.insert(k.clone(), v.clone());
            } else {
                left_out.push(format!("agent.env.{k}"));
            }
        }
        env.insert("ZAPPER_SESSION".into(), name.to_string());
        // `office` (the CLI, the stop hook, the stdio bridge for harnesses
        // without HTTP MCP) reaches the edge here.
        env.insert("ZAPPER_OFFICE_URL".into(), self.mcp_url());
        let mut files = Vec::new();
        harness.guest(profile, &home, &mut env, &mut files);

        // `files` by guest path and `config_files` in the harness's config
        // directory, then the session's own.
        let config_dir = harness.config_dir(&home);
        let named = agent.files.iter().map(|(p, c)| (p.clone(), c)).chain(
            agent
                .config_files
                .iter()
                .map(|(p, c)| (format!("{config_dir}/{}", p.trim_start_matches('/')), c)),
        );
        for (path, content) in named {
            let path = match path.strip_prefix("~/") {
                Some(rest) => format!("{home}/{rest}"),
                None => path,
            };
            if !secret_refs(content).is_empty() {
                left_out.push(path);
                continue;
            }
            files.push(GuestFile {
                user_owned: path.starts_with(&home),
                path,
                content: content.clone(),
                mode: 0o644,
            });
        }
        files.extend(new.files.iter().cloned());
        if !left_out.is_empty() {
            tracing::warn!(
                sandbox = name,
                "left out, as they name secrets (the gateway's providers hold credentials): {}",
                left_out.join(", ")
            );
        }
        for f in &files {
            self.write_file(name, f)
                .await
                .with_context(|| format!("writing {}", f.path))?;
        }

        // The directories zapper mounted, copied as zapper copied them (a
        // session's changes stay in it): the profile's and the session's
        // mounts, and the harness's (skills, config directories).
        let workdir = new.workdir.to_string_lossy().into_owned();
        let mounts: Vec<Mount> = profile
            .mounts
            .iter()
            .chain(new.mounts.iter())
            .cloned()
            .chain(harness.mounts(agent, &home))
            .collect();
        for m in &mounts {
            let host = expand_home(&m.host.replace("{workdir}", &workdir));
            self.copy_dir(name, Path::new(&host), &m.guest)
                .await
                .with_context(|| format!("copying {host} to {}", m.guest))?;
        }

        let env: HashMap<String, String> = env.into_iter().collect();
        for cmd in &profile.setup {
            let argv = vec!["bash".to_string(), "-c".to_string(), cmd.clone()];
            let (code, _, err) = self
                .exec(
                    name,
                    argv,
                    env.clone(),
                    String::new(),
                    Vec::new(),
                    SETUP_OUTPUT,
                )
                .await?;
            if code != 0 {
                bail!("setup {cmd:?} exited {code}: {}", err.trim());
            }
        }
        Ok((env, agent.workdir.clone()))
    }

    async fn write_file(&self, name: &str, f: &GuestFile) -> Result<()> {
        let mode = format!("{:o}", f.mode);
        let argv = ["sh", "-c", WRITE_FILE, "sh", f.path.as_str(), mode.as_str()]
            .map(String::from)
            .to_vec();
        let input = f.content.clone().into_bytes();
        let (code, _, err) = self
            .exec(name, argv, HashMap::new(), String::new(), input, 0)
            .await?;
        if code != 0 {
            bail!("exit {code}: {}", err.trim());
        }
        Ok(())
    }

    async fn copy_dir(&self, name: &str, host: &Path, guest: &str) -> Result<()> {
        if !host.is_dir() {
            bail!("not a directory");
        }
        let tar = tokio::process::Command::new("tar")
            .arg("-C")
            .arg(host)
            .args(["-cf", "-", "."])
            .output()
            .await
            .context("running tar")?;
        if !tar.status.success() {
            bail!("tar: {}", String::from_utf8_lossy(&tar.stderr).trim());
        }
        let argv = ["sh", "-c", UNPACK, "sh", guest].map(String::from).to_vec();
        let (code, _, err) = self
            .exec(
                name,
                argv,
                HashMap::new(),
                String::new(),
                tar.stdout,
                SETUP_OUTPUT,
            )
            .await?;
        if code != 0 {
            bail!("exit {code}: {}", err.trim());
        }
        Ok(())
    }

    /// Runs `argv` in sandbox `name` with `stdin`: its exit code, stdout (at
    /// most `limit` bytes) and stderr. Over the same stream as `attach`.
    async fn exec(
        &self,
        name: &str,
        argv: Vec<String>,
        env: HashMap<String, String>,
        workdir: String,
        stdin: Vec<u8>,
        limit: u64,
    ) -> Result<(i32, Vec<u8>, String)> {
        let stream = self
            .open(exec_request(name, argv, env, workdir, None))
            .await?;
        let mut out = Vec::new();
        let input = Some(std::io::Cursor::new(stdin));
        let (code, err) = crate::drive::pump_command(stream, input, &mut out, limit).await?;
        Ok((code, out, err))
    }

    /// Starts `start` with `ExecSandboxInteractive`; the frame stream the
    /// office drives it over.
    async fn open(&self, start: proto::ExecSandboxRequest) -> Result<Box<dyn DriveStream>> {
        let sandbox = start.sandbox.clone();
        let (tx, rx) = mpsc::channel(16);
        let first = proto::ExecSandboxInput {
            payload: Some(proto::exec_sandbox_input::Payload::Start(start)),
        };
        tx.send(first)
            .await
            .map_err(|_| anyhow!("the command's input closed"))?;
        let mut grpc = self.sdk.raw_grpc_fresh().await?;
        let events = grpc
            .exec_sandbox_interactive(receiver_stream(rx))
            .await
            .with_context(|| format!("running a command in sandbox {sandbox}"))?
            .into_inner();
        let (ours, theirs) = tokio::io::duplex(PIPE);
        tokio::spawn(pump_exec(theirs, tx, events));
        Ok(Box::new(ours))
    }

    fn warn_rules_once(&self) {
        if !self.warned_rules.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                "network rules are kept but not enforced on OpenShell yet (phase 3): \
                 the sandboxes' policy is the gateway's"
            );
        }
    }

    /// A facility the daemon would have connected the sandboxes to: on
    /// OpenShell the office edge serves it (the tools, git), so registering
    /// it is only an id.
    fn registered(&self, what: &str) -> u64 {
        tracing::debug!("{what}: served through the office edge");
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// The edge's settings.
    pub fn edge_settings(&self) -> &EdgeSettings {
        &self.settings.edge
    }

    /// The office's MCP tools as the workers reach them (Streamable HTTP).
    fn mcp_url(&self) -> String {
        format!("{}/mcp", self.settings.edge.url.trim_end_matches('/'))
    }

    /// Points the profile's `office` MCP server at the edge: Claude Code
    /// speaks Streamable HTTP to it itself (its requests get the worker's
    /// token outside the VM); other harnesses keep `office mcp`, which
    /// bridges their stdio to it (`ZAPPER_OFFICE_URL`).
    fn office_over_http(&self, profile: &mut ResolvedProfile) {
        office_over_http(profile, &self.mcp_url());
    }

    /// The office's sandbox the gateway knows as `uuid` (a supervisor's
    /// SPIFFE ID names it): its name and the labels the office gave it.
    pub fn caller(&self, uuid: &str) -> Option<(String, BTreeMap<String, String>)> {
        self.sandboxes
            .lock()
            .unwrap()
            .iter()
            .find(|(_, s)| s.uuid == uuid)
            .map(|(name, s)| (name.clone(), s.info.labels.clone()))
    }
}

impl OfficeClient for OpenShell {
    fn add_layer_rule(&self, name: &str, rule: AnyRule) -> AnyRule {
        self.warn_rules_once();
        self.layers
            .lock()
            .unwrap()
            .entry(name.to_string())
            .or_default()
            .insert(rule.clone(), Some(0));
        rule
    }

    fn remove_layer_rule(&self, name: &str, id: &str) -> bool {
        self.layers
            .lock()
            .unwrap()
            .get_mut(name)
            .and_then(|p| p.remove(id))
            .is_some()
    }

    fn reset_layer(&self, name: &str) {
        self.layers
            .lock()
            .unwrap()
            .insert(name.to_string(), Policy::default());
    }

    fn record_writable(&self) -> bool {
        // The record is the office's own (its journals and transcripts);
        // there is no daemon record to wait for.
        true
    }

    fn config(&self) -> Config {
        let now = modified(&self.config_path);
        let mut c = self.config.lock().unwrap();
        if now != c.0 {
            match load_config(&self.config_path) {
                Ok(config) => *c = (now, config),
                Err(e) => tracing::warn!("{e:#}; keeping the config as it was"),
            }
        }
        c.1.clone()
    }

    fn session_count(&self) -> usize {
        block(self.running())
    }

    fn admit(&self, profile: &str) -> Result<Config> {
        block(self.has_room(profile))
    }

    fn promote_rule(&self, _what: &str, _scope: &Scope, _rule: NetRule) -> Result<AnyRule> {
        bail!("promoting a rule into the config isn't supported on OpenShell yet (phase 3)")
    }

    fn create_session<'a>(&'a self, new: NewSession) -> BoxFuture<'a, Result<SessionInfo>> {
        Box::pin(async move {
            if new.persistent {
                bail!("persistent sessions aren't supported on OpenShell");
            }
            if !new.forwards.is_empty() {
                bail!("port forwards aren't supported on OpenShell yet (phase 4)");
            }
            let config = self.has_room(&new.profile).await?;
            let mut profile = config.resolve(&new.profile)?;
            if let Some(overlay) = &new.agent_overlay {
                let mut agent = serde_json::to_value(&profile.agent)?;
                zapper_policy::merge(&mut agent, overlay);
                profile.agent = serde_json::from_value(agent).context("agent overlay")?;
            }
            self.office_over_http(&mut profile);
            let template = self.template(&profile).await?;
            let name = format!("zo-{:012x}", rand::random::<u64>() >> 16);
            let spec = SandboxTemplateCreateSpec {
                name: Some(name.clone()),
                template_name: template.clone(),
                labels: gateway_labels(&new.labels),
                providers: self.settings.providers.clone(),
                ..Default::default()
            };
            let created = self
                .sdk
                .create_sandbox_from_template(spec)
                .await
                .with_context(|| format!("creating a sandbox from template {template}"))?;
            let (env, workdir) = match self.boot(&name, &profile, &new).await {
                Ok(booted) => booted,
                Err(e) => {
                    let gone = DeleteOptions {
                        allow_missing: true,
                    };
                    if let Err(d) = self.sdk.delete_sandbox(&name, gone).await {
                        tracing::warn!(sandbox = %name, "deleting it after a failed boot: {d}");
                    }
                    return Err(e.context(format!("sandbox {name}")));
                }
            };
            let info = SessionInfo {
                id: name.clone(),
                profile: new.profile.clone(),
                status: Status::Running,
                created: zapper_util::now(),
                expires: None,
                persistent: false,
                ssh: false,
                disk_bytes: None,
                image: self.settings.image(&profile.engine.image),
                vcpus: profile.engine.vcpus,
                memory_mib: profile.engine.memory_mib,
                kernel: "openshell".into(),
                network: profile.engine.network,
                user: profile.agent.user.clone(),
                root: false,
                workdir: workdir.clone(),
                counters: json!({}),
                usage: None,
                rules: new.policy.rules().len(),
                firm: new.labels.get("firm").cloned(),
                member: new.labels.get("member").cloned(),
                labels: new.labels.clone(),
                driver: Value::Null,
            };
            if !new.policy.is_empty() {
                self.warn_rules_once();
            }
            self.sandboxes.lock().unwrap().insert(
                name,
                Sandbox {
                    info: info.clone(),
                    uuid: created.id,
                    env,
                    workdir,
                    command: profile.agent.command.clone(),
                    rules: new.policy.clone(),
                },
            );
            Ok(info)
        })
    }

    fn stop_session<'a>(&'a self, id: &'a str, reason: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let known = self.sandboxes.lock().unwrap().remove(id).is_some();
            if !known {
                bail!("no session {id}");
            }
            tracing::info!(sandbox = id, reason, "deleting the sandbox");
            self.sdk
                .delete_sandbox(
                    id,
                    DeleteOptions {
                        allow_missing: true,
                    },
                )
                .await
                .with_context(|| format!("deleting sandbox {id}"))?;
            Ok(())
        })
    }

    fn session(&self, id: &str) -> Result<SessionInfo> {
        let mut info = self
            .sandboxes
            .lock()
            .unwrap()
            .get(id)
            .map(|s| s.info.clone())
            .ok_or_else(|| anyhow!("no session {id}"))?;
        match block(self.sdk.get_sandbox(id)) {
            Ok(s) => info.status = status_of(s.phase),
            Err(SdkError::NotFound { .. }) => {
                info.status = Status::Stopped("gone from the gateway".into())
            }
            Err(e) => tracing::debug!(sandbox = id, "reading its phase: {e}"),
        }
        Ok(info)
    }

    fn attach<'a>(
        &'a self,
        id: &'a str,
        req: ShellRequest,
    ) -> BoxFuture<'a, Result<Box<dyn DriveStream>>> {
        Box::pin(async move {
            if req.persist || req.attach.is_some() {
                bail!("persistent terminals aren't supported on OpenShell");
            }
            let (mut env, workdir, command) = {
                let all = self.sandboxes.lock().unwrap();
                let s = all.get(id).ok_or_else(|| anyhow!("no session {id}"))?;
                (s.env.clone(), s.workdir.clone(), s.command.clone())
            };
            env.extend(req.env);
            let argv = if req.argv.is_empty() {
                command
            } else {
                req.argv
            };
            // A `system` command (zapper: root, unsupervised; the office's
            // file copies) runs as the sandbox user, from no directory of
            // the agent's.
            let workdir = if req.system { String::new() } else { workdir };
            self.open(exec_request(id, argv, env, workdir, req.tty))
                .await
        })
    }

    fn connect_port<'a>(
        &'a self,
        id: &'a str,
        port: u32,
    ) -> BoxFuture<'a, Result<Box<dyn DriveStream>>> {
        Box::pin(async move {
            use proto::tcp_forward_frame::Payload;
            if !self.sandboxes.lock().unwrap().contains_key(id) {
                bail!("no session {id}");
            }
            let (tx, rx) = mpsc::channel(16);
            let init = proto::TcpForwardInit {
                sandbox: id.to_string(),
                workspace: WORKSPACE.into(),
                target: Some(proto::tcp_forward_init::Target::Tcp(
                    proto::TcpRelayTarget {
                        host: "127.0.0.1".into(),
                        port,
                    },
                )),
                ..Default::default()
            };
            tx.send(proto::TcpForwardFrame {
                payload: Some(Payload::Init(init)),
            })
            .await
            .map_err(|_| anyhow!("the forward's input closed"))?;
            let mut grpc = self.sdk.raw_grpc_fresh().await?;
            let frames = grpc
                .forward_tcp(receiver_stream(rx))
                .await
                .with_context(|| format!("forwarding port {port} of sandbox {id}"))?
                .into_inner();
            let (ours, theirs) = tokio::io::duplex(PIPE);
            tokio::spawn(pump_tcp(theirs, tx, frames));
            Ok(Box::new(ours) as Box<dyn DriveStream>)
        })
    }

    fn add_mcp_socket(
        &self,
        name: &str,
        _sessions: BTreeMap<String, String>,
        _socket: &str,
        _instructions: Option<String>,
        _owner: &str,
    ) -> Result<u64> {
        Ok(self.registered(&format!("MCP server {name}")))
    }

    fn remove_mcp_server(&self, _id: u64) -> bool {
        true
    }

    fn add_decider_socket(
        &self,
        _sessions: BTreeMap<String, String>,
        _socket: &str,
    ) -> Result<u64> {
        Ok(self.registered("the officers' decider"))
    }

    fn remove_decider(&self, _id: u64) -> bool {
        true
    }

    fn add_service(&self, spec: ServiceSpec) -> Result<u64> {
        Ok(self.registered(&format!("service {}", spec.name)))
    }

    fn add_zone(
        &self,
        domain: &str,
        _sessions: BTreeMap<String, String>,
        _owner: &str,
    ) -> Result<u64> {
        Ok(self.registered(&format!("zone {domain}")))
    }

    fn remove_service(&self, _id: u64) {}

    fn requests(&self, _id: &str) -> Vec<Value> {
        // Phase 3 reads the sandbox's network events (`GetSandboxLogs`).
        Vec::new()
    }

    fn put_session_rule(&self, id: &str, rule: AnyRule) {
        self.warn_rules_once();
        if let Some(s) = self.sandboxes.lock().unwrap().get_mut(id) {
            if let Some(rule_id) = rule.id() {
                s.rules.remove(rule_id);
            }
            s.rules.insert(rule, Some(0));
            s.info.rules = s.rules.rules().len();
        }
    }

    fn remove_session_rule(&self, id: &str, rule_id: &str) -> bool {
        let mut all = self.sandboxes.lock().unwrap();
        let Some(s) = all.get_mut(id) else {
            return false;
        };
        let removed = s.rules.remove(rule_id).is_some();
        s.info.rules = s.rules.rules().len();
        removed
    }

    fn edge(&self) -> Option<String> {
        Some(self.settings.edge.url.clone())
    }

    fn allowed(&self, id: &str, req: NetRequest) -> Option<String> {
        // The layers the daemon would judge by, most specific first: the
        // session's, its role's, its profile's, the global one.
        let (own, profile, role) = {
            let all = self.sandboxes.lock().unwrap();
            let s = all.get(id)?;
            let role = s.info.labels.get("firm-role").cloned();
            (s.rules.clone(), s.info.profile.clone(), role)
        };
        let config = self.config();
        let mut layers: Vec<(Scope, Policy)> = vec![(Scope::Session(id.to_string()), own)];
        if let Some(role) = role {
            let p = self.layers.lock().unwrap().get(&role).cloned();
            layers.push((Scope::Role(role), p.unwrap_or_default()));
        }
        layers.extend(config.resolve(&profile).ok()?.layers);
        layers.push((Scope::Global, config.policy.clone()));
        let layers: Vec<_> = layers.iter().map(|(s, p)| (s.clone(), p)).collect();
        let (d, _) = zapper_policy::evaluate_net(&layers, &req);
        (d.action == zapper_policy::Action::Allow)
            .then(|| d.rule.clone().unwrap_or_else(|| "default".into()))
    }
}

/// The office's own routes on OpenShell that the daemon served before: the
/// VMs view's session list.
pub fn routes(os: Arc<OpenShell>) -> axum::Router {
    axum::Router::new().route(
        "/sessions",
        axum::routing::get(move || {
            let os = os.clone();
            async move { axum::Json(os.list()) }
        }),
    )
}

fn load_config(path: &Path) -> Result<Config> {
    if !path.exists() {
        return Ok(Config::default());
    }
    Config::from_yaml(&std::fs::read_to_string(path)?)
        .with_context(|| format!("loading {}", path.display()))
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// A `/proc/meminfo` value in MiB.
fn host_memory_mib(key: &str) -> Option<u64> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    text.lines().find_map(|l| {
        let rest = l.strip_prefix(key)?.strip_prefix(':')?;
        let kib: u64 = rest.trim().trim_end_matches("kB").trim().parse().ok()?;
        Some(kib / 1024)
    })
}

/// A session's state from its sandbox's phase.
fn status_of(phase: SandboxPhase) -> Status {
    match phase {
        SandboxPhase::Ready => Status::Running,
        SandboxPhase::Provisioning
        | SandboxPhase::Starting
        | SandboxPhase::Unspecified
        | SandboxPhase::Unknown => Status::Starting,
        SandboxPhase::Stopping | SandboxPhase::Deleting | SandboxPhase::Suspending => {
            Status::Stopping("the gateway is stopping it".into())
        }
        SandboxPhase::Error => Status::Failed("the sandbox is in its error phase".into()),
        _ => Status::Stopped(format!("the sandbox is {phase:?}").to_lowercase()),
    }
}

/// A template's name for profile `profile`: a DNS label.
/// [`OpenShell::office_over_http`], with the tools at `mcp_url`.
fn office_over_http(profile: &mut ResolvedProfile, mcp_url: &str) {
    if profile.agent.kind != "claude-code" {
        return;
    }
    if let Some(Value::Object(servers)) = &mut profile.agent.mcp_servers {
        if servers.contains_key(crate::ops::SERVER) {
            servers.insert(
                crate::ops::SERVER.into(),
                json!({ "type": "http", "url": mcp_url }),
            );
        }
    }
}

fn template_name(profile: &str) -> String {
    let s: String = profile
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let s = format!("zapper-{}", s.trim_matches('-'));
    s.chars()
        .take(63)
        .collect::<String>()
        .trim_end_matches('-')
        .to_string()
}

/// The labels the gateway gets: which firm, run and worker (the office keeps
/// all of a session's labels itself), as label values.
fn gateway_labels(labels: &BTreeMap<String, String>) -> HashMap<String, String> {
    let mut out = HashMap::from([("zapper-office".to_string(), "1".to_string())]);
    for k in ["firm", "run", "member"] {
        if let Some(v) = labels.get(k).map(|v| label_value(v)) {
            if !v.is_empty() {
                out.insert(format!("zapper-{k}"), v);
            }
        }
    }
    out
}

/// `v` as a label value: at most 63 of `[A-Za-z0-9._-]`, alphanumeric at
/// both ends.
fn label_value(v: &str) -> String {
    let s: String = v
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .take(63)
        .collect();
    s.trim_matches(|c: char| !c.is_ascii_alphanumeric())
        .to_string()
}

fn exec_request(
    sandbox: &str,
    argv: Vec<String>,
    env: HashMap<String, String>,
    workdir: String,
    tty: Option<(u16, u16)>,
) -> proto::ExecSandboxRequest {
    proto::ExecSandboxRequest {
        sandbox: sandbox.to_string(),
        workspace_scope: Some(proto::workspace_selector(WORKSPACE)),
        command: argv,
        workdir,
        environment: env,
        tty: tty.is_some(),
        rows: tty.map_or(0, |(rows, _)| rows.into()),
        cols: tty.map_or(0, |(_, cols)| cols.into()),
        // The argv as given, as zapper runs it: no login files first.
        no_login_shell: true,
        ..Default::default()
    }
}

/// A channel's receiver as the request stream of a streaming call; the
/// stream ends (the command's stdin closes) when every sender is gone.
fn receiver_stream<T: Send + 'static>(rx: mpsc::Receiver<T>) -> impl Stream<Item = T> + Send {
    futures_util::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|m| (m, rx)) })
}

/// Writes `data` to `w` as frames of at most [`CHUNK`] bytes.
async fn write_frames(
    w: &mut (impl AsyncWrite + Unpin),
    data: &[u8],
    frame: fn(Vec<u8>) -> Frame,
) -> std::io::Result<()> {
    for c in data.chunks(CHUNK) {
        w.write_all(&frame(c.to_vec()).encode()).await?;
    }
    Ok(())
}

/// Carries one interactive exec between the office's end of `pipe` (frames,
/// as a zapper terminal speaks them) and the gateway: stdin and resizes go
/// to `tx`; stdout, stderr and the exit status come from `events`. `Eof`
/// closes the command's stdin; a hang-up, or the office letting go of the
/// stream, ends the call, and with it the command.
async fn pump_exec<S, E>(pipe: DuplexStream, tx: mpsc::Sender<proto::ExecSandboxInput>, events: S)
where
    S: Stream<Item = Result<proto::ExecSandboxEvent, E>> + Send,
    E: std::fmt::Display,
{
    use proto::exec_sandbox_event::Payload as Out;
    use proto::exec_sandbox_input::Payload as In;
    let (mut r, mut w) = tokio::io::split(pipe);
    let input = async move {
        let mut tx = Some(tx);
        loop {
            let payload = match zapper_driver::read_frame(&mut r).await {
                Ok(Some(Frame::Data(d))) => In::Stdin(d),
                Ok(Some(Frame::Resize(rows, cols))) => In::Resize(proto::ExecSandboxWindowResize {
                    cols: cols.into(),
                    rows: rows.into(),
                }),
                Ok(Some(Frame::Eof)) => {
                    tx = None;
                    continue;
                }
                Ok(Some(Frame::Hangup)) | Ok(None) | Err(_) => return,
                Ok(Some(_)) => continue,
            };
            if let Some(t) = &tx {
                let msg = proto::ExecSandboxInput {
                    payload: Some(payload),
                };
                if t.send(msg).await.is_err() {
                    tx = None;
                }
            }
        }
    };
    let output = async move {
        let mut events = std::pin::pin!(events);
        while let Some(event) = events.next().await {
            let written = match event {
                Ok(event) => match event.payload {
                    Some(Out::Stdout(c)) => write_frames(&mut w, &c.data, Frame::Data).await,
                    Some(Out::Stderr(c)) => write_frames(&mut w, &c.data, Frame::Stderr).await,
                    Some(Out::Exit(x)) => {
                        let _ = w.write_all(&Frame::Exit(x.exit_code).encode()).await;
                        return;
                    }
                    None => Ok(()),
                },
                Err(e) => {
                    let why = format!("openshell: {e}\n").into_bytes();
                    let _ = w.write_all(&Frame::Stderr(why).encode()).await;
                    return;
                }
            };
            if written.is_err() {
                return;
            }
        }
    };
    // Whichever ends first ends the call: dropping the response stream
    // cancels it. The office reads what is buffered, then the end.
    tokio::select! {
        _ = input => {}
        _ = output => {}
    }
}

/// Carries one TCP forward between the office's end of `pipe` (raw bytes)
/// and the gateway.
async fn pump_tcp<S, E>(pipe: DuplexStream, tx: mpsc::Sender<proto::TcpForwardFrame>, frames: S)
where
    S: Stream<Item = Result<proto::TcpForwardFrame, E>> + Send,
{
    use proto::tcp_forward_frame::Payload;
    let (mut r, mut w) = tokio::io::split(pipe);
    let input = async move {
        let mut buf = vec![0u8; CHUNK];
        loop {
            let n = match r.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            let frame = proto::TcpForwardFrame {
                payload: Some(Payload::Data(buf[..n].to_vec())),
            };
            if tx.send(frame).await.is_err() {
                return;
            }
        }
    };
    let output = async move {
        let mut frames = std::pin::pin!(frames);
        while let Some(Ok(frame)) = frames.next().await {
            if let Some(Payload::Data(d)) = frame.payload {
                if w.write_all(&d).await.is_err() {
                    return;
                }
            }
        }
    };
    tokio::select! {
        _ = input => {}
        _ = output => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proto::exec_sandbox_event::Payload as Out;
    use proto::exec_sandbox_input::Payload as In;
    use tokio::io::AsyncRead;

    async fn read_all(mut r: impl AsyncRead + Unpin) -> Vec<u8> {
        let mut v = Vec::new();
        let _ = r.read_to_end(&mut v).await;
        v
    }

    fn event(p: Out) -> Result<proto::ExecSandboxEvent, String> {
        Ok(proto::ExecSandboxEvent { payload: Some(p) })
    }

    // The office's end of an interactive exec: what it writes reaches the
    // command's stdin, and what the command prints comes back as frames —
    // the path the foreman's `claude -p` and the drive's tar copies take.
    #[tokio::test]
    async fn an_exec_speaks_frames_to_the_office() {
        let (ours, theirs) = tokio::io::duplex(PIPE);
        let (tx, mut rx) = mpsc::channel(16);
        let (etx, erx) = mpsc::channel(16);
        let pump = tokio::spawn(pump_exec(theirs, tx, receiver_stream(erx)));

        let big = vec![b'x'; CHUNK + 10];
        let office = tokio::spawn(async move {
            let mut out = Vec::new();
            let input = Some(std::io::Cursor::new(b"hello".to_vec()));
            let r = crate::drive::pump_command(ours, input, &mut out, 1 << 20).await;
            (r.unwrap(), out)
        });
        // The stdin, then its end.
        match rx.recv().await.and_then(|m| m.payload) {
            Some(In::Stdin(d)) => assert_eq!(d, b"hello"),
            other => panic!("{other:?}"),
        }
        assert!(rx.recv().await.is_none(), "Eof closes the command's stdin");
        etx.send(event(Out::Stdout(proto::ExecSandboxStdout {
            data: big.clone(),
        })))
        .await
        .unwrap();
        etx.send(event(Out::Stderr(proto::ExecSandboxStderr {
            data: b"warn".to_vec(),
        })))
        .await
        .unwrap();
        etx.send(event(Out::Exit(proto::ExecSandboxExit { exit_code: 3 })))
            .await
            .unwrap();
        let ((code, err), out) = office.await.unwrap();
        assert_eq!(code, 3);
        assert_eq!(err, "warn");
        assert_eq!(out, big, "stdout past a frame's size arrives whole");
        pump.await.unwrap();
    }

    // A call that fails says why on stderr, and the stream ends without an
    // exit status (the foreman sees the agent gone).
    #[tokio::test]
    async fn a_failed_call_ends_the_stream() {
        let (ours, theirs) = tokio::io::duplex(PIPE);
        let (tx, _rx) = mpsc::channel(16);
        let events = futures_util::stream::iter(vec![Err::<proto::ExecSandboxEvent, _>(
            "unavailable".to_string(),
        )]);
        tokio::spawn(pump_exec(theirs, tx, events));
        let bytes = read_all(ours).await;
        let mut r = &bytes[..];
        match zapper_driver::read_frame(&mut r).await.unwrap() {
            Some(Frame::Stderr(d)) => assert!(String::from_utf8_lossy(&d).contains("unavailable")),
            other => panic!("{other:?}"),
        }
        assert!(zapper_driver::read_frame(&mut r).await.unwrap().is_none());
    }

    // The office letting go of the stream (the foreman's `Proc` dropped)
    // ends the call.
    #[tokio::test]
    async fn letting_go_ends_the_call() {
        let (ours, theirs) = tokio::io::duplex(PIPE);
        let (tx, _rx) = mpsc::channel(16);
        let (_etx, erx) = mpsc::channel::<Result<proto::ExecSandboxEvent, String>>(16);
        let pump = tokio::spawn(pump_exec(theirs, tx, receiver_stream(erx)));
        drop(ours);
        tokio::time::timeout(Duration::from_secs(5), pump)
            .await
            .expect("the pump ends")
            .unwrap();
    }

    #[test]
    fn templates_and_labels_are_names_the_gateway_takes() {
        assert_eq!(template_name("fake-corp-dev"), "zapper-fake-corp-dev");
        assert_eq!(template_name("Office_Fake"), "zapper-office-fake");
        assert!(template_name(&"p".repeat(100)).len() <= 63);
        assert_eq!(label_value("acme/eng"), "acme-eng");
        assert_eq!(label_value("-x-"), "x");
        let l = gateway_labels(&BTreeMap::from([
            ("firm".to_string(), "acme".to_string()),
            ("firm-role".to_string(), "acme/eng".to_string()),
        ]));
        assert_eq!(l["zapper-firm"], "acme");
        assert!(!l.contains_key("zapper-firm-role"));
    }

    #[test]
    fn claude_reaches_the_office_tools_over_http_and_pi_through_the_bridge() {
        let config = Config::from_yaml(
            "profiles:\n  c: { agent: { kind: claude-code, command: [claude] } }\n  p: { agent: { kind: pi, command: [pi] } }\n",
        )
        .unwrap();
        let bridge =
            json!({ "office": { "command": crate::ops::AGENT_PATH, "args": ["office", "mcp"] } });
        let url = "http://host.openshell.internal:8970/mcp";

        let mut claude = config.resolve("c").unwrap();
        claude.agent.mcp_servers = Some(bridge.clone());
        office_over_http(&mut claude, url);
        assert_eq!(
            claude.agent.mcp_servers,
            Some(json!({ "office": { "type": "http", "url": url } }))
        );

        let mut pi = config.resolve("p").unwrap();
        pi.agent.mcp_servers = Some(bridge.clone());
        office_over_http(&mut pi, url);
        assert_eq!(pi.agent.mcp_servers, Some(bridge));
    }

    #[test]
    fn the_edge_defaults_to_host_loopback_and_the_drivers_identity() {
        let s = Settings::from_yaml("gateway: https://gw\n").unwrap();
        assert_eq!(s.edge.listen.to_string(), "127.0.0.1:8970");
        assert_eq!(s.edge.url, "http://host.openshell.internal:8970");
        assert_eq!(s.edge.jwks, "http://127.0.0.1:8961/.well-known/jwks.json");
        let s = Settings::from_yaml(
            "gateway: https://gw\nedge: { listen: 127.0.0.1:9000, url: http://host.openshell.internal:9000 }\n",
        )
        .unwrap();
        assert_eq!(s.edge.listen.port(), 9000);
        assert_eq!(s.edge.trust_domain, "openshell.local");
        assert!(Settings::from_yaml("gateway: https://gw\nedge: { nope: 1 }\n").is_err());
    }

    #[test]
    fn settings_name_a_gateway_and_default_the_images() {
        assert!(Settings::from_yaml("providers: [anthropic]\n").is_err());
        assert!(Settings::from_yaml("gateway: https://gw\nnope: 1\n").is_err());
        assert!(Settings::from_yaml("gateway: https://gw\nclient_cert: c\n").is_err());
        let s = Settings::from_yaml(
            "gateway: https://gw\nimages: { office: ghcr.io/acme/office:1 }\nproviders: [anthropic]\n",
        )
        .unwrap();
        assert_eq!(s.image("office"), "ghcr.io/acme/office:1");
        assert_eq!(
            s.image("office-fake"),
            "localhost/zapper-openshell-office-fake"
        );
        assert_eq!(s.ready_timeout(), READY_TIMEOUT);
    }

    #[test]
    fn phases_become_states() {
        assert_eq!(status_of(SandboxPhase::Ready), Status::Running);
        assert_eq!(status_of(SandboxPhase::Provisioning), Status::Starting);
        assert!(matches!(status_of(SandboxPhase::Error), Status::Failed(_)));
        assert!(!status_of(SandboxPhase::Stopped).is_live());
    }
}
