//! The SSH connector: the user's public keys, each with a name and the
//! sandboxes it reaches, and the lines the host's sshd checks them with.
//!
//! Each key is a line of `~/.ssh/authorized_keys`, between markers the
//! workbench owns (every other line is left as it is), restricted to one
//! command: `zapper-workbench ssh-proxy --key NAME`. sshd runs only that,
//! with no shell and no forwarding of the key's own; the proxy reads the
//! sandbox from `SSH_ORIGINAL_COMMAND`, refuses one the key doesn't reach,
//! and carries SSH to the sandbox's terminals (`zapper-ssh`), the key's name
//! in the record. What a key reaches is read from the workbench's file at
//! each connection, so a change applies without touching `authorized_keys`.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use axum::extract::{Extension, Path as UrlPath, Query};
use axum::http::StatusCode;
use axum::routing::{get, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::facility::{Facility, Panel};
use crate::gateway::{Ctx, Error};
use crate::tokens::Grant;

/// The sandboxes a key reaches.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Reach {
    /// Every sandbox (that has SSH on).
    All,
    /// Those with all these labels.
    Labels(BTreeMap<String, String>),
    /// These, by ID.
    Sessions(Vec<String>),
}

impl Reach {
    /// Whether it reaches the session whose info is `info`.
    pub fn reaches(&self, info: &Value) -> bool {
        match self {
            Reach::All => true,
            Reach::Labels(l) => l
                .iter()
                .all(|(k, v)| info["labels"][k].as_str() == Some(v.as_str())),
            Reach::Sessions(ids) => info["id"]
                .as_str()
                .is_some_and(|id| ids.iter().any(|i| i == id)),
        }
    }
}

/// A key, as the workbench keeps it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Key {
    pub name: String,
    /// `ALGORITHM BASE64`, without options or comment.
    pub key: String,
    pub reach: Reach,
}

/// A key's name: what the record says connected.
pub fn checked_name(name: &str) -> Result<&str> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b));
    if !ok {
        bail!("{name:?} is not a key's name: letters, digits, -, _ and ., at most 64");
    }
    Ok(name)
}

/// The keys file and the `authorized_keys` file the keys are written to.
#[derive(Clone)]
pub struct Keys {
    file: PathBuf,
    authorized: PathBuf,
    /// The forced command, but for the key's name: the proxy, and where
    /// zapper's socket is.
    command: String,
}

const BEGIN: &str = "# zapper-workbench: the keys below are managed by zapper-workbench (its SSH keys); change them there";
const END: &str = "# zapper-workbench: end";

/// `authorized_keys` with the workbench's block made `block` (gone when
/// empty), every other line as it was.
fn with_block(existing: &str, block: &[String]) -> String {
    let mut out: Vec<&str> = vec![];
    let mut inside = false;
    for line in existing.lines() {
        if line == BEGIN {
            inside = true;
        } else if line == END && inside {
            inside = false;
        } else if !inside {
            out.push(line);
        }
    }
    let mut s = out.join("\n");
    if !s.is_empty() {
        s.push('\n');
    }
    if !block.is_empty() {
        s.push_str(BEGIN);
        s.push('\n');
        for l in block {
            s.push_str(l);
            s.push('\n');
        }
        s.push_str(END);
        s.push('\n');
    }
    s
}

impl Keys {
    /// The keys in `file`, written to `authorized`; the forced command runs
    /// `exe ssh-proxy`, finding zapper at `socket`.
    pub fn new(file: PathBuf, authorized: PathBuf, exe: &Path, socket: &Path) -> Result<Keys> {
        for p in [exe, socket] {
            let s = p.display().to_string();
            if s.contains(['"', '\\', '\n', ' ']) {
                bail!("{s:?}: a path a forced command can name, without quotes, backslashes or spaces");
            }
        }
        Ok(Keys {
            file,
            authorized,
            command: format!(
                "ZAPPER_SOCKET={} {} ssh-proxy --key",
                socket.display(),
                exe.display()
            ),
        })
    }

    pub fn list(&self) -> Result<Vec<Key>> {
        match std::fs::read(&self.file) {
            Ok(b) => Ok(serde_json::from_slice(&b).context("ssh-keys.json")?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(vec![]),
            Err(e) => Err(e.into()),
        }
    }

    /// The key named `name`, if there is one.
    pub fn get(&self, name: &str) -> Result<Option<Key>> {
        Ok(self.list()?.into_iter().find(|k| k.name == name))
    }

    /// The `authorized_keys` line for `k`.
    fn line(&self, k: &Key) -> String {
        format!(
            "restrict,command=\"{} {}\" {} {}",
            self.command, k.name, k.key, k.name
        )
    }

    /// Writes the keys, and `authorized_keys` from them.
    fn store(&self, keys: &[Key]) -> Result<()> {
        write_private(&self.file, &serde_json::to_vec_pretty(keys)?)?;
        if let Some(dir) = self.authorized.parent() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)?;
        }
        let existing = match std::fs::read_to_string(&self.authorized) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e.into()),
        };
        let block: Vec<String> = keys.iter().map(|k| self.line(k)).collect();
        write_private(&self.authorized, with_block(&existing, &block).as_bytes())
    }

    /// Adds or replaces key `name`.
    pub fn put(&self, name: &str, key: &str, reach: Reach) -> Result<Key> {
        let name = checked_name(name)?.to_string();
        let key = zapper_ssh::public_key(key)?;
        if let Reach::Sessions(ids) = &reach {
            for id in ids {
                zapper_ssh::sandbox_id(id)?;
            }
        }
        let mut keys = self.list()?;
        if keys.iter().any(|k| k.key == key && k.name != name) {
            bail!("that key is in the workbench already, under another name");
        }
        keys.retain(|k| k.name != name);
        let k = Key { name, key, reach };
        keys.push(k.clone());
        self.store(&keys)?;
        Ok(k)
    }

    /// Removes key `name`; whether there was one.
    pub fn remove(&self, name: &str) -> Result<bool> {
        let mut keys = self.list()?;
        let before = keys.len();
        keys.retain(|k| k.name != name);
        if keys.len() == before {
            return Ok(false);
        }
        self.store(&keys)?;
        Ok(true)
    }
}

/// Writes `bytes` to `path`, readable by this user only.
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("zapper-workbench.tmp");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("writing {}", tmp.display()))?;
    f.write_all(bytes)?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// The facility: the keys, managed by the owner's token only, since a key
/// is a shell in the sandboxes it reaches, which a token limited to some
/// mustn't hand out for more.
pub struct Ssh {
    keys: Keys,
    /// The gateway's host key's `known_hosts` line.
    known_hosts: String,
}

impl Ssh {
    pub fn new(keys: Keys, known_hosts: String) -> Ssh {
        Ssh { keys, known_hosts }
    }
}

fn owner(g: &Grant) -> Result<(), Error> {
    if *g == Grant::owner() {
        Ok(())
    } else {
        Err(Error(
            StatusCode::FORBIDDEN,
            "only the owner's token manages SSH keys".into(),
        ))
    }
}

fn bad(e: anyhow::Error) -> Error {
    Error(StatusCode::BAD_REQUEST, format!("{e:#}"))
}

#[derive(Clone)]
struct Shared(Arc<Ssh>);

/// A key as listed: its name, what it reaches, and enough of the key to
/// tell it (its algorithm and its end).
fn view(k: &Key) -> Value {
    let (algo, blob) = k.key.split_once(' ').unwrap_or(("?", ""));
    let tail: String = blob
        .chars()
        .rev()
        .take(12)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    json!({ "name": k.name, "reach": k.reach, "key": format!("{algo} …{tail}") })
}

async fn list(
    Extension(g): Extension<Grant>,
    Extension(s): Extension<Shared>,
) -> Result<Json<Value>, Error> {
    owner(&g)?;
    Ok(Json(Value::Array(
        s.0.keys.list()?.iter().map(view).collect(),
    )))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PutKey {
    key: String,
    reach: Reach,
}

async fn put_key(
    Extension(g): Extension<Grant>,
    Extension(s): Extension<Shared>,
    UrlPath(name): UrlPath<String>,
    Json(p): Json<PutKey>,
) -> Result<Json<Value>, Error> {
    owner(&g)?;
    let k = s.0.keys.put(&name, &p.key, p.reach).map_err(bad)?;
    Ok(Json(view(&k)))
}

async fn remove_key(
    Extension(g): Extension<Grant>,
    Extension(s): Extension<Shared>,
    UrlPath(name): UrlPath<String>,
) -> Result<Json<Value>, Error> {
    owner(&g)?;
    if !s.0.keys.remove(&name)? {
        return Err(Error(StatusCode::NOT_FOUND, format!("no key {name}")));
    }
    Ok(Json(json!({ "removed": name })))
}

#[derive(Deserialize)]
struct ConfigQuery {
    /// This host as the user's machine reaches it: `user@host`.
    host: Option<String>,
}

/// What the user's machine needs: its `~/.ssh/config` block and the
/// `known_hosts` line for the gateway's one host key.
async fn client_config(
    Extension(g): Extension<Grant>,
    Extension(s): Extension<Shared>,
    Query(q): Query<ConfigQuery>,
) -> Result<Json<Value>, Error> {
    owner(&g)?;
    let host = q.host.unwrap_or_else(|| "you@this-host".into());
    let plain = !host.is_empty()
        && host.len() <= 255
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"@.-_:[]".contains(&b));
    if !plain {
        return Err(Error(
            StatusCode::BAD_REQUEST,
            format!("{host:?} isn't user@host"),
        ));
    }
    let config = format!(
        "Host *.zapper\n    ProxyCommand ssh -T -i ~/.ssh/YOUR_KEY {host} %h\n    HostKeyAlias zapper\n    User user\n"
    );
    Ok(Json(
        json!({ "ssh_config": config, "known_hosts": s.0.known_hosts }),
    ))
}

/// A sandbox reached through zapper's API, for the key `key`: each terminal
/// the SSH gateway's (refused while the sandbox's SSH is off, closed when it
/// goes off), named for the key in the record.
struct Sandbox {
    zapper: crate::zapper::Zapper,
    id: String,
    key: String,
}

impl zapper_ssh::Sandbox for Sandbox {
    async fn terminal(&self, t: zapper_ssh::Terminal) -> Result<Box<dyn zapper_ssh::Stream>> {
        use crate::zapper::url_encode;
        let mut q = vec![
            "ssh=true".to_string(),
            format!("client={}", url_encode(&self.key)),
        ];
        if let Some((r, c)) = t.tty {
            q.push(format!("tty={r}x{c}"));
        }
        if !t.argv.is_empty() {
            q.push(format!(
                "argv={}",
                url_encode(&serde_json::to_string(&t.argv)?)
            ));
        }
        if !t.env.is_empty() {
            q.push(format!(
                "env={}",
                url_encode(&serde_json::to_string(&t.env)?)
            ));
        }
        let (io, _) = self
            .zapper
            .upgrade(&format!("/v1/sessions/{}/attach?{}", self.id, q.join("&")))
            .await?;
        Ok(Box::new(io))
    }
}

/// `zapper-workbench ssh-proxy --key NAME`: what sshd runs for the key. It
/// speaks SSH on stdin and stdout for the sandbox `target` names (else
/// `$SSH_ORIGINAL_COMMAND`), if the key is still the workbench's and reaches
/// it. Stdout is the SSH stream: what goes wrong is said on stderr.
pub async fn proxy(
    zapper: crate::zapper::Zapper,
    keys: &Keys,
    key: &str,
    target: Option<String>,
    host_key: &Path,
) -> Result<()> {
    let target = target
        .or_else(|| std::env::var("SSH_ORIGINAL_COMMAND").ok())
        .context("no sandbox named: `ssh ID.zapper`")?;
    let id = zapper_ssh::sandbox_id(&target)?;
    let Some(k) = keys.get(key)? else {
        bail!("key {key} is no longer the workbench's");
    };
    let info = zapper
        .call(axum::http::Method::GET, &format!("/v1/sessions/{id}"), None)
        .await?
        .ok()
        .map_err(|(_, why)| anyhow::anyhow!("{why}"))?;
    // A sandbox the key doesn't reach is answered as one that isn't there.
    if !k.reach.reaches(&info) {
        bail!("no sandbox {id} for key {key}");
    }
    if info["ssh"] != true {
        bail!("SSH is off for sandbox {id}");
    }
    let host_key = zapper_ssh::host_key(host_key)?;
    let stdio = tokio::io::join(tokio::io::stdin(), tokio::io::stdout());
    let sandbox = Arc::new(Sandbox {
        zapper,
        id,
        key: key.to_string(),
    });
    zapper_ssh::serve(sandbox, host_key, stdio).await
}

const PANEL: &str = include_str!("ssh.js");

impl Facility for Ssh {
    fn name(&self) -> &'static str {
        "ssh"
    }

    fn routes(&self) -> Router<Ctx> {
        let shared = Shared(Arc::new(Ssh {
            keys: self.keys.clone(),
            known_hosts: self.known_hosts.clone(),
        }));
        Router::new()
            .route("/keys", get(list))
            .route("/keys/{name}", put(put_key).delete(remove_key))
            .route("/config", get(client_config))
            .layer(Extension(shared))
    }

    fn panel(&self) -> Option<Panel> {
        Some(Panel {
            title: "SSH keys",
            script: PANEL,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_workbenchs_block_is_its_own_and_the_rest_stays() {
        let mine = vec!["restrict,command=\"x\" ssh-ed25519 AAAA a".to_string()];
        let theirs = "ssh-rsa BBBB me@laptop\n# a note\n";
        let once = with_block(theirs, &mine);
        assert!(once.starts_with(theirs), "{once}");
        assert!(once.contains(BEGIN) && once.contains(&mine[0]));
        // Again: replaced, not added to.
        let twice = with_block(&once, &["restrict ssh-ed25519 CCCC c".into()]);
        assert_eq!(twice.matches(BEGIN).count(), 1);
        assert!(!twice.contains("AAAA") && twice.contains("CCCC"));
        assert!(twice.contains("ssh-rsa BBBB me@laptop"));
        // No keys: no block.
        assert_eq!(with_block(&twice, &[]), theirs);
        assert_eq!(with_block("", &[]), "");
    }

    #[test]
    fn a_key_reaches_all_sandboxes_some_by_label_or_some_by_id() {
        let info = json!({ "id": "abc234", "labels": { "project": "x" } });
        assert!(Reach::All.reaches(&info));
        assert!(Reach::Labels([("project".into(), "x".into())].into()).reaches(&info));
        assert!(!Reach::Labels([("project".into(), "y".into())].into()).reaches(&info));
        assert!(Reach::Sessions(vec!["abc234".into()]).reaches(&info));
        assert!(!Reach::Sessions(vec!["zzz999".into()]).reaches(&info));
    }

    #[test]
    fn a_forced_command_names_paths_it_can_quote() {
        let f = PathBuf::from("/tmp/k.json");
        assert!(Keys::new(
            f.clone(),
            f.clone(),
            Path::new("/usr/bin/zw"),
            Path::new("/run/s.sock")
        )
        .is_ok());
        assert!(Keys::new(
            f.clone(),
            f.clone(),
            Path::new("/a b/zw"),
            Path::new("/run/s.sock")
        )
        .is_err());
        assert!(Keys::new(
            f.clone(),
            f,
            Path::new("/usr/bin/zw"),
            Path::new("/run/\"x")
        )
        .is_err());
    }
}
