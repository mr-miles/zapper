//! The workbench's own config, `~/.config/zapper-workbench/workbench.yaml`:
//! what is the host's rather than the repository's.
//!
//! ```yaml
//! openshell: ~/.config/zapper/openshell.yaml   # the gateway (default: beside the zapper config)
//! workspace: default                           # the gateway workspace (default: openshell.yaml's, else `default`)
//! providers: [anthropic, workbench-edge]       # attached to the sandboxes `new` makes (the default)
//! agent: ~/.local/bin/zapper-agent             # the guest `office` command the images carry
//! edge:                                        # where sandboxes reach the drive and repository
//!   listen: 127.0.0.1:8971
//!   url: http://host.openshell.internal:8971
//!   token_url: http://127.0.0.1:8971/oauth/token
//! ```

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;
use zapper_openshell::EdgeSettings;

#[derive(Debug, Deserialize)]
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
    /// The static guest binary the base images install as `office`.
    pub agent: Option<PathBuf>,
    /// The workbench's edge: as the office's, on a port of its own.
    pub edge: EdgeSettings,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            openshell: None,
            workspace: None,
            providers: None,
            agent: None,
            edge: edge_defaults(),
        }
    }
}

/// The office edge's defaults, on port 8971: the workbench beside the office
/// on one host.
pub fn edge_defaults() -> EdgeSettings {
    EdgeSettings {
        listen: "127.0.0.1:8971".parse().unwrap(),
        url: "http://host.openshell.internal:8971".into(),
        token_url: "http://127.0.0.1:8971/oauth/token".into(),
        ..EdgeSettings::default()
    }
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_are_optional_and_strict() {
        let dir = std::env::temp_dir().join(format!("zapper-wb-settings-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("workbench.yaml");
        let s = Settings::load(&p).unwrap();
        assert_eq!(s.edge.listen.port(), 8971, "none there: the defaults");
        std::fs::write(&p, "agent: /opt/zapper-agent\n").unwrap();
        let s = Settings::load(&p).unwrap();
        assert_eq!(s.agent.unwrap(), Path::new("/opt/zapper-agent"));
        assert_eq!(s.edge.listen.port(), 8971, "the edge's defaults kept");
        std::fs::write(&p, "agnet: /x\n").unwrap();
        assert!(Settings::load(&p).is_err(), "a misspelling refused");
        let _ = std::fs::remove_dir_all(dir);
    }
}
