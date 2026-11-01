//! The record: firms' journals and transcripts, sessions' audit logs and
//! terminal recordings. Every write to it goes through here.
//!
//! With `daemon.keeper` configured, the record is appended through
//! `zapper-keeper`, a separate user who owns its files: the daemon can add
//! lines and never change or remove them, and reads them from the keeper's
//! directory. Without it, the record is written under `state` as before.
//! Paths stay the state paths everywhere else; `view` maps a path to where
//! it can be read.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use zapper_keeper::Client;

struct Keeper {
    client: Client,
    socket: PathBuf,
    dir: PathBuf,
    public_key: String,
}

struct Records {
    state: PathBuf,
    keeper: Option<Keeper>,
}

static RECORDS: OnceLock<Records> = OnceLock::new();
/// False after a write to the record failed, until one succeeds again.
static HEALTHY: AtomicBool = AtomicBool::new(true);

/// Sets up the record at the daemon's start. With a keeper configured, it
/// must answer: the daemon doesn't run without its record.
pub fn init(state: &Path, keeper: Option<&zapper_policy::KeeperConfig>) -> Result<()> {
    let keeper = match keeper {
        None => None,
        Some(k) => {
            let socket = PathBuf::from(zapper_policy::expand_home(&k.socket));
            let dir = PathBuf::from(zapper_policy::expand_home(&k.dir));
            let client = Client::new(&socket);
            let hello = client.hello().with_context(|| "daemon.keeper is set, and the keeper doesn't answer; is zapper-keeper running? (scripts/install-keeper.sh)")?;
            let public_key = hello["public_key"].as_str().unwrap_or("").to_string();
            let on_disk = std::fs::read_to_string(dir.join("keeper.pub"))
                .with_context(|| format!("reading the keeper's public key in {}", dir.display()))?;
            if on_disk.trim() != public_key {
                bail!(
                    "the keeper on {} and the directory {} don't match (different public keys)",
                    socket.display(),
                    dir.display()
                );
            }
            Some(Keeper {
                client,
                socket,
                dir,
                public_key,
            })
        }
    };
    let _ = RECORDS.set(Records {
        state: state.to_path_buf(),
        keeper,
    });
    Ok(())
}

fn records() -> Option<&'static Records> {
    RECORDS.get()
}

/// The stream a state path is, when the keeper keeps the record.
fn stream(path: &Path) -> Option<(&'static Keeper, String)> {
    let r = records()?;
    let k = r.keeper.as_ref()?;
    let rel = path.strip_prefix(&r.state).ok()?;
    let s = rel.to_str()?.to_string();
    zapper_keeper::valid_stream(&s).then_some((k, s))
}

/// Where a file of the record can be read: in the keeper's directory when
/// it keeps it, else where it was written.
pub fn view(path: &Path) -> PathBuf {
    match stream(path) {
        Some((k, s)) => k.dir.join(s),
        None => path.to_path_buf(),
    }
}

/// Whether the record is being written.
pub fn healthy() -> bool {
    HEALTHY.load(Ordering::Relaxed)
}

/// An append-only file of the record: written here, or through the keeper.
pub struct Writer(Sink);

enum Sink {
    Local(File),
    Kept(&'static Keeper, String),
}

impl Writer {
    pub fn open(path: &Path) -> Result<Writer> {
        if let Some((k, s)) = stream(path) {
            return Ok(Writer(Sink::Kept(k, s)));
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("creating {}", path.display()))?;
        Ok(Writer(Sink::Local(f)))
    }

    /// Appends one line (without its newline).
    pub fn line(&mut self, line: &[u8]) -> Result<()> {
        if line.contains(&b'\n') {
            bail!("a record line can't contain a newline");
        }
        let r = match &mut self.0 {
            Sink::Local(f) => {
                let mut buf = Vec::with_capacity(line.len() + 1);
                buf.extend_from_slice(line);
                buf.push(b'\n');
                f.write_all(&buf).map_err(|e| anyhow!(e))
            }
            Sink::Kept(k, s) => k.client.append(s, line).map(|_| ()),
        };
        match &r {
            Ok(()) => HEALTHY.store(true, Ordering::Relaxed),
            Err(e) => {
                if HEALTHY.swap(false, Ordering::Relaxed) {
                    tracing::error!(
                        "the record can't be written: {e:#}; firms stop taking turns until it can"
                    );
                }
            }
        }
        r
    }
}

/// The record's status, for `zapper check`, the API and the UI.
pub fn status() -> Value {
    match records().and_then(|r| r.keeper.as_ref()) {
        None => {
            json!({ "keeper": false, "healthy": healthy(), "dir": records().map(|r| r.state.display().to_string()) })
        }
        Some(k) => {
            let answers = k.client.hello().is_ok();
            json!({
                "keeper": true, "healthy": healthy() && answers, "answers": answers,
                "socket": k.socket.display().to_string(), "dir": k.dir.display().to_string(),
                "public_key": k.public_key, "fingerprint": k.public_key.get(..16).unwrap_or(""),
            })
        }
    }
}

/// Verifies the files under `dir`, a directory of the daemon's state (a
/// session's, a run's), against the keeper's chains: an error if it isn't in
/// the record, or there is no keeper.
pub fn verify_state_dir(dir: &Path) -> Result<Value> {
    let rel = rel(dir).ok_or_else(|| anyhow!("{} isn't in the record", dir.display()))?;
    verify_dir(&rel)
}

/// Verifies files of the record against the keeper's chains: every file
/// under `rel` (a directory of the state, like a run's) that has a chain.
fn verify_dir(rel: &Path) -> Result<Value> {
    let r = records().ok_or_else(|| anyhow!("no record"))?;
    let Some(k) = r.keeper.as_ref() else {
        bail!("the record isn't kept by a keeper here, so it has no chains to verify")
    };
    let root = k.dir.join(rel);
    let mut out = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            let name = p.to_string_lossy();
            if name.ends_with(".chain") || !(name.ends_with(".jsonl") || name.ends_with(".cast")) {
                continue;
            }
            let stream = p
                .strip_prefix(&k.dir)
                .ok()
                .and_then(|x| x.to_str())
                .unwrap_or("")
                .to_string();
            let res = zapper_keeper::verify(&p, &stream, &k.public_key);
            out.push(match res {
                Ok(rep) => json!({ "file": stream, "ok": true, "lines": rep.lines, "signed": rep.signed, "checkpoints": rep.checkpoints }),
                Err(e) => json!({ "file": stream, "ok": false, "error": format!("{e:#}") }),
            });
        }
    }
    out.sort_by(|a, b| a["file"].as_str().cmp(&b["file"].as_str()));
    let ok = out.iter().all(|x| x["ok"] == json!(true));
    Ok(json!({ "ok": ok, "files": out, "public_key": k.public_key }))
}

/// A state path's place in the record, for verifying.
fn rel(path: &Path) -> Option<PathBuf> {
    path.strip_prefix(&records()?.state)
        .ok()
        .map(Path::to_path_buf)
}
