//! Who may use the workbench, and for what: its tokens. The owner's, with
//! every right, is in `token` (made on first use, mode 0600); others, each
//! limited to sessions with given labels and to a set of rights, are kept
//! in `tokens.json` as their SHA-256 only, the token shown once when made.
//! Both are read on each request, so a token added or removed applies at
//! once.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What a token may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Right {
    /// See its sessions: their info, events, audit, requests, terminals
    /// (read-only), rules and approvals.
    Read,
    /// Create sessions, which get the token's labels.
    Create,
    /// Stop, start and remove its sessions.
    Control,
    /// Open terminals in its sessions.
    Terminal,
    /// Answer its sessions' approvals.
    Answer,
    /// Add and remove rules in its sessions' own layers.
    Rules,
    /// Manage the secrets (their entries in zapper's config and their
    /// values, never shown back), and add rules that use them.
    Secrets,
}

impl Right {
    pub const ALL: [Right; 7] = [
        Right::Read,
        Right::Create,
        Right::Control,
        Right::Terminal,
        Right::Answer,
        Right::Rules,
        Right::Secrets,
    ];

    pub fn parse(s: &str) -> Result<Right> {
        serde_json::from_value(Value::String(s.to_string())).map_err(|_| {
            anyhow::anyhow!(
                "{s:?} isn't a right: read, create, control, terminal, answer, rules, secrets"
            )
        })
    }
}

/// What a request may do: its token's name, the labels its sessions have
/// (all of them; none for every session), and its rights.
#[derive(Debug, Clone, PartialEq)]
pub struct Grant {
    pub name: String,
    pub labels: BTreeMap<String, String>,
    pub rights: BTreeSet<Right>,
}

impl Grant {
    /// The owner's: every right, every session.
    pub fn owner() -> Grant {
        Grant {
            name: "owner".into(),
            labels: BTreeMap::new(),
            rights: Right::ALL.into(),
        }
    }

    pub fn may(&self, right: Right) -> bool {
        self.rights.contains(&right)
    }

    /// Whether it reaches every session, and so what no session's is.
    pub fn reaches_all(&self) -> bool {
        self.labels.is_empty()
    }

    /// Whether it reaches the session whose info is `info`: it has every
    /// label the grant names, with the same value.
    pub fn reaches(&self, info: &Value) -> bool {
        self.labels
            .iter()
            .all(|(k, v)| info["labels"][k].as_str() == Some(v.as_str()))
    }
}

/// A token other than the owner's, as it is kept.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Kept {
    pub name: String,
    /// SHA-256 of the token, in hex.
    pub sha256: String,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    pub rights: BTreeSet<Right>,
    pub created: u64,
}

/// The tokens in a directory: `token`, the owner's, and `tokens.json`.
#[derive(Debug, Clone)]
pub struct Tokens {
    dir: PathBuf,
    /// The owner's token was made just now, by `open`.
    made: bool,
}

fn sha256_hex(s: &str) -> String {
    ring::digest::digest(&ring::digest::SHA256, s.as_bytes())
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A new token: 32 random bytes, in hex.
fn random_token() -> Result<String> {
    let mut b = [0u8; 32];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut b)
        .map_err(|_| anyhow::anyhow!("no randomness"))?;
    Ok(b.iter().map(|b| format!("{b:02x}")).collect())
}

/// Writes `bytes` to `path`, readable by this user only.
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
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

impl Tokens {
    /// The tokens in `dir`, which is made (mode 0700) with the owner's token
    /// if it isn't there.
    pub fn open(dir: PathBuf) -> Result<Tokens> {
        use std::os::unix::fs::DirBuilderExt as _;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .with_context(|| format!("making {}", dir.display()))?;
        let mut t = Tokens { dir, made: false };
        if !t.owner_path().exists() {
            write_private(&t.owner_path(), random_token()?.as_bytes())?;
            t.made = true;
        }
        Ok(t)
    }

    /// Whether the owner's token was made when these were opened: the one
    /// time it is shown without being asked for.
    pub fn owner_is_new(&self) -> bool {
        self.made
    }

    fn owner_path(&self) -> PathBuf {
        self.dir.join("token")
    }

    fn kept_path(&self) -> PathBuf {
        self.dir.join("tokens.json")
    }

    /// The owner's token, for the UI's address.
    pub fn owner(&self) -> Result<String> {
        Ok(std::fs::read_to_string(self.owner_path())?
            .trim()
            .to_string())
    }

    /// The tokens other than the owner's.
    pub fn list(&self) -> Result<Vec<Kept>> {
        match std::fs::read(self.kept_path()) {
            Ok(b) => Ok(serde_json::from_slice(&b).context("tokens.json")?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(vec![]),
            Err(e) => Err(e.into()),
        }
    }

    /// What `offered` may do, if it is a token.
    pub fn grant(&self, offered: &str) -> Option<Grant> {
        if offered.is_empty() {
            return None;
        }
        let hash = sha256_hex(offered);
        if self.owner().ok().is_some_and(|o| sha256_hex(&o) == hash) {
            return Some(Grant::owner());
        }
        self.list()
            .ok()?
            .into_iter()
            .find(|k| k.sha256 == hash)
            .map(|k| Grant {
                name: k.name,
                labels: k.labels,
                rights: k.rights,
            })
    }

    /// Adds a token named `name`, for sessions with `labels`, with `rights`;
    /// the token, which is kept only as its hash.
    pub fn add(
        &self,
        name: &str,
        labels: BTreeMap<String, String>,
        rights: BTreeSet<Right>,
    ) -> Result<String> {
        if name.is_empty()
            || name.len() > 64
            || name == "owner"
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        {
            bail!("a token's name: 1 to 64 letters, digits or -_. (and not owner)");
        }
        if rights.is_empty() {
            bail!("a token needs a right");
        }
        let mut kept = self.list()?;
        if kept.iter().any(|k| k.name == name) {
            bail!("there is a token {name} already");
        }
        let token = random_token()?;
        kept.push(Kept {
            name: name.to_string(),
            sha256: sha256_hex(&token),
            labels,
            rights,
            created: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or_default(),
        });
        write_private(&self.kept_path(), &serde_json::to_vec_pretty(&kept)?)?;
        Ok(token)
    }

    /// Removes the token named `name`; whether there was one.
    pub fn remove(&self, name: &str) -> Result<bool> {
        let mut kept = self.list()?;
        let before = kept.len();
        kept.retain(|k| k.name != name);
        if kept.len() == before {
            return Ok(false);
        }
        write_private(&self.kept_path(), &serde_json::to_vec_pretty(&kept)?)?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("zapper-wb-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn the_owners_token_is_made_once_private_and_grants_everything() {
        let d = dir("owner");
        let t = Tokens::open(d.clone()).unwrap();
        let owner = t.owner().unwrap();
        assert_eq!(owner.len(), 64);
        assert!(t.owner_is_new());
        let again = Tokens::open(d.clone()).unwrap();
        assert_eq!(again.owner().unwrap(), owner);
        assert!(!again.owner_is_new());
        use std::os::unix::fs::PermissionsExt as _;
        let mode = |p: PathBuf| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(d.join("token")), 0o600);
        assert_eq!(mode(d.clone()), 0o700);
        assert_eq!(t.grant(&owner), Some(Grant::owner()));
        assert_eq!(t.grant("nope"), None);
        assert_eq!(t.grant(""), None);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_token_is_kept_as_its_hash_with_its_labels_and_rights() {
        let d = dir("kept");
        let t = Tokens::open(d.clone()).unwrap();
        let labels: BTreeMap<_, _> = [("project".to_string(), "x".to_string())].into();
        let token = t
            .add("laptop", labels.clone(), [Right::Read].into())
            .unwrap();
        let file = std::fs::read_to_string(d.join("tokens.json")).unwrap();
        assert!(!file.contains(&token), "only its hash is kept");
        let g = t.grant(&token).unwrap();
        assert_eq!((g.name.as_str(), &g.labels), ("laptop", &labels));
        assert!(g.may(Right::Read) && !g.may(Right::Answer));
        assert!(
            t.add("laptop", BTreeMap::new(), [Right::Read].into())
                .is_err(),
            "names are unique"
        );
        assert!(t
            .add("owner", BTreeMap::new(), [Right::Read].into())
            .is_err());
        assert!(
            t.add("x", BTreeMap::new(), BTreeSet::new()).is_err(),
            "a right at least"
        );
        assert!(t.remove("laptop").unwrap());
        assert_eq!(t.grant(&token), None, "removed, refused at once");
        assert!(!t.remove("laptop").unwrap());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_grant_reaches_the_sessions_with_all_its_labels() {
        let g = Grant {
            name: "t".into(),
            labels: [("project".into(), "x".into()), ("team".into(), "a".into())].into(),
            rights: [Right::Read].into(),
        };
        assert!(g.reaches(&json!({ "labels": { "project": "x", "team": "a", "more": "1" } })));
        assert!(!g.reaches(&json!({ "labels": { "project": "x" } })));
        assert!(!g.reaches(&json!({ "labels": { "project": "y", "team": "a" } })));
        assert!(!g.reaches(&json!({ "id": "abc" })));
        assert!(!g.reaches_all());
        assert!(Grant::owner().reaches(&json!({ "id": "abc" })));
    }
}
