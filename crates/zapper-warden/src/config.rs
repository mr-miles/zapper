//! The warden's config, `~/.config/zapperclaw/warden.yaml` (`--config` for
//! another; [config/warden.yaml](../../../config/warden.yaml) is the
//! checkout's). Relative paths in it resolve against its own directory.
//!
//! ```yaml
//! openshell: ~/.config/zapper/openshell.yaml  # the gateway, its TLS, the workers' edge, the driver's identity
//! zapper: ~/.config/zapper/zapper.yaml        # the profiles and image recipes named below
//! listen: 127.0.0.1:8972                      # the routes, for the office sandbox's supervisor
//! url: http://host.openshell.internal:8972    # the routes as the office sandbox reaches them
//! board: 127.0.0.1:8974                       # the board's forward into the office sandbox (8099 is zapper-office's own)
//! state: ~/.local/state/zapperclaw            # the warden's audit log
//! max_workers: 8                              # workers at once, of every profile
//! office:
//!   profile: zapperclaw-office                # the office sandbox's zapper profile
//!   providers: [warden]                       # its gateway providers: the warden's routes, nothing else
//!   ports: { edge: 8970, board: 8099, agent: 8973 }   # where it listens, in its sandbox
//! profiles:                                   # the worker profiles the office may ask for
//!   claude:
//!     providers: [anthropic, office-edge]     # the default
//!     max: 4                                  # workers of it at once (default: max_workers)
//!     proposals: true                         # workers may propose rules (policy.local); a person decides them
//!     instructions: agents/claude.md          # optional: the profile's standing instructions
//!     skills: skills/claude                   # optional: a directory of skills
//!     model: claude-opus-5-5                  # optional
//! ```
//!
//! Everything but `office.profile` has a default. A change takes effect when
//! the warden starts; `apply` remakes the templates from it. Sandboxes keep
//! what they were made with: a change to the office's providers, ports or
//! profile reaches the office sandbox only once it is deleted and the
//! warden makes it again, and a worker's once the office deletes and makes
//! it again.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::names;

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub openshell: String,
    pub zapper: String,
    pub listen: SocketAddr,
    pub url: String,
    pub board: SocketAddr,
    pub state: String,
    pub max_workers: usize,
    pub office: Office,
    pub profiles: BTreeMap<String, WorkerProfile>,
    /// The directory paths resolve against: the config file's.
    #[serde(skip)]
    pub dir: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Office {
    pub profile: String,
    pub providers: Vec<String>,
    pub ports: Ports,
}

/// Where the office listens in its sandbox, for the warden's forwards.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Ports {
    /// The workers' edge (their tools, drive, Stop hook and token grant).
    pub edge: u16,
    /// The board's dashboard and CLI.
    pub board: u16,
    /// Workers' agent streams, each opened with its ticket.
    pub agent: u16,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WorkerProfile {
    pub providers: Vec<String>,
    pub max: Option<usize>,
    pub proposals: bool,
    pub instructions: Option<String>,
    pub skills: Option<String>,
    pub model: Option<String>,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            openshell: "~/.config/zapper/openshell.yaml".into(),
            zapper: "~/.config/zapper/zapper.yaml".into(),
            listen: "127.0.0.1:8972".parse().unwrap(),
            url: "http://host.openshell.internal:8972".into(),
            board: "127.0.0.1:8974".parse().unwrap(),
            state: "~/.local/state/zapperclaw".into(),
            max_workers: 8,
            office: Office::default(),
            profiles: BTreeMap::new(),
            dir: PathBuf::from("."),
        }
    }
}

impl Default for Office {
    fn default() -> Office {
        Office {
            profile: String::new(),
            providers: vec!["warden".into()],
            ports: Ports::default(),
        }
    }
}

impl Default for Ports {
    fn default() -> Ports {
        Ports {
            edge: 8970,
            board: 8099,
            agent: 8973,
        }
    }
}

impl Default for WorkerProfile {
    fn default() -> WorkerProfile {
        WorkerProfile {
            providers: vec!["anthropic".into(), "office-edge".into()],
            max: None,
            proposals: true,
            instructions: None,
            skills: None,
            model: None,
        }
    }
}

impl Config {
    /// The config in `text`, its relative paths against `dir`; checked as a
    /// whole.
    pub fn from_yaml(text: &str, dir: &Path) -> Result<Config> {
        let mut c: Config = if text.trim().is_empty() {
            Config::default()
        } else {
            serde_yaml::from_str(text)?
        };
        c.dir = dir.to_path_buf();
        c.check()?;
        Ok(c)
    }

    /// The config at `path`.
    pub fn load(path: &Path) -> Result<Config> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        let dir = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        Config::from_yaml(&text, &dir).with_context(|| format!("loading {}", path.display()))
    }

    fn check(&self) -> Result<()> {
        if self.office.profile.trim().is_empty() {
            bail!("office.profile: name the zapper profile the office sandbox is made from");
        }
        if self.office.providers.is_empty() {
            bail!("office.providers: the office reaches the warden through its provider (warden)");
        }
        let p = self.office.ports;
        if p.edge == p.board || p.edge == p.agent || p.board == p.agent {
            bail!("office.ports: edge, board and agent are three different ports");
        }
        if self.max_workers == 0 {
            bail!("max_workers: at least 1");
        }
        if self.url.trim().is_empty() {
            bail!("url: where the office sandbox reaches the warden");
        }
        for (name, w) in &self.profiles {
            names::check("profile", name)?;
            if w.max == Some(0) {
                bail!("profiles.{name}.max: at least 1, or leave the profile out");
            }
        }
        if self.listen == self.board {
            bail!("listen and board are two different addresses");
        }
        // The token endpoint is plain HTTP, which the supervisor dials only
        // on loopback, and the board is a person's: neither leaves the host.
        if !self.listen.ip().is_loopback() {
            bail!("listen: a loopback address (the supervisors reach it as host.openshell.internal)");
        }
        if !self.board.ip().is_loopback() {
            bail!("board: a loopback address (reach it from elsewhere with an SSH tunnel)");
        }
        Ok(())
    }

    /// `p` as a path: `~/` is the home, a relative path is the config's
    /// directory's.
    pub fn path(&self, p: &str) -> PathBuf {
        let expanded = if let Some(rest) = p.strip_prefix("~/") {
            crate::home(rest)
        } else {
            PathBuf::from(p)
        };
        if expanded.is_absolute() {
            expanded
        } else {
            self.dir.join(expanded)
        }
    }

    /// The most workers of `profile` at once.
    pub fn max_of(&self, profile: &str) -> usize {
        self.profiles
            .get(profile)
            .and_then(|w| w.max)
            .unwrap_or(self.max_workers)
    }

    /// The warden's token endpoint, as the office's provider profile and its
    /// supervisor's SVID name it.
    pub fn token_url(&self) -> String {
        format!("http://{}/oauth/token", self.listen)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(text: &str) -> Result<Config> {
        Config::from_yaml(text, Path::new("/etc/zc"))
    }

    #[test]
    fn only_the_office_profile_must_be_given() {
        assert!(load("").is_err(), "no office profile");
        let c = load("office: { profile: zapperclaw-office }\n").unwrap();
        assert_eq!(c.listen.port(), 8972);
        assert_eq!(c.office.providers, ["warden"]);
        assert_eq!(c.office.ports.agent, 8973);
        assert_eq!(c.max_workers, 8);
        assert_eq!(c.token_url(), "http://127.0.0.1:8972/oauth/token");
    }

    #[test]
    fn worker_profiles_have_defaults_and_their_own_caps() {
        let c = load(
            "office: { profile: o }\nmax_workers: 5\nprofiles:\n  claude: { max: 2 }\n  pi: {}\n",
        )
        .unwrap();
        assert_eq!(c.profiles["pi"].providers, ["anthropic", "office-edge"]);
        assert!(c.profiles["pi"].proposals);
        assert_eq!(c.max_of("claude"), 2);
        assert_eq!(c.max_of("pi"), 5);
    }

    #[test]
    fn what_cant_work_is_refused_with_why() {
        for (bad, why) in [
            ("office: { profile: o }\nagnet: 1\n", "a misspelling"),
            ("office: { profile: o, ports: { edge: 1, board: 1 } }\n", "ports shared"),
            ("office: { profile: o }\nprofiles: { Claude: {} }\n", "a profile name"),
            ("office: { profile: o }\nprofiles: { c: { max: 0 } }\n", "max 0"),
            ("office: { profile: o, providers: [] }\n", "no providers"),
            ("office: { profile: o }\nlisten: 0.0.0.0:8972\n", "routes off loopback"),
            ("office: { profile: o }\nboard: 192.168.1.2:8974\n", "board off loopback"),
            ("office: { profile: o }\nmax_workers: 0\n", "no workers"),
            ("office: { profile: o }\nboard: 127.0.0.1:8972\n", "board on listen"),
        ] {
            assert!(load(bad).is_err(), "{why}");
        }
    }

    #[test]
    fn paths_are_the_configs_or_the_homes() {
        let c = load("office: { profile: o }\n").unwrap();
        assert_eq!(c.path("zapper.yaml"), PathBuf::from("/etc/zc/zapper.yaml"));
        assert_eq!(c.path("/abs/x"), PathBuf::from("/abs/x"));
        assert!(c.path("~/x").ends_with("x") && c.path("~/x").is_absolute());
    }
}
