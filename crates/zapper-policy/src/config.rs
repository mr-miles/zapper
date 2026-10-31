use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use zapper_proto::FsRule;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The whole daemon configuration. This is what `GET /config` returns and what
/// `POST /config/save` writes to the startup file.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub daemon: DaemonConfig,
    /// Named secrets. Their values live on the host only; the sandbox sees a
    /// placeholder that the proxy swaps for the value on allowed requests.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub secrets: BTreeMap<String, Secret>,
    /// The global policy layer, below every profile.
    #[serde(default)]
    pub policy: Policy,
    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,
    /// Org charts of workers run by the office (`zapper-office`), as
    /// written: the office parses and checks them, with the rest of each
    /// change.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub firms: BTreeMap<String, serde_yaml::Value>,
    /// Image recipes, by the image's name: what `zapper image build NAME`
    /// builds. (`daemon.images` is where the built images go.)
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub images: BTreeMap<String, ImageRecipe>,
}

/// How an image is built: an OCI base, the files it takes as named
/// arguments, the commands run in it, and where its arguments are copied.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageRecipe {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// The OCI image it starts from, e.g. `docker.io/library/ubuntu:26.04`.
    pub from: String,
    /// Files or directories from the host it takes, by name: `--arg
    /// NAME=PATH` gives one, else its `default`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub args: BTreeMap<String, ImageArg>,
    /// Shell commands run in the image at build time, in order, before the
    /// arguments are copied in (so changing one doesn't run these again).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub run: Vec<String>,
    /// Arguments copied into the image, in order, after `run`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub copy: Vec<ImageCopy>,
    /// Shell commands run once the arguments are copied in.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub after: Vec<String>,
}

/// One argument copied into the image.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageCopy {
    /// The argument's name.
    pub from: String,
    /// Where it goes: an absolute path, its mode kept; a directory's
    /// contents go into the directory named.
    pub to: String,
}

/// A file (or a directory, with all it holds) an image recipe takes from
/// the host.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageArg {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// The file or directory used when no `--arg` gives one; relative to
    /// the config's directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DaemonConfig {
    /// Firecracker binary.
    pub firecracker: String,
    /// Static `zapper-agent` binary; empty looks next to `zapper`.
    pub agent: String,
    /// Directory of root filesystem images (`<name>.ext4`).
    pub images: String,
    /// Directory of guest kernels.
    pub kernels: String,
    /// Directory of per-session state: sockets, disks, audit logs.
    pub state: String,
    /// Address of the web UI (answering approvals, and the board's firm
    /// actions); empty disables it.
    pub ui: String,
    /// How long an `ask` decision waits for an approval before denying.
    pub ask_timeout_secs: u64,
    /// Upper bound on concurrent sessions.
    pub max_sessions: usize,
    /// Refuse a session when the memory given to the VMs starting or
    /// running, with its own, would exceed the host's memory times this.
    /// Unset: never refused, only warned of past 1.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_overcommit: Option<f64>,
    /// Private networks (CIDR) sessions may reach at all, if a rule allows.
    /// Loopback, link-local (cloud metadata) and the host's own addresses are
    /// never reachable; other private and unique-local addresses only if
    /// listed here.
    pub private_networks: Vec<String>,
    /// The record (firms' journals and transcripts, sessions' audit logs
    /// and terminal recordings) kept by `zapper-keeper`, a separate user:
    /// the daemon can append to it and never change it. Unset: the record
    /// is written under `state`, by the daemon's own user.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keeper: Option<KeeperConfig>,
    /// The driver: agents run headless and given tasks (`agent.driven`),
    /// and The Office's workers. Off, zapper only sandboxes and monitors:
    /// it starts no agent on its own and gives none any input.
    pub driver: bool,
    /// Each session's Firecracker runs as a uid of its own, from the daemon
    /// user's ranges in /etc/subuid and /etc/subgid, mapped with newuidmap
    /// and newgidmap (package uidmap), where the host has them; else, and
    /// when off, as the daemon user.
    #[serde(skip_serializing_if = "is_true")]
    pub vmm_subids: bool,
}

/// Where the keeper listens and where its files are.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeeperConfig {
    /// Its socket, e.g. `/run/zapper-keeper/keeper.sock`.
    pub socket: String,
    /// The directory it keeps, readable by the daemon's user, e.g.
    /// `/var/lib/zapper-keeper/record/1000`.
    pub dir: String,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            firecracker: "~/.local/share/zapper/bin/firecracker".into(),
            agent: String::new(),
            images: "~/.local/share/zapper/images".into(),
            kernels: "~/.local/share/zapper/kernels".into(),
            state: "~/.local/state/zapper".into(),
            ui: "127.0.0.1:7878".into(),
            ask_timeout_secs: 60,
            max_sessions: 16,
            memory_overcommit: None,
            private_networks: Vec::new(),
            keeper: None,
            driver: true,
            vmm_subids: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Secret {
    #[serde(flatten)]
    pub source: SecretSource,
    /// How the secret reaches its user.
    #[serde(default, skip_serializing_if = "SecretMode::is_proxy")]
    pub mode: SecretMode,
    /// Host globs the proxy may send the secret to. A rule can only inject a
    /// secret into requests to these hosts, whatever else it matches. Required
    /// for `proxy` secrets.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hosts: Vec<String>,
    /// Also send the secret over a non-TLS connection. Off by default:
    /// anything on the path can read it. For lab-internal services only. For a
    /// `proxy` secret this governs plain `http://`; for a `dbauth` secret it
    /// governs the proxy→server leg (the guest→proxy leg carries no secret).
    #[serde(default, skip_serializing_if = "is_false")]
    pub plaintext: bool,
    /// For a `dbauth` secret: the database user the proxy authenticates as. The
    /// secret defines the user; the guest's requested user is ignored. Required
    /// for `dbauth`, unused by other modes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// For a `dbauth` secret: the database to pin the guest to. `None` honours
    /// the database the guest named in its startup. Unused by other modes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    /// For a `dbauth` secret used by Kafka: the SASL mechanism —
    /// `SCRAM-SHA-256` (the default), `SCRAM-SHA-512`, `PLAIN`, or
    /// `OAUTHBEARER` (the secret's value is the bearer token). Ignored by the
    /// other protocols.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mechanism: Option<String>,
}

/// The SASL mechanisms the Kafka relay can speak.
pub const KAFKA_MECHANISMS: [&str; 4] = ["SCRAM-SHA-256", "SCRAM-SHA-512", "PLAIN", "OAUTHBEARER"];

impl Secret {
    pub fn allows_host(&self, host: &str) -> bool {
        self.hosts
            .iter()
            .any(|h| crate::eval::host_matches(h, host))
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SecretMode {
    /// The guest sees a placeholder; the proxy puts the value into the headers
    /// of allowed requests to `hosts`. For bearer tokens and API keys.
    #[default]
    Proxy,
    /// `{{secret:NAME}}` in the agent's env and files becomes the value
    /// itself, readable by the workload. For credentials the client must hold:
    /// signing keys (S3), client certificates, kubeconfigs. Scope them and
    /// make them expire.
    Guest,
    /// The guest connects to a database with no password; the proxy terminates
    /// the client-side auth (presenting the guest as authenticated) and itself
    /// authenticates to the real server with this host-held credential, then
    /// relays the session. Bound to `hosts` + `user` (+ optional `database`);
    /// reached by a database `protocol:` net rule. Proposal 07.
    DbAuth,
    /// AWS credentials the proxy signs with (SigV4). The value is
    /// credential_process JSON: `{"AccessKeyId", "SecretAccessKey",
    /// "SessionToken"?}`. `{{secret:NAME}}` in the guest's env becomes a fake
    /// access key id (`aws_key_id`), which the guest's SDK signs with; the
    /// proxy re-signs allowed requests to `hosts` with the real keys.
    Aws,
}

impl SecretMode {
    fn is_proxy(&self) -> bool {
        *self == SecretMode::Proxy
    }
}

/// The wire protocol a net rule governs. `tcp` (the default) is today's
/// behaviour — HTTP inspection or a plain tunnel. Every other value routes an
/// allowed connection to that database's auth-relay, which injects the rule's
/// `dbauth` secret instead of taking the HTTP path.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NetProtocol {
    #[default]
    Tcp,
    Postgres,
    Mysql,
    Mongodb,
    Redis,
    Cassandra,
    Tds,
    Kafka,
}

impl NetProtocol {
    fn is_tcp(&self) -> bool {
        *self == NetProtocol::Tcp
    }

    /// The protocol's name as a static string (for request-log schemes etc.).
    pub fn scheme(&self) -> &'static str {
        match self {
            NetProtocol::Tcp => "tcp",
            NetProtocol::Postgres => "postgres",
            NetProtocol::Mysql => "mysql",
            NetProtocol::Mongodb => "mongodb",
            NetProtocol::Redis => "redis",
            NetProtocol::Cassandra => "cassandra",
            NetProtocol::Tds => "tds",
            NetProtocol::Kafka => "kafka",
        }
    }
}

impl fmt::Display for NetProtocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.scheme())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum SecretSource {
    /// Read from the daemon's environment.
    Env { env: String },
    /// Read from a file, optionally a string field of a JSON file selected by a
    /// JSON pointer (e.g. `/claudeAiOauth/accessToken`).
    File {
        file: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        json: Option<String>,
    },
    /// Inline value. Saved into the startup file as is; prefer `env` or `file`.
    Value { value: String },
    /// Minted by a host command that prints the credential, as
    /// `{"value": "...", "expires_at": "<RFC 3339>"}` or a bare value kept
    /// for `ttl_secs`. Refreshed before it expires; see [`crate::mint`].
    /// `zapper` as the first word is this binary.
    Command {
        command: Vec<String>,
        /// Kill the command after this long (default 30).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_secs: Option<u64>,
        /// How long a bare value lasts (default 300).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ttl_secs: Option<u64>,
    },
}

impl SecretSource {
    pub fn resolve(&self) -> Result<String> {
        match self {
            SecretSource::Env { env } => {
                std::env::var(env).with_context(|| format!("environment variable {env} is not set"))
            }
            SecretSource::File { file, json } => {
                let path = expand_home(file);
                let text =
                    std::fs::read_to_string(&path).with_context(|| format!("reading {path}"))?;
                match json {
                    None => Ok(text.trim().to_string()),
                    Some(pointer) => {
                        let v: Value = serde_json::from_str(&text)
                            .with_context(|| format!("parsing {path}"))?;
                        v.pointer(pointer)
                            .and_then(Value::as_str)
                            .map(str::to_string)
                            .ok_or_else(|| anyhow!("{path}: no string at {pointer}"))
                    }
                }
            }
            SecretSource::Value { value } => Ok(value.clone()),
            SecretSource::Command { .. } => {
                bail!("a command secret is minted by the daemon, not read")
            }
        }
    }

    /// For a `command` secret: what to run, and for how long.
    pub fn mint_spec(&self) -> Option<crate::mint::Spec> {
        match self {
            SecretSource::Command {
                command,
                timeout_secs,
                ttl_secs,
            } => Some(crate::mint::Spec::new(command, *timeout_secs, *ttl_secs)),
            _ => None,
        }
    }
}

/// Parses `4GiB`, `512MiB`, `1G`, or plain bytes.
pub fn parse_size(s: &str) -> Result<u64> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: u64 = num.parse().with_context(|| format!("size {s:?}"))?;
    let mult = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1 << 10,
        "m" | "mb" | "mib" => 1 << 20,
        "g" | "gb" | "gib" => 1 << 30,
        "t" | "tb" | "tib" => 1 << 40,
        _ => bail!("size {s:?}: unknown unit"),
    };
    Ok(n * mult)
}

/// Where relative paths in the config are resolved: the config file's
/// directory, set once when the config is found.
static CONFIG_DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

pub fn set_config_dir(dir: &std::path::Path) {
    let _ = CONFIG_DIR.set(dir.to_path_buf());
}

/// Expands a leading `~/` to `$HOME`, and resolves a relative path against
/// the config file's directory, so a config never needs an absolute path.
pub fn expand_home(path: &str) -> String {
    match (path.strip_prefix("~/"), std::env::var("HOME")) {
        (Some(rest), Ok(home)) => format!("{home}/{rest}"),
        _ if !path.is_empty() && !path.starts_with('/') && !path.starts_with('~') => {
            match CONFIG_DIR.get() {
                Some(dir) => dir.join(path).to_string_lossy().into_owned(),
                None => path.to_string(),
            }
        }
        _ => path.to_string(),
    }
}

impl Config {
    /// Whether the config holds secret values itself (`value:`), so the file
    /// must not be readable by others.
    pub fn has_inline_secrets(&self) -> bool {
        self.secrets
            .values()
            .any(|s| matches!(s.source, SecretSource::Value { .. }))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Allow,
    Deny,
    Ask,
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Action::Allow => "allow",
            Action::Deny => "deny",
            Action::Ask => "ask",
        })
    }
}

/// One policy layer.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Policy {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub net: Vec<NetRule>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub exec: Vec<ExecRule>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub fs: Vec<FsRule>,
    /// Action when no rule matches. The most specific layer that sets it wins;
    /// `deny` if none does.
    #[serde(skip_serializing_if = "Defaults::is_empty")]
    pub defaults: Defaults,
}

impl Policy {
    pub fn is_empty(&self) -> bool {
        self.net.is_empty()
            && self.exec.is_empty()
            && self.fs.is_empty()
            && self.defaults.is_empty()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Defaults {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub net: Option<Action>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exec: Option<Action>,
}

impl Defaults {
    pub fn is_empty(&self) -> bool {
        self.net.is_none() && self.exec.is_none()
    }
}

/// Egress rule, matched against every connection and, when the connection is
/// inspected, every HTTP request on it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetRule {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub action: Action,
    /// Host glob, case-insensitive: `api.github.com`, `*.github.com`, `*`.
    #[serde(default = "any")]
    pub host: String,
    /// Destination ports; empty matches any.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<u16>,
    /// HTTP methods; empty matches any. Setting it makes the proxy inspect TLS.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub methods: Vec<String>,
    /// URL path glob (no query string). Setting it makes the proxy inspect TLS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// The wire protocol. A database's (`postgres`, `redis`, `mysql`, …)
    /// routes an allowed connection to the database auth-relay, which
    /// injects the named `dbauth` secret, instead of the HTTP path. Default
    /// `tcp`.
    #[serde(default, skip_serializing_if = "NetProtocol::is_tcp")]
    pub protocol: NetProtocol,
    /// Secrets whose placeholders are replaced with their values in the
    /// headers of requests this rule allows.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<String>,
    /// Headers set on requests this rule allows; values may use
    /// `{{secret:NAME}}`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub set_headers: BTreeMap<String, String>,
    #[serde(default, rename = "final", skip_serializing_if = "is_false")]
    pub final_: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

impl NetRule {
    /// Whether matching or applying this rule needs the decrypted HTTP request.
    /// A database rule is never HTTP-inspected — it goes to the auth-relay
    /// ([`db_relay`](Self::db_relay)) instead, even though it names a secret.
    pub fn needs_inspection(&self) -> bool {
        self.protocol == NetProtocol::Tcp
            && (!self.methods.is_empty()
                || self.path.is_some()
                || !self.secrets.is_empty()
                || !self.set_headers.is_empty())
    }

    /// The database protocol whose auth-relay this rule routes to, if any.
    /// `None` for an ordinary `tcp` rule (HTTP inspection or a plain tunnel).
    pub fn db_relay(&self) -> Option<NetProtocol> {
        match self.protocol {
            NetProtocol::Tcp => None,
            p => Some(p),
        }
    }
}

/// Exec rule, matched against every `execve` inside the sandbox.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecRule {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub action: Action,
    /// Glob over the resolved executable path if it contains `/`
    /// (`/usr/bin/*`), otherwise over its file name (`git`).
    #[serde(default = "any")]
    pub path: String,
    /// Glob over the arguments after argv[0], joined by spaces.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<String>,
    #[serde(default, rename = "final", skip_serializing_if = "is_false")]
    pub final_: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

fn any() -> String {
    "*".into()
}

fn is_true(b: &bool) -> bool {
    *b
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Profile {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extends: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Merged field by field over the parent's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub engine: Option<Value>,
    /// Merged field by field over the parent's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<Value>,
    /// Added to the parent's.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub mounts: Vec<Mount>,
    /// Shell commands run as root when the sandbox is created, after the
    /// parent's. Network goes through the proxy with this profile's policy.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub setup: Vec<String>,
    #[serde(skip_serializing_if = "Policy::is_empty")]
    pub policy: Policy,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mount {
    /// Host directory; `{workdir}` is the directory `zapper run` was started
    /// in. It is copied into an ext4 disk when the session starts: changes in
    /// the guest stay in the session until copied out with `zapper cp`.
    pub host: String,
    pub guest: String,
    #[serde(default)]
    pub mode: MountMode,
    /// Free space added to a `rw` disk, e.g. `1GiB`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MountMode {
    Ro,
    #[default]
    Rw,
}

/// The microVM a session runs in.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Engine {
    /// Root filesystem image name, built with `zapper image build`. The
    /// session gets a private copy; the image itself is never written.
    pub image: String,
    /// Guest kernel name in the images directory.
    pub kernel: String,
    pub vcpus: u8,
    pub memory_mib: u32,
    /// Size of the session's root disk, e.g. `4GiB`; the image is grown to it.
    pub disk: String,
    /// Session lifetime; the VM is destroyed when it runs out. `0` is no limit.
    pub ttl_secs: u64,
    /// Extra kernel command line.
    pub boot_args: String,
    /// How the guest reaches the network.
    pub network: NetworkMode,
    /// Guest TCP ports the host may connect to over vsock, for services
    /// other sessions reach (`zapper service add --to-session ID:PORT`):
    /// each goes to the guest's NIC address, or loopback without a NIC.
    pub ports: Vec<u16>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NetworkMode {
    /// A virtio NIC. The VMM runs in its own network namespace whose only way
    /// out is zapper: every TCP connection and DNS query from the guest is
    /// captured there and goes through policy, whatever the program.
    #[default]
    Nic,
    /// Loopback and `HTTP(S)_PROXY` pointing at a vsock forwarder, for programs
    /// that use proxy variables. The VMM's network namespace stays empty.
    Vsock,
}

impl Default for Engine {
    fn default() -> Self {
        Self {
            image: "ubuntu-26.04".into(),
            kernel: "vmlinux-6.1".into(),
            vcpus: 2,
            memory_mib: 2048,
            disk: "4GiB".into(),
            ttl_secs: 0,
            boot_args: String::new(),
            network: NetworkMode::Nic,
            ports: vec![],
        }
    }
}

/// Agent harnesses zapper knows (`agent.kind`); each is implemented in the
/// daemon's `harness` module.
pub const AGENT_KINDS: &[&str] = &["claude-code", "pi", "generic"];

/// The harnesses the driver can run headless (`agent.driven`).
pub const DRIVABLE_KINDS: &[&str] = &["claude-code", "pi"];

/// What runs inside the sandbox and how it is configured.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentSpec {
    /// The agent harness (`AGENT_KINDS`): `claude-code` sets Claude Code up
    /// and `pi` sets pi up, and both can run office workers; `generic` only
    /// writes `files` and `env`.
    pub kind: String,
    /// Default command of `zapper run`.
    pub command: Vec<String>,
    /// Run the agent headless, given tasks through the API, `zapper tell`
    /// or the UI instead of a terminal (`DRIVABLE_KINDS` only; needs
    /// `daemon.driver`).
    pub driven: bool,
    /// User the command runs as, created with uid 1000 if missing.
    pub user: String,
    /// Run as root instead. The workload can then tamper with in-guest
    /// controls (exec policy, `fs` rules, instrumentation); host-side
    /// controls (network, secrets, disks, limits) still hold.
    pub root: bool,
    /// Working directory of the command.
    pub workdir: String,
    /// Environment; values may use `{{secret:NAME}}`.
    pub env: BTreeMap<String, String>,
    // The agent's configuration, in terms every harness understands; each
    // writes it in its own place and format (daemon::harness).
    /// The model the agent uses.
    pub model: Option<String>,
    /// How the agent approves its own actions (Claude Code:
    /// `permissions.defaultMode` in its managed settings).
    pub permission_mode: Option<String>,
    /// Standing instructions: the harness's instructions file (Claude
    /// Code: `~/.claude/CLAUDE.md`).
    #[serde(alias = "claude_md")]
    pub instructions: Option<String>,
    /// A host directory of skills (`<name>/SKILL.md`), mounted read-only
    /// where the harness reads skills.
    pub skills: Option<String>,
    /// MCP servers: `{name: {command, args, env}}`.
    pub mcp_servers: Option<Value>,
    /// A command the harness runs when the agent is about to end a turn: if
    /// it prints a reason, the agent goes on with it (Claude Code: a Stop
    /// hook).
    pub turn_end: Option<String>,
    /// Harness-native settings merged into its main config file (Claude
    /// Code: `~/.claude/settings.json`).
    pub settings: Option<Value>,
    /// Files in the harness's config directory (Claude Code: `~/.claude`),
    /// by path relative to it.
    pub config_files: BTreeMap<String, String>,
    /// Host directories mounted in the harness's config directory, at
    /// `guest` relative to it.
    pub config_mounts: Vec<Mount>,
    /// Extra files, guest path (`~` is the user's home) to content.
    pub files: BTreeMap<String, String>,
}

impl Default for AgentSpec {
    fn default() -> Self {
        Self {
            kind: "claude-code".into(),
            command: vec!["claude".into()],
            driven: false,
            user: "agent".into(),
            root: false,
            workdir: "/work".into(),
            env: BTreeMap::new(),
            model: None,
            permission_mode: None,
            instructions: None,
            skills: None,
            mcp_servers: None,
            turn_end: None,
            settings: None,
            config_files: BTreeMap::new(),
            config_mounts: Vec::new(),
            files: BTreeMap::new(),
        }
    }
}

/// A profile with its `extends` chain resolved.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResolvedProfile {
    pub name: String,
    pub engine: Engine,
    pub agent: AgentSpec,
    pub mounts: Vec<Mount>,
    pub setup: Vec<String>,
    /// Policy layers from the profile itself up to its root ancestor, global
    /// layer excluded.
    pub layers: Vec<(Scope, Policy)>,
}

/// Where a policy layer comes from.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub enum Scope {
    Global,
    Profile(String),
    /// Standing approvals for one role of a running firm (`firm/role`).
    Role(String),
    Session(String),
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Scope::Global => f.write_str("global"),
            Scope::Profile(p) => write!(f, "profile:{p}"),
            Scope::Role(r) => write!(f, "role:{r}"),
            Scope::Session(s) => write!(f, "session:{s}"),
        }
    }
}

impl std::str::FromStr for Scope {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s.split_once(':') {
            None if s == "global" => Ok(Scope::Global),
            Some(("profile", p)) if !p.is_empty() => Ok(Scope::Profile(p.into())),
            Some(("session", id)) if !id.is_empty() => Ok(Scope::Session(id.into())),
            Some(("role", r)) if !r.is_empty() => Ok(Scope::Role(r.into())),
            _ => bail!(
                "invalid scope {s:?}: expected global, profile:NAME, role:FIRM/ROLE or session:ID"
            ),
        }
    }
}

impl From<Scope> for String {
    fn from(s: Scope) -> String {
        s.to_string()
    }
}

impl TryFrom<String> for Scope {
    type Error = anyhow::Error;
    fn try_from(s: String) -> Result<Self> {
        s.parse()
    }
}

/// Rule categories.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Net,
    Exec,
    Fs,
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Kind::Net => "net",
            Kind::Exec => "exec",
            Kind::Fs => "fs",
        })
    }
}

/// A rule of any kind, as accepted by the rules API.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum AnyRule {
    Net(NetRule),
    Exec(ExecRule),
    Fs(FsRule),
}

impl AnyRule {
    pub fn kind(&self) -> Kind {
        match self {
            AnyRule::Net(_) => Kind::Net,
            AnyRule::Exec(_) => Kind::Exec,
            AnyRule::Fs(_) => Kind::Fs,
        }
    }

    pub fn set_id(&mut self, id: String) {
        match self {
            AnyRule::Net(r) => r.id = Some(id),
            AnyRule::Exec(r) => r.id = Some(id),
            AnyRule::Fs(r) => r.id = Some(id),
        }
    }

    pub fn id(&self) -> Option<&str> {
        match self {
            AnyRule::Net(r) => r.id.as_deref(),
            AnyRule::Exec(r) => r.id.as_deref(),
            AnyRule::Fs(r) => r.id.as_deref(),
        }
    }
}

impl Policy {
    fn ids(&self) -> impl Iterator<Item = &str> {
        let net = self.net.iter().filter_map(|r| r.id.as_deref());
        let exec = self.exec.iter().filter_map(|r| r.id.as_deref());
        let fs = self.fs.iter().filter_map(|r| r.id.as_deref());
        net.chain(exec).chain(fs)
    }

    /// Gives every rule without an id one that is not in `taken`.
    pub fn assign_ids(&mut self, taken: &mut BTreeSet<String>) {
        fn fresh(kind: Kind, taken: &mut BTreeSet<String>) -> String {
            let id = (1..)
                .map(|n| format!("{kind}-{n}"))
                .find(|id| !taken.contains(id))
                .unwrap();
            taken.insert(id.clone());
            id
        }
        for r in &mut self.net {
            r.id.get_or_insert_with(|| fresh(Kind::Net, taken));
        }
        for r in &mut self.exec {
            r.id.get_or_insert_with(|| fresh(Kind::Exec, taken));
        }
        for r in &mut self.fs {
            r.id.get_or_insert_with(|| fresh(Kind::Fs, taken));
        }
    }

    /// Inserts a rule at `position` within its kind's list (end if `None`).
    pub fn insert(&mut self, rule: AnyRule, position: Option<usize>) {
        fn put<T>(list: &mut Vec<T>, item: T, position: Option<usize>) {
            let at = position.unwrap_or(list.len()).min(list.len());
            list.insert(at, item);
        }
        match rule {
            AnyRule::Net(r) => put(&mut self.net, r, position),
            AnyRule::Exec(r) => put(&mut self.exec, r, position),
            AnyRule::Fs(r) => put(&mut self.fs, r, position),
        }
    }

    /// Removes the rule with this id, returning it.
    pub fn remove(&mut self, id: &str) -> Option<AnyRule> {
        if let Some(i) = self.net.iter().position(|r| r.id.as_deref() == Some(id)) {
            return Some(AnyRule::Net(self.net.remove(i)));
        }
        if let Some(i) = self.exec.iter().position(|r| r.id.as_deref() == Some(id)) {
            return Some(AnyRule::Exec(self.exec.remove(i)));
        }
        if let Some(i) = self.fs.iter().position(|r| r.id.as_deref() == Some(id)) {
            return Some(AnyRule::Fs(self.fs.remove(i)));
        }
        None
    }

    /// All rules in evaluation order, tagged with their kind.
    pub fn rules(&self) -> Vec<AnyRule> {
        let net = self.net.iter().cloned().map(AnyRule::Net);
        let exec = self.exec.iter().cloned().map(AnyRule::Exec);
        let fs = self.fs.iter().cloned().map(AnyRule::Fs);
        net.chain(exec).chain(fs).collect()
    }
}

impl Config {
    pub fn from_yaml(text: &str) -> Result<Self> {
        let mut config: Config = serde_yaml::from_str(text)?;
        config.normalize();
        config.validate()?;
        Ok(config)
    }

    pub fn to_yaml(&self) -> String {
        serde_yaml::to_string(self).expect("config serialises")
    }

    /// Ids of every rule in the config.
    pub fn rule_ids(&self) -> BTreeSet<String> {
        let profiles = self.profiles.values().flat_map(|p| p.policy.ids());
        self.policy
            .ids()
            .chain(profiles)
            .map(str::to_string)
            .collect()
    }

    /// Gives every rule an id.
    pub fn normalize(&mut self) {
        let mut taken = self.rule_ids();
        self.policy.assign_ids(&mut taken);
        for p in self.profiles.values_mut() {
            p.policy.assign_ids(&mut taken);
        }
    }

    pub fn policy(&self, scope: &Scope) -> Option<&Policy> {
        match scope {
            Scope::Global => Some(&self.policy),
            Scope::Profile(name) => self.profiles.get(name).map(|p| &p.policy),
            Scope::Role(_) | Scope::Session(_) => None,
        }
    }

    pub fn policy_mut(&mut self, scope: &Scope) -> Option<&mut Policy> {
        match scope {
            Scope::Global => Some(&mut self.policy),
            Scope::Profile(name) => self.profiles.get_mut(name).map(|p| &mut p.policy),
            Scope::Role(_) | Scope::Session(_) => None,
        }
    }

    /// Finds the scope holding the rule with this id.
    pub fn find_rule(&self, id: &str) -> Option<Scope> {
        if self.policy.ids().any(|i| i == id) {
            return Some(Scope::Global);
        }
        self.profiles
            .iter()
            .find(|(_, p)| p.policy.ids().any(|i| i == id))
            .map(|(n, _)| Scope::Profile(n.clone()))
    }

    pub fn validate(&self) -> Result<()> {
        let mut errors = Vec::new();
        let mut seen = BTreeSet::new();
        let mut layers = vec![(Scope::Global, &self.policy)];
        layers.extend(
            self.profiles
                .iter()
                .map(|(n, p)| (Scope::Profile(n.clone()), &p.policy)),
        );
        for (scope, policy) in layers {
            for id in policy.ids() {
                if !seen.insert(id) {
                    errors.push(format!("{scope}: duplicate rule id {id}"));
                }
            }
            if let Err(e) = crate::eval::check_globs(policy) {
                errors.push(format!("{scope}: {e}"));
            }
            for r in &policy.net {
                let id = r.id.as_deref().unwrap_or("?");
                let refs = r.set_headers.values().flat_map(|v| crate::secret_refs(v));
                for s in r.secrets.iter().cloned().chain(refs) {
                    if !self.secrets.contains_key(&s) {
                        errors.push(format!("{scope}: rule {id} uses unknown secret {s}"));
                    }
                }
                for s in r.set_headers.values().flat_map(|v| crate::secret_refs(v)) {
                    if matches!(self.secrets.get(&s), Some(x) if x.mode == SecretMode::Aws) {
                        errors.push(format!(
                            "{scope}: rule {id}: aws secret {s} signs requests; it can't be a header's value"
                        ));
                    }
                }
                let dbauth = |n: &str| matches!(self.secrets.get(n), Some(s) if s.mode == SecretMode::DbAuth);
                match r.db_relay() {
                    Some(proto) => {
                        // The relay injects exactly one database credential and
                        // never parses HTTP, so method/path/header matching and
                        // any secret but a single dbauth one are nonsense here.
                        if r.secrets.len() != 1 || !r.secrets.iter().all(|s| dbauth(s)) {
                            errors.push(format!(
                                "{scope}: rule {id}: a {proto} rule names exactly one dbauth secret"
                            ));
                        }
                        if !r.methods.is_empty() || r.path.is_some() || !r.set_headers.is_empty() {
                            errors.push(format!(
                                "{scope}: rule {id}: a {proto} rule can't carry HTTP method, path or header matching"
                            ));
                        }
                    }
                    None => {
                        if r.secrets.iter().any(|s| dbauth(s)) {
                            errors.push(format!(
                                "{scope}: rule {id}: a dbauth secret needs a database protocol (e.g. protocol: postgres)"
                            ));
                        }
                    }
                }
            }
        }
        for (name, s) in &self.secrets {
            if let SecretSource::Command {
                command,
                timeout_secs,
                ttl_secs,
            } = &s.source
            {
                if command.first().is_none_or(|c| c.trim().is_empty()) {
                    errors.push(format!("secret {name}: command needs a program to run"));
                }
                if *timeout_secs == Some(0) || *ttl_secs == Some(0) {
                    errors.push(format!(
                        "secret {name}: timeout_secs and ttl_secs must be above zero"
                    ));
                }
            }
            match s.mode {
                SecretMode::Proxy if s.hosts.is_empty() => errors.push(format!(
                    "secret {name}: a proxy secret needs hosts it may be sent to"
                )),
                SecretMode::DbAuth => {
                    if s.hosts.is_empty() {
                        errors.push(format!(
                            "secret {name}: a dbauth secret needs hosts (the database it may authenticate to)"
                        ));
                    }
                    if s.user.is_none() {
                        errors.push(format!(
                            "secret {name}: a dbauth secret needs a user to authenticate as"
                        ));
                    }
                    if let Some(m) = &s.mechanism {
                        if !KAFKA_MECHANISMS.contains(&m.as_str()) {
                            errors.push(format!(
                                "secret {name}: unknown SASL mechanism {m:?} (one of {})",
                                KAFKA_MECHANISMS.join(", ")
                            ));
                        }
                    }
                }
                SecretMode::Aws if s.hosts.is_empty() => errors.push(format!(
                    "secret {name}: an aws secret needs hosts it may sign for (e.g. \"*.amazonaws.com\")"
                )),
                _ => {}
            }
            if s.mode != SecretMode::DbAuth
                && (s.user.is_some() || s.database.is_some() || s.mechanism.is_some())
            {
                errors.push(format!(
                    "secret {name}: user/database/mechanism apply only to a dbauth secret"
                ));
            }
        }
        let mut key_ids: BTreeMap<String, &str> = BTreeMap::new();
        for (name, s) in &self.secrets {
            if s.mode == SecretMode::Aws {
                if let Some(other) = key_ids.insert(crate::aws_key_id(name), name) {
                    errors.push(format!(
                        "secrets {other} and {name}: their fake AWS key ids collide; rename one"
                    ));
                }
            }
        }
        for (name, r) in &self.images {
            if name.is_empty()
                || !name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
            {
                errors.push(format!(
                    "images.{name}: an image's name is letters, digits, '-', '_' and '.'"
                ));
            }
            if r.from.trim().is_empty() {
                errors.push(format!("images.{name}: from: the OCI image it starts from"));
            }
            for a in r.args.keys() {
                if !crate::valid_worker_name(a) {
                    errors.push(format!(
                        "images.{name}.args.{a}: lowercase letters, digits and dashes"
                    ));
                }
            }
            for (i, c) in r.copy.iter().enumerate() {
                if !r.args.contains_key(&c.from) {
                    errors.push(format!(
                        "images.{name}.copy[{i}]: from: {:?} isn't one of its args",
                        c.from
                    ));
                }
                if !c.to.starts_with('/') {
                    errors.push(format!(
                        "images.{name}.copy[{i}]: to: where it goes in the image, an absolute path"
                    ));
                }
            }
        }
        for name in self.profiles.keys() {
            if let Err(e) = self.resolve(name) {
                errors.push(format!("profile {name}: {e:#}"));
            }
        }
        for (name, p) in self
            .profiles
            .keys()
            .filter_map(|n| Some((n, self.resolve(n).ok()?)))
        {
            for port in &p.engine.ports {
                let reserved = [
                    crate::proto::CONTROL_PORT,
                    crate::proto::SHELL_PORT,
                    crate::proto::PROXY_PORT,
                    crate::proto::MCP_PORT,
                ];
                if *port == 0 || reserved.contains(&(*port as u32)) {
                    errors.push(format!(
                        "profile {name}: engine.ports: {port} is not a port the guest can offer"
                    ));
                }
            }
            if !p.agent.driven {
                continue;
            }
            if !DRIVABLE_KINDS.contains(&p.agent.kind.as_str()) {
                errors.push(format!(
                    "profile {name}: agent.driven needs a kind the driver runs ({})",
                    DRIVABLE_KINDS.join(", ")
                ));
            }
            if !self.daemon.driver {
                errors.push(format!(
                    "profile {name}: agent.driven, but the driver is off (daemon.driver: false)"
                ));
            }
        }
        if let Some(r) = self.daemon.memory_overcommit {
            if !(r.is_finite() && r > 0.0) {
                errors.push(format!(
                    "daemon.memory_overcommit is {r}; it must be a number above 0"
                ));
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            bail!("invalid config:\n  {}", errors.join("\n  "))
        }
    }

    /// Resolves a profile's `extends` chain.
    pub fn resolve(&self, name: &str) -> Result<ResolvedProfile> {
        let mut chain = Vec::new();
        let mut next = Some(name.to_string());
        while let Some(n) = next {
            if chain.iter().any(|(c, _)| *c == n) {
                bail!("extends cycle through {n}");
            }
            let p = self
                .profiles
                .get(&n)
                .ok_or_else(|| anyhow!("unknown profile {n}"))?;
            next = p.extends.clone();
            chain.push((n, p));
        }
        // Merge from the root ancestor down to the profile itself.
        let mut engine = Value::Object(Default::default());
        let mut agent = Value::Object(Default::default());
        let mut mounts = Vec::new();
        let mut setup = Vec::new();
        for (_, p) in chain.iter().rev() {
            if let Some(e) = &p.engine {
                merge(&mut engine, e);
            }
            if let Some(a) = &p.agent {
                merge(&mut agent, a);
            }
            mounts.extend(p.mounts.iter().cloned());
            setup.extend(p.setup.iter().cloned());
        }
        let engine: Engine = serde_json::from_value(engine).context("engine")?;
        let agent: AgentSpec = serde_json::from_value(agent).context("agent")?;
        if !AGENT_KINDS.contains(&agent.kind.as_str()) {
            bail!(
                "agent.kind {:?}: one of {}",
                agent.kind,
                AGENT_KINDS.join(", ")
            );
        }
        // A generic agent has no harness to put skills or configuration
        // where it reads them: refused, rather than given nowhere.
        if agent.kind == "generic" && (agent.skills.is_some() || !agent.config_mounts.is_empty()) {
            bail!("agent.kind generic takes no skills or config_mounts: no harness reads them");
        }
        let layers = chain
            .into_iter()
            .map(|(n, p)| (Scope::Profile(n), p.policy.clone()))
            .collect();
        Ok(ResolvedProfile {
            name: name.into(),
            engine,
            agent,
            mounts,
            setup,
            layers,
        })
    }
}

/// Deep-merges `over` into `base`: objects merge key by key, anything else
/// replaces, and `null` removes the key.
pub fn merge(base: &mut Value, over: &Value) {
    match (base, over) {
        (Value::Object(b), Value::Object(o)) => {
            for (k, v) in o {
                if v.is_null() {
                    b.remove(k);
                } else {
                    merge(b.entry(k.clone()).or_insert(Value::Null), v);
                }
            }
        }
        (b, o) => *b = o.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::Config;

    /// Skills or config mounts for a generic agent, which no harness would
    /// put anywhere, are refused rather than dropped.
    #[test]
    fn a_generic_agent_takes_no_skills() {
        let c = |agent: &str| Config::from_yaml(&format!("profiles:\n  p:\n    agent: {agent}\n"));
        let e = c("{ kind: generic, skills: ~/skills }").unwrap_err();
        assert!(
            format!("{e:#}").contains("generic takes no skills"),
            "{e:#}"
        );
        assert!(c("{ kind: generic, config_mounts: [{ host: /a, guest: /b }] }").is_err());
        assert!(c("{ kind: generic }").is_ok());
        assert!(c("{ kind: pi, skills: ~/skills }").is_ok());
    }

    /// A recipe's copies take from its own args, to absolute paths.
    #[test]
    fn image_recipes_are_checked() {
        let check = |copy: &str| {
            super::Config::from_yaml(&format!(
                "images:\n  x:\n    from: docker.io/library/ubuntu:26.04\n    args: {{ tool: {{ default: ./tool }} }}\n    copy: [{copy}]\n"
            ))
            .map(|c| c.images["x"].copy.len())
            .map_err(|e| format!("{e:#}"))
        };
        assert_eq!(check("{ from: tool, to: /usr/local/bin/tool }"), Ok(1));
        assert!(check("{ from: other, to: /x }")
            .unwrap_err()
            .contains("isn't one of its args"));
        assert!(check("{ from: tool, to: bin/tool }")
            .unwrap_err()
            .contains("absolute path"));
        assert!(
            check("{ from: tool, to: /x, mode: 755 }").is_err(),
            "unknown fields are refused"
        );
    }

    #[test]
    fn memory_overcommit_is_above_zero() {
        let c = super::Config::from_yaml("daemon: { memory_overcommit: 1.5 }\n").unwrap();
        assert_eq!(c.daemon.memory_overcommit, Some(1.5));
        for bad in ["0", "-1", ".nan"] {
            let e = super::Config::from_yaml(&format!("daemon: {{ memory_overcommit: {bad} }}\n"))
                .map(|_| ())
                .unwrap_err();
            assert!(
                format!("{e:#}").contains("memory_overcommit"),
                "{bad}: {e:#}"
            );
        }
    }

    #[test]
    fn driven_agents_and_the_driver_switch() {
        use super::{Config, DaemonConfig};
        let ok = |y: &str| {
            Config::from_yaml(y)
                .map(|_| ())
                .map_err(|e| format!("{e:#}"))
        };
        let driven =
            "profiles:\n  a: { agent: { kind: claude-code, driven: true } }\n  b: { extends: a }\n";
        ok(driven).unwrap();
        let c = Config::from_yaml(driven).unwrap();
        assert!(c.resolve("b").unwrap().agent.driven, "inherited");
        assert!(!c.resolve("a").unwrap().agent.user.is_empty());
        let e = ok("profiles:\n  g: { agent: { kind: generic, driven: true } }\n").unwrap_err();
        assert!(e.contains("needs a kind the driver runs"), "{e}");
        let off = "daemon: { driver: false }\n";
        ok(off).unwrap();
        let e = ok(&format!("{off}{driven}")).unwrap_err();
        assert!(
            e.contains("profile a: agent.driven, but the driver is off"),
            "{e}"
        );
        assert!(e.contains("profile b: agent.driven"), "{e}");
        assert!(DaemonConfig::default().driver, "on unless turned off");
    }

    #[test]
    fn a_profile_offers_guest_ports_for_services() {
        use super::Config;
        let c = Config::from_yaml(
            "profiles:\n  k: { engine: { ports: [6443] } }\n  l: { extends: k }\n",
        )
        .unwrap();
        assert_eq!(c.resolve("l").unwrap().engine.ports, [6443], "inherited");
        for port in [0, 1000, 1024, 1100, 3128] {
            let e = Config::from_yaml(&format!(
                "profiles:\n  k: {{ engine: {{ ports: [{port}] }} }}\n"
            ))
            .err()
            .map(|e| format!("{e:#}"))
            .unwrap_or_default();
            assert!(
                e.contains(&format!(
                    "engine.ports: {port} is not a port the guest can offer"
                )),
                "{port}: {e}"
            );
        }
    }

    #[test]
    fn command_secrets_are_parsed_and_checked() {
        use super::Config;
        let ok = "secrets:\n  gh: { command: [zapper, mint, github-app, --app, acme, --repo, acme/api, --perm, contents=write], hosts: [github.com, api.github.com], timeout_secs: 20 }\n";
        let c = Config::from_yaml(ok).unwrap();
        let spec = c.secrets["gh"].source.mint_spec().unwrap();
        assert_eq!(spec.argv[0], "zapper");
        assert_eq!(spec.timeout, std::time::Duration::from_secs(20));
        assert!(!c.has_inline_secrets(), "a command holds no secret itself");
        assert_eq!(Config::from_yaml(&c.to_yaml()).unwrap(), c, "round trips");

        let err = |y: &str| {
            format!(
                "{:#}",
                Config::from_yaml(y)
                    .err()
                    .map(|e| e.to_string())
                    .unwrap_or_default()
            )
        };
        assert!(err("secrets:\n  gh: { command: [], hosts: [h] }\n").contains("needs a program"));
        assert!(
            err("secrets:\n  gh: { command: [\"\"], hosts: [h] }\n").contains("needs a program")
        );
        assert!(
            err("secrets:\n  gh: { command: [x], hosts: [h], ttl_secs: 0 }\n")
                .contains("above zero")
        );
        assert!(err("secrets:\n  gh: { command: [x] }\n").contains("needs hosts"));
        assert!(
            !err("secrets:\n  gh: { command: [x], value: y, hosts: [h] }\n").is_empty(),
            "a secret has one source"
        );
    }

    #[test]
    fn aws_secrets_are_validated() {
        use super::Config;
        let err = |y: &str| {
            Config::from_yaml(y)
                .err()
                .map(|e| format!("{e:#}"))
                .unwrap_or_default()
        };
        Config::from_yaml("secrets:\n  aws: { file: /x, mode: aws, hosts: ['*.amazonaws.com'] }\npolicy:\n  net:\n    - { host: '*.amazonaws.com', ports: [443], methods: [GET], secrets: [aws], action: allow }\n").unwrap();
        assert!(
            err("secrets:\n  aws: { file: /x, mode: aws }\n").contains("an aws secret needs hosts")
        );
        // It signs; it is never a header's value.
        assert!(err("secrets:\n  aws: { file: /x, mode: aws, hosts: [h] }\npolicy:\n  net:\n    - { host: h, set_headers: { x-key: '{{secret:aws}}' }, action: allow }\n")
            .contains("signs requests"));
        // Fake key ids are 20 characters, and tell names apart.
        let id = crate::aws_key_id("aws");
        assert_eq!(id.len(), 20);
        assert!(
            crate::is_aws_fake_key_id(&id)
                && id
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
        );
        assert_ne!(id, crate::aws_key_id("aws2"));
    }

    #[test]
    fn dbauth_secrets_and_postgres_rules_are_validated() {
        use super::Config;
        let err = |y: &str| {
            Config::from_yaml(y)
                .err()
                .map(|e| format!("{e:#}"))
                .unwrap_or_default()
        };
        // A well-formed dbauth secret reached by a postgres rule is accepted.
        let good = "\
secrets:\n  db: { value: pw, mode: dbauth, hosts: [db.internal], user: app_ro, database: app }\n\
policy:\n  net:\n    - { host: db.internal, ports: [5432], protocol: postgres, secrets: [db], action: allow }\n";
        Config::from_yaml(good).unwrap();

        // A dbauth secret needs hosts and a user.
        assert!(
            err("secrets:\n  db: { value: pw, mode: dbauth, user: u }\n")
                .contains("a dbauth secret needs hosts")
        );
        assert!(
            err("secrets:\n  db: { value: pw, mode: dbauth, hosts: [h] }\n")
                .contains("a dbauth secret needs a user")
        );
        // user/database are rejected on a non-dbauth secret.
        assert!(err("secrets:\n  t: { value: x, hosts: [h], user: u }\n")
            .contains("apply only to a dbauth secret"));
        // A postgres rule must name exactly one dbauth secret...
        assert!(err(&format!(
            "{good}    - {{ host: h, protocol: postgres, action: allow }}\n"
        ))
        .contains("names exactly one dbauth secret"));
        // ...and a dbauth secret can't be used by a plain tcp rule.
        let tcp = "\
secrets:\n  db: { value: pw, mode: dbauth, hosts: [db.internal], user: u }\n\
policy:\n  net:\n    - { host: db.internal, secrets: [db], action: allow }\n";
        assert!(err(tcp).contains("a dbauth secret needs a database protocol"));
        // A postgres rule can't carry HTTP matching.
        let http = "\
secrets:\n  db: { value: pw, mode: dbauth, hosts: [db.internal], user: u }\n\
policy:\n  net:\n    - { host: db.internal, protocol: postgres, secrets: [db], path: /x, action: allow }\n";
        assert!(err(http).contains("can't carry HTTP method, path or header"));
    }

    #[test]
    fn relative_paths_resolve_against_the_config() {
        super::set_config_dir(std::path::Path::new("/repo/config"));
        assert_eq!(super::expand_home("skills"), "/repo/config/skills");
        assert_eq!(super::expand_home("/abs/x"), "/abs/x");
        let home = std::env::var("HOME").unwrap();
        assert_eq!(super::expand_home("~/.local/x"), format!("{home}/.local/x"));
    }
}
