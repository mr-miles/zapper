//! A daemon on a directory of its own, for tests: placeholder agent and
//! Firecracker files, no keeper, no VM. Sessions come from
//! `session::test_session`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::Result;

use super::session::{NewSession, Session};
use super::Daemon;

pub struct TestDaemon {
    pub d: Arc<Daemon>,
    pub dir: PathBuf,
}

impl TestDaemon {
    /// A daemon whose config is `yaml` (profiles, policy, firms...).
    pub fn new(yaml: &str) -> TestDaemon {
        TestDaemon::with_daemon("", yaml)
    }

    /// As `new`, with `daemon` added to the config's `daemon` section
    /// (one `key: value` per line, e.g. `driver: false`).
    pub fn with_daemon(daemon: &str, yaml: &str) -> TestDaemon {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "zapper-daemon-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for f in ["agent", "firecracker"] {
            std::fs::write(dir.join(f), f).unwrap();
        }
        let mut extra = String::new();
        for l in daemon.lines().filter(|l| !l.trim().is_empty()) {
            extra.push_str(&format!("  {}\n", l.trim()));
        }
        let d = dir.display();
        let config = format!(
            "daemon:\n  state: {d}/state\n  agent: {d}/agent\n  firecracker: {d}/firecracker\n  images: {d}/images\n  kernels: {d}/kernels\n  ui: \"\"\n{extra}{yaml}"
        );
        let path = dir.join("zapper.yaml");
        std::fs::write(&path, config).unwrap();
        let d = Daemon::new(path).unwrap();
        TestDaemon { d, dir }
    }

    /// The startup file.
    pub fn startup(&self) -> PathBuf {
        self.dir.join("zapper.yaml")
    }

    /// A working directory for sessions.
    pub fn workdir(&self) -> PathBuf {
        self.dir.clone()
    }

    /// A session registered as `new` describes, without a VM; `running`
    /// marks it running.
    pub fn test_session(&self, new: NewSession, running: bool) -> Result<Arc<Session>> {
        super::session::test_session(&self.d, &new, running)
    }
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
