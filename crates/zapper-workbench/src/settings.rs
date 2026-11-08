//! The workbench's own config, `~/.config/zapper-workbench/workbench.yaml`:
//! what is the host's rather than the repository's.
//!
//! ```yaml
//! openshell: ~/.config/zapper/openshell.yaml   # the gateway (default: beside the zapper config)
//! workspace: default                           # the gateway workspace (default: openshell.yaml's, else `default`)
//! providers: [anthropic, workbench-edge]       # attached to the sandboxes `new` makes (the default)
//! edge:                                        # where sandboxes reach the drive and repository (each optional)
//!   listen: 127.0.0.1:8971
//!   url: http://host.openshell.internal:8971
//!   token_url: http://127.0.0.1:8971/oauth/token
//! ```
//!
//! The edge's other settings (the driver's JWKS, issuer and trust domain)
//! are `openshell.yaml`'s, as the office's edge has them.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;
use zapper_openshell::EdgeSettings;

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// The OpenShell settings (the gateway, its TLS, images, providers).
    pub openshell: Option<PathBuf>,
    /// The gateway workspace its templates and sandboxes are in, over
    /// `openshell.yaml`'s.
    pub workspace: Option<String>,
    /// The gateway providers `zapper-workbench new` attaches; by default
    /// `anthropic` and `workbench-edge`.
    pub providers: Option<Vec<String>>,
    /// The workbench's edge, on a port of its own (8971).
    pub edge: Edge,
}

/// Where the workbench's edge is, each by default beside the office's on
/// port 8971.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Edge {
    pub listen: Option<SocketAddr>,
    pub url: Option<String>,
    pub token_url: Option<String>,
}

impl Settings {
    /// The settings in `path`; none there, the defaults.
    pub fn load(path: &Path) -> Result<Settings> {
        match std::fs::read_to_string(path) {
            Ok(t) if t.trim().is_empty() => Ok(Settings::default()),
            Ok(t) => serde_yaml::from_str(&t).with_context(|| format!("{}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Where the edge listens.
    pub fn listen(&self) -> SocketAddr {
        self.edge
            .listen
            .unwrap_or_else(|| "127.0.0.1:8971".parse().unwrap())
    }

    /// The workbench's edge: `openshell.yaml`'s (`base`) at the workbench's
    /// addresses.
    pub fn edge(&self, base: EdgeSettings) -> EdgeSettings {
        EdgeSettings {
            listen: self.listen(),
            url: self
                .edge
                .url
                .clone()
                .unwrap_or_else(|| "http://host.openshell.internal:8971".into()),
            token_url: self
                .edge
                .token_url
                .clone()
                .unwrap_or_else(|| "http://127.0.0.1:8971/oauth/token".into()),
            ..base
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_are_optional_and_strict_and_keep_the_drivers_identity() {
        let dir = std::env::temp_dir().join(format!("zapper-wb-settings-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("workbench.yaml");
        let s = Settings::load(&p).unwrap();
        assert_eq!(s.listen().port(), 8971, "none there: the defaults");
        std::fs::write(&p, "edge: { url: http://h:9 }\n").unwrap();
        let s = Settings::load(&p).unwrap();
        let base = EdgeSettings {
            trust_domain: "example.org".into(),
            ..EdgeSettings::default()
        };
        let e = s.edge(base);
        assert_eq!(e.url, "http://h:9");
        assert_eq!(e.listen.port(), 8971, "the others keep their defaults");
        assert_eq!(e.trust_domain, "example.org", "openshell.yaml's identity");
        std::fs::write(&p, "agnet: /x\n").unwrap();
        assert!(Settings::load(&p).is_err(), "a misspelling refused");
        let _ = std::fs::remove_dir_all(dir);
    }
}
