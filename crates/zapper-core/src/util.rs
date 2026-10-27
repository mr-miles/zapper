//! Helpers the core's clients share with it: ids and time, files only the
//! user reads, a config with its inline secrets hidden, the terminal
//! channel's frames, and the warning for listening beyond loopback.

use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;

pub use crate::daemon::redact;
pub use crate::daemon::session::read_frame;

/// The alphabet session and worker ids are drawn from: lowercase letters and
/// digits, without the ambiguous `l`, `o`, `0` and `1`.
pub const ID_CHARS: &[u8] = b"abcdefghijkmnpqrstuvwxyz23456789";

/// Unix time in seconds.
pub fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

/// Whether `s` has the form of a session id, as `random_id(6)` makes them.
pub fn is_session_id(s: &str) -> bool {
    s.len() == 6 && s.bytes().all(|b| ID_CHARS.contains(&b))
}

pub fn random_id(len: usize) -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    (0..len)
        .map(|_| ID_CHARS[rng.gen_range(0..ID_CHARS.len())] as char)
        .collect()
}

/// Writes `data` to `path` atomically with mode 0600.
pub fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// What `zapper serve` prints when an operator's port is reachable from
/// the network: the UI (`daemon.ui`) or a firm's Kubernetes dashboard.
pub fn exposed_warning(what: &str) -> String {
    format!(
        "\n\
         ################################################################\n\
         WARNING: {what}, reachable from the network.\n\
         Whoever has the token controls zapper: sessions, rules, approvals,\n\
         firms, and the firm's cluster as cluster-admin. It is plain HTTP:\n\
         use it on a network you trust, or reach it through an SSH tunnel\n\
         or a TLS proxy. zapper is not built for production. The VMs still\n\
         can't reach it: the host's own addresses are closed to them.\n\
         ################################################################\n"
    )
}
