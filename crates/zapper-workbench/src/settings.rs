//! The workbench's own config, `~/.config/zapper-workbench/workbench.yaml`:
//! what is the host's rather than the repository's.
//!
//! ```yaml
//! listen: 127.0.0.1:7880      # where serve listens, and url says
//! daemon:                     # zapper's daemon section, which a repository's config is given
//!   ui: 127.0.0.1:7878
//! ```
//!
//! Without `daemon`, a repository's config takes zapper's running daemon
//! section as it is.

use std::net::SocketAddr;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub listen: Option<SocketAddr>,
    pub daemon: Option<Value>,
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
        assert!(Settings::load(&p).unwrap().listen.is_none(), "none there");
        std::fs::write(&p, "listen: 0.0.0.0:7999\ndaemon: { ui: 127.0.0.1:7878 }\n").unwrap();
        let s = Settings::load(&p).unwrap();
        assert_eq!(s.listen.unwrap().port(), 7999);
        assert_eq!(s.daemon.unwrap()["ui"], "127.0.0.1:7878");
        std::fs::write(&p, "lisen: 1.2.3.4:1\n").unwrap();
        assert!(Settings::load(&p).is_err(), "a misspelling refused");
        let _ = std::fs::remove_dir_all(dir);
    }
}
