//! The keeper of zapper's record: firms' journals and transcripts, sessions'
//! audit logs and terminal recordings.
//!
//! It runs as its own OS user and owns the record's files; the user who runs
//! zapper can read them but not change them. The daemon sends it one kind of
//! request over a unix socket, append a line to a stream, and gets back only
//! an acknowledgement: nothing is read, rewritten or removed through it. So a
//! compromised daemon can add to the record but can't change what is there.
//!
//! Next to each file the keeper keeps a hash chain (`<file>.chain`): every
//! line's hash covers the line and the hash before it, and every so often a
//! checkpoint signed with a key only the keeper can read. `verify` checks a
//! file against its chain, so a change made around the keeper (by root, or on
//! a copy) shows too.

pub mod ui;

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use ring::digest;
use ring::signature::{self, Ed25519KeyPair, KeyPair};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Longest line the keeper takes, in bytes.
pub const MAX_LINE: usize = 4 << 20;
/// A checkpoint every this many lines of a stream, and after this long.
pub const CHECKPOINT_EVERY: u64 = 64;
pub const CHECKPOINT_SECS: f64 = 30.0;

/// One request: `{"op": "append", "stream": "...", "data": "..."}` appends
/// `data` and a newline to the stream; `{"op": "hello"}` names the keeper.
#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    pub op: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub stream: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub data: String,
}

/// Whether `stream` may name a file of the record: a relative path of plain
/// names (letters, digits, `.`, `_`, `-`, none starting with a dot), ending
/// in `.jsonl` or `.cast`.
pub fn valid_stream(stream: &str) -> bool {
    let parts: Vec<&str> = stream.split('/').collect();
    stream.len() <= 512
        && parts.len() <= 10
        && parts.iter().all(|p| {
            !p.is_empty()
                && !p.starts_with('.')
                && p.len() <= 128
                && p.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        })
        && (stream.ends_with(".jsonl") || stream.ends_with(".cast"))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Result<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        bail!("bad hex");
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| anyhow!("bad hex")))
        .collect()
}

/// The hash before a stream's first line.
pub fn genesis(stream: &str) -> Vec<u8> {
    digest::digest(
        &digest::SHA256,
        format!("zapper-keeper\n{stream}").as_bytes(),
    )
    .as_ref()
    .to_vec()
}

/// The hash of a line: over the previous hash and the line's bytes.
pub fn link(prev: &[u8], line: &[u8]) -> Vec<u8> {
    let mut ctx = digest::Context::new(&digest::SHA256);
    ctx.update(prev);
    ctx.update(line);
    ctx.finish().as_ref().to_vec()
}

/// What a checkpoint signs.
pub fn checkpoint_message(stream: &str, seq: u64, hash: &str, ts: f64) -> String {
    format!("zapper-keeper checkpoint\n{stream}\n{seq}\n{hash}\n{ts}")
}

fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// A line of a chain file: an entry, or a signed checkpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainLine {
    pub seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub off: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub len: Option<u64>,
    pub hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ts: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sig: Option<String>,
}

/// A stream the keeper is appending to.
struct Stream {
    name: String,
    data: File,
    chain: File,
    size: u64,
    seq: u64,
    hash: Vec<u8>,
    signed_seq: u64,
    signed_at: f64,
}

impl Stream {
    /// Opens a stream, picking up where its chain left off; lines appended
    /// to the data but not chained (a crash between the two writes) are
    /// chained now.
    fn open(root: &Path, name: &str, group: Option<u32>) -> Result<Stream> {
        let path = root.join(name);
        let fresh = !path.exists();
        if let Some(dir) = path.parent() {
            create_dirs(root, dir, group)?;
        }
        let mode = if group.is_some() { 0o640 } else { 0o644 };
        let data = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .mode(mode)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        let chain_path = chain_path(&path);
        let chain = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .mode(mode)
            .open(&chain_path)?;
        if let Some(g) = group {
            give(&data, g)?;
            give(&chain, g)?;
        }
        if fresh {
            // The new names themselves must survive a crash too.
            sync_dir(path.parent().unwrap_or(root));
        }
        let mut s = Stream {
            name: name.into(),
            size: 0,
            seq: 0,
            hash: genesis(name),
            signed_seq: 0,
            signed_at: now(),
            data,
            chain,
        };
        // The chain's last entry, and the data it covers.
        let mut covered = 0u64;
        for line in BufReader::new(File::open(&chain_path)?).lines() {
            let line = line?;
            let Ok(c) = serde_json::from_str::<ChainLine>(&line) else {
                continue;
            };
            if c.sig.is_some() {
                s.signed_seq = c.seq;
                continue;
            }
            s.seq = c.seq;
            s.hash = unhex(&c.hash)?;
            covered = c.off.unwrap_or(0) + c.len.unwrap_or(0) + 1;
        }
        s.size = s.data.metadata()?.len();
        if s.size > covered {
            let mut f = File::open(&path)?;
            f.seek(SeekFrom::Start(covered))?;
            let mut rest = Vec::new();
            f.read_to_end(&mut rest)?;
            let mut off = covered;
            for line in rest.split_inclusive(|b| *b == b'\n') {
                if !line.ends_with(b"\n") {
                    break;
                }
                s.chain_entry(off, &line[..line.len() - 1])?;
                off += line.len() as u64;
            }
        }
        Ok(s)
    }

    fn chain_entry(&mut self, off: u64, line: &[u8]) -> Result<()> {
        self.seq += 1;
        self.hash = link(&self.hash, line);
        let c = ChainLine {
            seq: self.seq,
            off: Some(off),
            len: Some(line.len() as u64),
            hash: hex(&self.hash),
            ts: None,
            sig: None,
        };
        let mut out = serde_json::to_vec(&c)?;
        out.push(b'\n');
        self.chain.write_all(&out)?;
        Ok(())
    }

    /// Appends a line, durably: it is on disk before the daemon hears back.
    /// The chain needn't be: after a crash it is rebuilt from the data.
    fn append(&mut self, line: &[u8]) -> Result<u64> {
        let off = self.data.metadata()?.len();
        let mut buf = Vec::with_capacity(line.len() + 1);
        buf.extend_from_slice(line);
        buf.push(b'\n');
        self.data.write_all(&buf)?;
        self.data.sync_data()?;
        self.size = off + buf.len() as u64;
        self.chain_entry(off, line)?;
        Ok(self.seq)
    }

    fn checkpoint(&mut self, key: &Ed25519KeyPair) -> Result<()> {
        if self.seq == self.signed_seq {
            return Ok(());
        }
        let ts = (now() * 1000.0).round() / 1000.0;
        let h = hex(&self.hash);
        let sig = key.sign(checkpoint_message(&self.name, self.seq, &h, ts).as_bytes());
        let c = ChainLine {
            seq: self.seq,
            off: None,
            len: None,
            hash: h,
            ts: Some(ts),
            sig: Some(hex(sig.as_ref())),
        };
        let mut out = serde_json::to_vec(&c)?;
        out.push(b'\n');
        self.chain.write_all(&out)?;
        let _ = self.data.sync_data();
        let _ = self.chain.sync_data();
        self.signed_seq = self.seq;
        self.signed_at = now();
        Ok(())
    }
}

/// Creates `dir` and its parents under `root`, flushing each new entry;
/// with a group, the group may read them and others may not.
fn create_dirs(root: &Path, dir: &Path, group: Option<u32>) -> Result<()> {
    if dir.exists() {
        return Ok(());
    }
    if let Some(parent) = dir.parent() {
        if parent != root && parent.starts_with(root) {
            create_dirs(root, parent, group)?;
        }
    }
    std::fs::create_dir(dir).or_else(|e| {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            Ok(())
        } else {
            Err(e)
        }
    })?;
    if let Some(g) = group {
        give(&File::open(dir)?, g)?;
        std::fs::set_permissions(dir, std::os::unix::fs::PermissionsExt::from_mode(0o750))?;
    }
    sync_dir(dir.parent().unwrap_or(root));
    Ok(())
}

/// Gives a file the writer's group: the keeper's user is a member of it.
pub fn give(f: &File, gid: u32) -> Result<()> {
    if unsafe { libc::fchown(f.as_raw_fd(), u32::MAX, gid) } != 0 {
        bail!(
            "chown to group {gid}: {} (is the keeper's user in that group?)",
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

fn sync_dir(dir: &Path) {
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
}

pub fn chain_path(data: &Path) -> PathBuf {
    let mut p = data.as_os_str().to_owned();
    p.push(".chain");
    PathBuf::from(p)
}

/// The keeper's key: made on first start, readable only by the keeper. Its
/// public half is written to `<dir>/keeper.pub` for `verify`.
pub fn load_key(path: &Path, dir: &Path) -> Result<Ed25519KeyPair> {
    let pkcs8 = match std::fs::read(path) {
        Ok(b) => b,
        Err(_) => {
            let rng = ring::rand::SystemRandom::new();
            let doc = Ed25519KeyPair::generate_pkcs8(&rng).map_err(|_| anyhow!("making a key"))?;
            let mut f = OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(path)
                .with_context(|| format!("writing {}", path.display()))?;
            f.write_all(doc.as_ref())?;
            doc.as_ref().to_vec()
        }
    };
    let key = Ed25519KeyPair::from_pkcs8(&pkcs8)
        .map_err(|_| anyhow!("{} is not a keeper key", path.display()))?;
    let public = format!("{}\n", hex(key.public_key().as_ref()));
    if std::fs::read_to_string(dir.join("keeper.pub"))
        .ok()
        .as_deref()
        != Some(public.as_str())
    {
        let tmp = dir.join(".keeper.pub.tmp");
        std::fs::write(&tmp, &public)?;
        std::fs::set_permissions(&tmp, std::os::unix::fs::PermissionsExt::from_mode(0o644))?;
        std::fs::rename(&tmp, dir.join("keeper.pub"))?;
    }
    Ok(key)
}

/// The uid of the process at the other end of a unix socket.
pub fn peer_uid(s: &UnixStream) -> Result<u32> {
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let r = unsafe {
        libc::getsockopt(
            s.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    if r != 0 {
        bail!("SO_PEERCRED: {}", std::io::Error::last_os_error());
    }
    Ok(cred.uid)
}

/// Free space the keeper leaves on its filesystem: below it, appends are
/// refused, so a flood of appends (from a compromised writer) can't fill
/// the disk. The daemon then stops its firms rather than run unrecorded.
pub const MIN_FREE: u64 = 1 << 30;

/// Bytes free on the filesystem of `dir`, for unprivileged users.
pub fn free_bytes(dir: &Path) -> Option<u64> {
    let c = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    Some((st.f_bavail as u64).saturating_mul(st.f_frsize as u64))
}

/// The keeper's state: open streams, the key.
pub struct Keeper {
    root: PathBuf,
    group: Option<u32>,
    min_free: u64,
    key: Ed25519KeyPair,
    streams: Mutex<std::collections::HashMap<String, Stream>>,
}

/// Streams kept open at once; beyond it, all are checkpointed and closed.
const MAX_OPEN: usize = 2048;

impl Keeper {
    pub fn new(root: &Path, key: Ed25519KeyPair) -> Keeper {
        Keeper {
            root: root.into(),
            group: None,
            min_free: MIN_FREE,
            key,
            streams: Mutex::new(Default::default()),
        }
    }

    pub fn with_min_free(mut self, bytes: u64) -> Keeper {
        self.min_free = bytes;
        self
    }

    /// Files readable by this group (the writer's) and not by others.
    pub fn with_group(mut self, gid: Option<u32>) -> Keeper {
        self.group = gid;
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn append(&self, stream: &str, data: &str) -> Result<u64> {
        if !valid_stream(stream) {
            bail!("{stream:?} is not a stream name");
        }
        if data.len() > MAX_LINE || data.contains('\n') {
            bail!("a line of at most {MAX_LINE} bytes, without newlines");
        }
        if free_bytes(&self.root).is_some_and(|f| f < self.min_free + data.len() as u64) {
            bail!(
                "the record's disk has less than {} MiB free; refusing to fill it",
                self.min_free >> 20
            );
        }
        let mut streams = self.streams.lock().unwrap();
        if !streams.contains_key(stream) {
            if streams.len() >= MAX_OPEN {
                for s in streams.values_mut() {
                    let _ = s.checkpoint(&self.key);
                }
                streams.clear();
            }
            streams.insert(
                stream.to_string(),
                Stream::open(&self.root, stream, self.group)?,
            );
        }
        let s = streams.get_mut(stream).unwrap();
        let seq = s.append(data.as_bytes())?;
        if seq - s.signed_seq >= CHECKPOINT_EVERY {
            s.checkpoint(&self.key)?;
        }
        Ok(seq)
    }

    /// Signs what isn't signed yet, in streams idle long enough.
    pub fn tick(&self) {
        let t = now();
        for s in self.streams.lock().unwrap().values_mut() {
            if s.seq != s.signed_seq && t - s.signed_at >= CHECKPOINT_SECS {
                let _ = s.checkpoint(&self.key);
            }
        }
    }

    pub fn checkpoint_all(&self) {
        for s in self.streams.lock().unwrap().values_mut() {
            let _ = s.checkpoint(&self.key);
        }
    }

    pub fn public_key(&self) -> String {
        hex(self.key.public_key().as_ref())
    }

    /// Serves one connection: requests in, acknowledgements out.
    pub fn serve(&self, s: UnixStream) {
        let Ok(reader) = s.try_clone() else { return };
        let mut w = s;
        let mut reader = BufReader::new(reader.take(u64::MAX));
        let mut line = Vec::new();
        loop {
            line.clear();
            match (&mut reader)
                .take(MAX_LINE as u64 + 4096)
                .read_until(b'\n', &mut line)
            {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
            let reply = match serde_json::from_slice::<Request>(&line) {
                Err(e) => json!({ "error": format!("bad request: {e}") }),
                Ok(r) if r.op == "append" => match self.append(&r.stream, &r.data) {
                    Ok(seq) => json!({ "ok": { "seq": seq } }),
                    Err(e) => json!({ "error": format!("{e:#}") }),
                },
                Ok(r) if r.op == "hello" => {
                    json!({ "ok": { "keeper": 1, "public_key": self.public_key() } })
                }
                Ok(r) => json!({ "error": format!("no op {:?}: the keeper only appends", r.op) }),
            };
            let mut out = reply.to_string().into_bytes();
            out.push(b'\n');
            if w.write_all(&out).is_err() {
                return;
            }
        }
    }
}

/// The daemon's side: one connection, reopened when it breaks.
pub struct Client {
    path: PathBuf,
    conn: Mutex<Option<(UnixStream, BufReader<UnixStream>)>>,
}

impl Client {
    pub fn new(path: &Path) -> Client {
        Client {
            path: path.into(),
            conn: Mutex::new(None),
        }
    }

    fn connect(&self) -> Result<(UnixStream, BufReader<UnixStream>)> {
        let s = UnixStream::connect(&self.path)
            .with_context(|| format!("the keeper at {}", self.path.display()))?;
        s.set_read_timeout(Some(Duration::from_secs(10)))?;
        s.set_write_timeout(Some(Duration::from_secs(10)))?;
        let r = BufReader::new(s.try_clone()?);
        Ok((s, r))
    }

    fn call(&self, req: &Value) -> Result<Value> {
        let mut out = req.to_string().into_bytes();
        out.push(b'\n');
        let mut conn = self.conn.lock().unwrap();
        // Once on the open connection, once more on a fresh one.
        for attempt in 0..2 {
            if conn.is_none() {
                *conn = Some(self.connect()?);
            }
            let (w, r) = conn.as_mut().unwrap();
            let mut line = String::new();
            let ok =
                w.write_all(&out).is_ok() && r.read_line(&mut line).map(|n| n > 0).unwrap_or(false);
            if !ok {
                *conn = None;
                if attempt == 1 {
                    bail!("the keeper at {} didn't answer", self.path.display());
                }
                continue;
            }
            let v: Value = serde_json::from_str(&line).context("the keeper's answer")?;
            if let Some(e) = v["error"].as_str() {
                bail!("the keeper: {e}");
            }
            return Ok(v["ok"].clone());
        }
        unreachable!()
    }

    pub fn hello(&self) -> Result<Value> {
        self.call(&json!({ "op": "hello" }))
    }

    /// Appends `line` (without its newline) to `stream`; its sequence number.
    pub fn append(&self, stream: &str, line: &[u8]) -> Result<u64> {
        let data = std::str::from_utf8(line).map_err(|_| anyhow!("the record is text"))?;
        let v = self.call(&json!({ "op": "append", "stream": stream, "data": data }))?;
        v["seq"]
            .as_u64()
            .ok_or_else(|| anyhow!("the keeper's answer has no seq"))
    }
}

/// What `verify` found.
#[derive(Debug, Serialize)]
pub struct Report {
    pub lines: u64,
    pub signed: u64,
    pub checkpoints: u64,
}

/// Checks a file of the record against its chain and the keeper's public
/// key (hex): every line hashes to its chain entry, the entries link, and
/// every checkpoint's signature holds. Lines after the last checkpoint are
/// chained but not yet signed.
pub fn verify(data_path: &Path, stream: &str, public_key: &str) -> Result<Report> {
    let key = unhex(public_key.trim())?;
    let pk = signature::UnparsedPublicKey::new(&signature::ED25519, key);
    let mut data = Vec::new();
    File::open(data_path)
        .with_context(|| format!("{}", data_path.display()))?
        .read_to_end(&mut data)?;
    let chain = File::open(chain_path(data_path))
        .with_context(|| format!("{}", chain_path(data_path).display()))?;
    let mut hash = genesis(stream);
    let (mut seq, mut covered, mut signed, mut checkpoints) = (0u64, 0u64, 0u64, 0u64);
    for (n, line) in BufReader::new(chain).lines().enumerate() {
        let line = line?;
        let c: ChainLine =
            serde_json::from_str(&line).with_context(|| format!("chain line {}", n + 1))?;
        if let Some(sig) = &c.sig {
            if c.seq != seq || c.hash != hex(&hash) {
                bail!(
                    "checkpoint at chain line {} doesn't match the chain (seq {} vs {seq})",
                    n + 1,
                    c.seq
                );
            }
            let msg = checkpoint_message(stream, c.seq, &c.hash, c.ts.unwrap_or(0.0));
            pk.verify(msg.as_bytes(), &unhex(sig)?)
                .map_err(|_| anyhow!("checkpoint at seq {} has a bad signature", c.seq))?;
            signed = c.seq;
            checkpoints += 1;
            continue;
        }
        let (off, len) = (
            c.off
                .ok_or_else(|| anyhow!("chain line {} has no offset", n + 1))?,
            c.len.unwrap_or(0),
        );
        if c.seq != seq + 1 || off != covered {
            bail!(
                "chain line {} is out of order (seq {}, offset {off}; expected {}, {covered})",
                n + 1,
                c.seq,
                seq + 1
            );
        }
        let end = (off + len) as usize;
        if end > data.len() || data.get(end) != Some(&b'\n') {
            bail!("line {} (offset {off}) is missing or was changed: the file is shorter or different", c.seq);
        }
        hash = link(&hash, &data[off as usize..end]);
        if hex(&hash) != c.hash {
            bail!("line {} (offset {off}) was changed", c.seq);
        }
        seq = c.seq;
        covered = end as u64 + 1;
    }
    if (data.len() as u64) > covered {
        bail!(
            "{} bytes after line {seq} aren't in the chain",
            data.len() as u64 - covered
        );
    }
    Ok(Report {
        lines: seq,
        signed,
        checkpoints,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory of the test's own: tests run in parallel, and two could
    /// start within the same instant.
    fn tmp() -> PathBuf {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let d =
            std::env::temp_dir().join(format!("keeper-test-{}-{}-{n}", std::process::id(), now()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn streams() {
        assert!(valid_stream(
            "firms/acme/runs/20261017-010101-abc/journal.jsonl"
        ));
        assert!(valid_stream("sessions/ab3k9z/shell-1.cast"));
        assert!(!valid_stream("../etc/passwd.jsonl"));
        assert!(!valid_stream("/abs/x.jsonl"));
        assert!(!valid_stream("a//b.jsonl"));
        assert!(!valid_stream("a/.hidden.jsonl"));
        assert!(!valid_stream("a/b.txt"));
        assert!(!valid_stream("keeper.pub"));
    }

    #[test]
    fn append_verify_and_tamper() {
        let d = tmp();
        let key = load_key(&d.join("key"), &d).unwrap();
        let k = Keeper::new(&d, key);
        let pubkey = std::fs::read_to_string(d.join("keeper.pub")).unwrap();
        let s = "firms/acme/runs/r1/journal.jsonl";
        for i in 0..150 {
            assert_eq!(k.append(s, &format!("{{\"seq\":{i}}}")).unwrap(), i + 1);
        }
        assert!(k.append(s, "two\nlines").is_err());
        assert!(k.append("../x.jsonl", "x").is_err());
        k.checkpoint_all();
        let path = d.join(s);
        let r = verify(&path, s, &pubkey).unwrap();
        assert_eq!(r.lines, 150);
        assert_eq!(r.signed, 150);
        assert!(r.checkpoints >= 3);
        // Reopening carries on the chain.
        drop(k);
        let k = Keeper::new(&d, load_key(&d.join("key"), &d).unwrap());
        k.append(s, "after").unwrap();
        k.checkpoint_all();
        assert_eq!(verify(&path, s, &pubkey).unwrap().lines, 151);
        // A changed line, a truncation and an extra line all show.
        let orig = std::fs::read(&path).unwrap();
        let mut changed = orig.clone();
        let at = changed.iter().position(|b| *b == b'7').unwrap();
        changed[at] = b'8';
        std::fs::write(&path, &changed).unwrap();
        assert!(verify(&path, s, &pubkey)
            .unwrap_err()
            .to_string()
            .contains("was changed"));
        std::fs::write(&path, &orig[..orig.len() - 10]).unwrap();
        assert!(verify(&path, s, &pubkey).is_err());
        let mut more = orig.clone();
        more.extend_from_slice(b"forged\n");
        std::fs::write(&path, &more).unwrap();
        assert!(verify(&path, s, &pubkey)
            .unwrap_err()
            .to_string()
            .contains("aren't in the chain"));
        // Another key's checkpoints don't verify.
        std::fs::write(&path, &orig).unwrap();
        let other = tmp();
        let k2 = load_key(&other.join("key"), &other).unwrap();
        assert!(verify(&path, s, &hex(k2.public_key().as_ref()))
            .unwrap_err()
            .to_string()
            .contains("bad signature"));
        std::fs::remove_dir_all(d).unwrap();
        std::fs::remove_dir_all(other).unwrap();
    }

    #[test]
    fn a_full_disk_refuses_appends() {
        let d = tmp();
        let k = Keeper::new(&d, load_key(&d.join("key"), &d).unwrap()).with_min_free(u64::MAX / 2);
        assert!(k
            .append("a/b.jsonl", "x")
            .unwrap_err()
            .to_string()
            .contains("refusing to fill it"));
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn a_crash_between_writes_is_chained_on_reopen() {
        let d = tmp();
        let k = Keeper::new(&d, load_key(&d.join("key"), &d).unwrap());
        let s = "sessions/x/audit.jsonl";
        k.append(s, "one").unwrap();
        drop(k);
        // The data got a line the chain didn't.
        let mut f = OpenOptions::new().append(true).open(d.join(s)).unwrap();
        f.write_all(b"two\n").unwrap();
        let k = Keeper::new(&d, load_key(&d.join("key"), &d).unwrap());
        assert_eq!(k.append(s, "three").unwrap(), 3);
        k.checkpoint_all();
        let pubkey = std::fs::read_to_string(d.join("keeper.pub")).unwrap();
        assert_eq!(verify(&d.join(s), s, &pubkey).unwrap().lines, 3);
        std::fs::remove_dir_all(d).unwrap();
    }
}
