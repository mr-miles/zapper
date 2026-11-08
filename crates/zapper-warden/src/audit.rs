//! The warden's record of what it did for the office: one JSON line per
//! control call (a worker made, deleted or started, a refusal), appended to
//! `<state>/warden.jsonl`, private to its user. It is the host's account of
//! the office, apart from the office's own journal, which a compromised
//! office could rewrite.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use serde_json::{json, Value};

pub struct Audit {
    path: Option<PathBuf>,
    /// One line at a time.
    lock: Mutex<()>,
}

impl Audit {
    /// The record in directory `dir` (made private if it isn't there).
    pub fn open(dir: &Path) -> Result<Audit> {
        std::fs::create_dir_all(dir).with_context(|| format!("making {}", dir.display()))?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("making {} private", dir.display()))?;
        Ok(Audit {
            path: Some(dir.join("warden.jsonl")),
            lock: Mutex::new(()),
        })
    }

    /// A record that keeps nothing (the tests').
    pub fn none() -> Audit {
        Audit {
            path: None,
            lock: Mutex::new(()),
        }
    }

    /// Appends `op` with `detail` (an object), and when. A record that
    /// can't be written is logged, not fatal: the warden's work goes on.
    pub fn record(&self, op: &str, detail: Value) {
        let Some(path) = &self.path else {
            return;
        };
        let mut line = json!({ "at": zapper_util::now(), "op": op });
        if let (Some(l), Value::Object(d)) = (line.as_object_mut(), detail) {
            l.extend(d);
        }
        let _one = self.lock.lock().unwrap();
        let written = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)
            .and_then(|mut f| writeln!(f, "{line}"));
        if let Err(e) = written {
            tracing::error!("warden: writing its record {}: {e}", path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_call_is_one_private_line() {
        let dir = std::env::temp_dir().join(format!("zc-audit-{}", std::process::id()));
        let a = Audit::open(&dir).unwrap();
        a.record("worker_made", json!({ "worker": "ada", "profile": "claude" }));
        a.record("refused", json!({ "why": "a\nb" }));
        let text = std::fs::read_to_string(dir.join("warden.jsonl")).unwrap();
        let lines: Vec<Value> = text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(lines.len(), 2, "a newline in a value stays in its line");
        assert_eq!(lines[0]["op"], "worker_made");
        assert_eq!(lines[0]["worker"], "ada");
        assert!(lines[1]["at"].is_number());
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.join("warden.jsonl")).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "private to its user");
        let _ = std::fs::remove_dir_all(dir);
    }
}
