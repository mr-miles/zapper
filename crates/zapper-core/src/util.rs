//! Helpers the core's clients share with it: ids and time, files only the
//! user reads, a config with its inline secrets hidden, the terminal
//! channel's frames, the warning for listening beyond loopback, and the
//! host's memory.

use std::path::Path;

use anyhow::{Context, Result};

pub use crate::daemon::redact;
pub use crate::daemon::session::read_frame;
// Generic host helpers used across zapper's crates (the CLI, the API, the
// core, the driver and The Office); they live in `zapper-util`, at the
// bottom of the graph, so an API client needs no `zapper-core`. Re-exported
// here so the core's own callers are unchanged.
pub use zapper_util::{exposed_warning, host_memory_mib, now, random_id, write_private, ID_CHARS};

/// Finds the executable `name` in `dirs`, in order: the first directory that
/// holds it as a file. Used to locate system tools (`ip`, `nft`, `newuidmap`,
/// …) without trusting `PATH`.
pub(crate) fn find_exe(name: &str, dirs: &[&str]) -> Option<std::path::PathBuf> {
    dirs.iter()
        .map(|d| Path::new(d).join(name))
        .find(|p| p.is_file())
}

/// A file's SHA-256, in hex.
pub(crate) fn sha256_file(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).with_context(|| format!("hashing {}", path.display()))?;
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        ctx.update(&buf[..n]);
    }
    Ok(ctx
        .finish()
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// Whether `s` has the form of a session id, as `random_id(6)` makes them.
pub(crate) fn is_session_id(s: &str) -> bool {
    s.len() == 6 && s.bytes().all(|b| ID_CHARS.contains(&b))
}
