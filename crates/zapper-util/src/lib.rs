//! Generic host helpers shared across zapper's crates: ids and time, files
//! only the user reads, the host's memory, and the warning for listening
//! beyond loopback. Nothing here is zapper-specific or depends on another
//! zapper crate, so it sits at the bottom of the graph (a client of the API,
//! such as The Office as its own process, uses it without linking the core).

use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;

/// The alphabet session and worker ids are drawn from: lowercase letters and
/// digits, without the ambiguous `l`, `o`, `0` and `1`.
pub const ID_CHARS: &[u8] = b"abcdefghijkmnpqrstuvwxyz23456789";

/// A field of the host's `/proc/meminfo` (`MemTotal`, `MemAvailable`, …),
/// in MiB.
pub fn host_memory_mib(field: &str) -> Option<u64> {
    meminfo_mib(&std::fs::read_to_string("/proc/meminfo").ok()?, field)
}

fn meminfo_mib(info: &str, field: &str) -> Option<u64> {
    info.lines()
        .find_map(|l| {
            l.strip_prefix(field)?
                .strip_prefix(':')?
                .trim()
                .strip_suffix("kB")?
                .trim()
                .parse::<u64>()
                .ok()
        })
        .map(|k| k / 1024)
}

/// Unix time in seconds.
pub fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

/// A random id `len` characters long, drawn from [`ID_CHARS`].
pub fn random_id(len: usize) -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    (0..len)
        .map(|_| ID_CHARS[rng.gen_range(0..ID_CHARS.len())] as char)
        .collect()
}

/// Writes `data` to `path` atomically with mode 0600. The temporary file is
/// this write's own, so writes at once don't mix.
pub fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_extension(format!("tmp-{}", random_id(8)));
    let written = (|| -> Result<()> {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
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

#[cfg(test)]
mod tests {
    #[test]
    fn meminfo_fields() {
        let info = "MemTotal:       16314512 kB\nMemFree:         1500000 kB\nMemAvailable:    8157256 kB\n";
        assert_eq!(super::meminfo_mib(info, "MemTotal"), Some(15932));
        assert_eq!(super::meminfo_mib(info, "MemAvailable"), Some(7966));
        assert_eq!(super::meminfo_mib(info, "Mem"), None, "a whole field name");
        assert_eq!(super::meminfo_mib("MemTotal: 10\n", "MemTotal"), None);
    }
}
