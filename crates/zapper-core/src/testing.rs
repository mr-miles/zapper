//! A `Core` on a directory of its own, for tests: placeholder agent and
//! Firecracker files, no keeper, no VM. Sessions come from
//! `Core::test_session`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::{Core, Session};

pub struct TestCore {
    pub core: Core,
    pub dir: PathBuf,
}

impl TestCore {
    /// A core whose config is `yaml` (profiles, policy, firms...).
    pub fn new(yaml: &str) -> TestCore {
        TestCore::with_daemon("", yaml)
    }

    /// As `new`, with `daemon` added to the config's `daemon` section
    /// (one `key: value` per line, e.g. `driver: false`).
    pub fn with_daemon(daemon: &str, yaml: &str) -> TestCore {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "zapper-core-test-{}-{}",
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
        let core = Core::start(path).unwrap();
        TestCore { core, dir }
    }

    /// The startup file.
    pub fn startup(&self) -> PathBuf {
        self.dir.join("zapper.yaml")
    }

    /// A working directory for sessions.
    pub fn workdir(&self) -> PathBuf {
        self.dir.clone()
    }

    /// A running session of `firm`'s worker `member`, of no run; none
    /// without the office.
    pub fn firm_worker(&self, profile: &str, firm: &str, member: &str) -> Option<Arc<Session>> {
        #[cfg(feature = "office")]
        {
            let mut new =
                crate::NewSession::plain(profile.into(), self.workdir(), Default::default());
            new.firm = Some(crate::daemon::office::FirmLink {
                firm: firm.into(),
                member: member.into(),
                worker: true,
                run: std::sync::Weak::new(),
                role: None,
            });
            Some(self.core.test_session(new, true).unwrap())
        }
        #[cfg(not(feature = "office"))]
        {
            let _ = (profile, firm, member);
            None
        }
    }
}

impl Drop for TestCore {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
