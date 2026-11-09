//! `zapperclaw.yaml`, the office's own config inside its sandbox: the
//! roster, how many turns run at once and for how long, and where the
//! state is kept. What the warden set when it made the office's sandbox
//! (its URL, the ports its forwards reach, the board's token) comes from the
//! environment instead, as the office can't change it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;

/// A worker on the roster.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Member {
    /// One of the warden's worker profiles.
    pub profile: String,
    /// Their instructions: a file, relative to the config's directory.
    #[serde(default)]
    pub instructions: Option<String>,
    /// What the directory says they do.
    #[serde(default)]
    pub about: Option<String>,
}

/// How the office's edge checks the workers' supervisors.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct EdgeConfig {
    /// The edge's URL as the workers use it.
    pub url: String,
    /// The token endpoint as the `office-edge` provider profile names it.
    pub token_url: String,
    /// The SVIDs' issuer.
    pub issuer: String,
    /// The SPIFFE trust domain of the sandboxes' IDs.
    pub trust_domain: String,
}

impl Default for EdgeConfig {
    fn default() -> Self {
        let e = zapper_openshell::EdgeSettings::default();
        EdgeConfig {
            url: e.url,
            token_url: e.token_url,
            issuer: e.issuer,
            trust_domain: e.trust_domain,
        }
    }
}

/// Where the post office keeps its state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StoreMode {
    #[default]
    Both,
    Database,
    Journal,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub workers: BTreeMap<String, Member>,
    /// Turns that run at once.
    pub max_concurrent: usize,
    /// How long a turn may take (`30m`, `90s`, `2h`).
    pub turn_timeout: String,
    /// Where the journal, transcripts and drive are kept.
    pub state: PathBuf,
    /// Relays' address namespaces (`wa`, `tg`, `cron`): mail to them waits
    /// for their relay.
    pub relays: Vec<String>,
    pub edge: EdgeConfig,
    /// Where the post office keeps its state: `both` (its events journaled
    /// and a database, `office.db`, made from them; the default),
    /// `database` (no events in the journal), or `journal` (memory, made
    /// again from the journal at every start).
    pub store: StoreMode,
    /// The config's directory, for relative paths.
    #[serde(skip)]
    pub dir: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            workers: BTreeMap::new(),
            max_concurrent: 2,
            turn_timeout: "30m".into(),
            state: PathBuf::from("~/.local/state/zapperclaw"),
            relays: vec![],
            store: StoreMode::default(),
            edge: EdgeConfig::default(),
            dir: PathBuf::from("."),
        }
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Where the office's config is, unless `--config` says.
pub fn default_path() -> PathBuf {
    home().join(".config/zapperclaw/zapperclaw.yaml")
}

impl Config {
    pub fn from_yaml(text: &str, dir: &Path) -> Result<Config> {
        let mut c: Config = serde_yaml::from_str(text)?;
        c.dir = dir.to_path_buf();
        c.check()?;
        Ok(c)
    }

    pub fn load(path: &Path) -> Result<Config> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let dir = path.parent().unwrap_or(Path::new("."));
        Config::from_yaml(&text, dir).with_context(|| format!("{}", path.display()))
    }

    /// Everything that can't work, refused with why.
    pub fn check(&self) -> Result<()> {
        if self.workers.is_empty() {
            bail!("workers: the roster is empty");
        }
        for name in self.workers.keys() {
            check_name(name).with_context(|| format!("workers.{name}"))?;
        }
        for ns in &self.relays {
            let ok = !ns.is_empty()
                && ns.len() <= 16
                && ns
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
            if !ok {
                bail!("relays: {ns:?} isn't a namespace (a short lowercase word)");
            }
        }
        if self.max_concurrent == 0 {
            bail!("max_concurrent: at least 1");
        }
        self.turn_timeout()?;
        Ok(())
    }

    pub fn turn_timeout(&self) -> Result<Duration> {
        parse_duration(&self.turn_timeout).context("turn_timeout")
    }

    /// `p`, from `~/` or relative to the config's directory.
    pub fn path(&self, p: impl AsRef<Path>) -> PathBuf {
        let p = p.as_ref();
        match p.strip_prefix("~") {
            Ok(rest) => home().join(rest),
            Err(_) if p.is_relative() => self.dir.join(p),
            Err(_) => p.to_path_buf(),
        }
    }

    /// Worker `name`'s instructions, if the roster gives a file.
    pub fn instructions(&self, name: &str) -> Result<Option<String>> {
        let Some(f) = self.workers.get(name).and_then(|m| m.instructions.as_ref()) else {
            return Ok(None);
        };
        let p = self.path(f);
        std::fs::read_to_string(&p)
            .map(Some)
            .with_context(|| format!("{name}'s instructions {}", p.display()))
    }

    /// What the directory says each worker does.
    pub fn about(&self) -> BTreeMap<String, String> {
        self.workers
            .iter()
            .filter_map(|(n, m)| m.about.clone().map(|a| (n.clone(), a)))
            .collect()
    }
}

/// A worker's name: what the warden takes (a lowercase letter, then
/// lowercase letters, digits and single hyphens, up to 40), and not one of
/// the post office's own addresses.
pub fn check_name(name: &str) -> Result<()> {
    let ok = name.len() <= 40
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !name.ends_with('-')
        && !name.contains("--");
    if !ok {
        bail!("{name:?} isn't a worker's name (a lowercase letter, then lowercase letters, digits and single hyphens, up to 40)");
    }
    if [crate::mail::BOARD, crate::mail::OFFICE].contains(&name) {
        bail!("{name} is the post office's own address");
    }
    Ok(())
}

/// `30m`, `90s`, `2h`, `1h30m`.
pub fn parse_duration(s: &str) -> Result<Duration> {
    let mut total = 0u64;
    let mut n = String::new();
    for c in s.trim().chars() {
        if c.is_ascii_digit() {
            n.push(c);
            continue;
        }
        let unit = match c {
            's' => 1,
            'm' => 60,
            'h' => 3600,
            _ => bail!("{s:?}: a duration is like 90s, 30m or 2h"),
        };
        let v: u64 = n
            .parse()
            .map_err(|_| anyhow!("{s:?}: a duration is like 90s, 30m or 2h"))?;
        total += v * unit;
        n.clear();
    }
    if !n.is_empty() || total == 0 {
        bail!("{s:?}: a duration is like 90s, 30m or 2h");
    }
    Ok(Duration::from_secs(total))
}

/// What the warden set in the office sandbox's environment.
#[derive(Debug, Clone, PartialEq)]
pub struct Env {
    /// The warden's URL (`ZAPPERCLAW_WARDEN_URL`).
    pub warden: String,
    /// The ports the warden's forwards reach.
    pub edge_port: u16,
    pub board_port: u16,
    pub agent_port: u16,
    /// The board's token (`ZAPPERCLAW_BOARD_TOKEN`), if the warden gave one.
    pub board_token: Option<String>,
}

impl Env {
    pub fn from(get: impl Fn(&str) -> Option<String>) -> Result<Env> {
        let var = |k: &str| get(k).filter(|v| !v.trim().is_empty());
        let port = |k: &str, default: u16| -> Result<u16> {
            match var(k) {
                Some(v) => v.trim().parse().with_context(|| format!("{k}={v}")),
                None => Ok(default),
            }
        };
        Ok(Env {
            warden: var("ZAPPERCLAW_WARDEN_URL").ok_or_else(|| {
                anyhow!("ZAPPERCLAW_WARDEN_URL isn't set: is this the office's sandbox?")
            })?,
            edge_port: port("ZAPPERCLAW_EDGE_PORT", 8970)?,
            board_port: port("ZAPPERCLAW_BOARD_PORT", 8099)?,
            agent_port: port("ZAPPERCLAW_AGENT_PORT", 8973)?,
            board_token: var("ZAPPERCLAW_BOARD_TOKEN"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROSTER: &str = "
workers:
  research: { profile: claude, instructions: research.md, about: finds things out }
  ops: { profile: claude }
max_concurrent: 1
turn_timeout: 10m
relays: [wa]
";

    #[test]
    fn a_roster_loads_with_its_defaults() {
        let c = Config::from_yaml(ROSTER, Path::new("/etc/zc")).unwrap();
        assert_eq!(c.workers.len(), 2);
        assert_eq!(c.turn_timeout().unwrap(), Duration::from_secs(600));
        assert_eq!(c.path("research.md"), PathBuf::from("/etc/zc/research.md"));
        assert_eq!(c.about()["research"], "finds things out");
        assert_eq!(c.edge, EdgeConfig::default());
        assert_eq!(c.edge.token_url, "http://127.0.0.1:8970/oauth/token");
    }

    #[test]
    fn what_cant_work_is_refused() {
        for bad in [
            "workers: {}",
            "workers: { Ada: { profile: claude } }",
            "workers: { board: { profile: claude } }",
            "workers: { a: { profile: claude } }\nmax_concurrent: 0",
            "workers: { a: { profile: claude } }\nturn_timeout: soon",
            "workers: { a: { profile: claude } }\nrelays: ['w:a']",
            "workers: { a: { profile: claude, image: x } }",
        ] {
            assert!(Config::from_yaml(bad, Path::new(".")).is_err(), "{bad}");
        }
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("1h30m").unwrap(), Duration::from_secs(5400));
        assert_eq!(parse_duration("90s").unwrap(), Duration::from_secs(90));
        for bad in ["", "10", "m", "1d", "0s"] {
            assert!(parse_duration(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_environment_is_the_wardens() {
        let vars = |k: &str| match k {
            "ZAPPERCLAW_WARDEN_URL" => Some("http://host.openshell.internal:8972".to_string()),
            "ZAPPERCLAW_AGENT_PORT" => Some("9000".to_string()),
            _ => None,
        };
        let e = Env::from(vars).unwrap();
        assert_eq!(e.agent_port, 9000);
        assert_eq!(e.edge_port, 8970);
        assert_eq!(e.board_token, None);
        assert!(Env::from(|_| None).is_err());
    }
}

#[cfg(test)]
mod checkout_tests {
    use super::*;

    #[test]
    fn the_checkouts_roster_loads_with_its_instructions() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/zapperclaw");
        let c = Config::load(&dir.join("zapperclaw.yaml")).unwrap();
        assert_eq!(c.workers["echo"].profile, "zapperclaw-echo");
        assert!(c.instructions("echo").unwrap().unwrap().contains("Answer"));
    }
}
