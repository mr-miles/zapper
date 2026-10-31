//! A session's directory: the one owner of its layout.

use std::path::{Path, PathBuf};

/// A session's directory and the files and sockets laid out under it: the
/// single owner of that layout. `vm` (the VMM process), the record and the
/// driver are handed paths from here rather than joining their own, so the
/// structure lives in one place.
#[derive(Clone)]
pub(crate) struct SessionDir {
    root: PathBuf,
}

impl SessionDir {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
    /// The session directory itself: console log, recordings and the audit
    /// log live here, outside the part the VMM sees.
    pub fn root(&self) -> &Path {
        &self.root
    }
    /// The part the VMM sees: disks, its config, metrics and sockets.
    pub fn vm(&self) -> PathBuf {
        self.root.join("vm")
    }
    pub fn root_disk(&self) -> PathBuf {
        self.vm().join("root.ext4")
    }
    /// The disks in the VMM's directory, by file name: kept while a
    /// persistent session is stopped.
    pub fn disks(&self) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = std::fs::read_dir(self.vm())
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "ext4"))
            .collect();
        v.sort();
        v
    }
    /// What the disks take on the host: their allocated blocks, not their
    /// sparse size.
    pub fn disk_bytes(&self) -> u64 {
        use std::os::unix::fs::MetadataExt;
        self.disks()
            .iter()
            .filter_map(|p| p.metadata().ok())
            .map(|m| m.blocks() * 512)
            .sum()
    }
    /// A persistent session's CA, kept (`tls::SessionCa::kept`).
    pub fn ca(&self) -> PathBuf {
        self.root.join("ca.json")
    }
    /// The synthetic names a persistent session has been given, in order
    /// (`dns::SessionDns::kept`).
    pub fn dns(&self) -> PathBuf {
        self.root.join("dns.jsonl")
    }
    /// A durable VMM's pid and subordinate id (`vm::Vmm`), for a daemon that
    /// didn't start it.
    pub fn vmm(&self) -> PathBuf {
        self.root.join("vmm.json")
    }
    /// A persistent session's resume token: what its agent sends to take
    /// its control channel again (`GuestMsg::Resume`).
    pub fn resume_token(&self) -> PathBuf {
        self.root.join("resume.token")
    }
    /// A persistent session as it was created ([`Saved`]), so a daemon that
    /// starts again lists it, stopped.
    pub fn saved(&self) -> PathBuf {
        self.root.join("session.json")
    }

    pub fn data_disk(&self, i: usize) -> PathBuf {
        self.vm().join(format!("disk{i}.ext4"))
    }
    /// The VM's vsock socket; guest-initiated connections arrive on
    /// `<vsock>_<port>`.
    pub fn vsock(&self) -> PathBuf {
        self.vm().join("v.sock")
    }
    pub fn vsock_port(&self, port: u32) -> PathBuf {
        PathBuf::from(format!("{}_{port}", self.vsock().display()))
    }
    /// The daemon's socket for the guest's captured traffic (`netns`).
    pub fn nic_socket(&self) -> PathBuf {
        self.vm().join(crate::netns::SOCKET)
    }
    /// Firecracker's own config and metrics, under the VMM's directory.
    pub fn vm_config(&self) -> PathBuf {
        self.vm().join("vm.json")
    }
    pub fn metrics(&self) -> PathBuf {
        self.vm().join("metrics.json")
    }
    pub fn console(&self) -> PathBuf {
        self.root.join("console.log")
    }
    pub fn audit(&self) -> PathBuf {
        self.root.join("audit.jsonl")
    }
    pub fn driver_log(&self) -> PathBuf {
        self.root.join("driver.jsonl")
    }

    /// A persistent session's driven agent's conversation id
    /// (`driver::Driver::kept`).
    pub fn conversation(&self) -> PathBuf {
        self.root.join("conversation.json")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn session_dir_lays_the_vm_files_apart_from_the_logs() {
        let d = SessionDir::new(PathBuf::from("/s"));
        assert_eq!(d.root(), Path::new("/s"));
        // Under the VMM's `vm` subdirectory: disks, sockets, config, metrics.
        assert_eq!(d.vm(), Path::new("/s/vm"));
        assert_eq!(d.root_disk(), Path::new("/s/vm/root.ext4"));
        assert_eq!(d.data_disk(0), Path::new("/s/vm/disk0.ext4"));
        assert_eq!(d.vsock(), Path::new("/s/vm/v.sock"));
        assert_eq!(d.vsock_port(5), Path::new("/s/vm/v.sock_5"));
        assert_eq!(d.nic_socket(), Path::new("/s/vm/nic.sock"));
        assert_eq!(d.vm_config(), Path::new("/s/vm/vm.json"));
        assert_eq!(d.metrics(), Path::new("/s/vm/metrics.json"));
        // Logs, recordings and the audit log stay outside it.
        assert_eq!(d.console(), Path::new("/s/console.log"));
        assert_eq!(d.audit(), Path::new("/s/audit.jsonl"));
        assert_eq!(d.driver_log(), Path::new("/s/driver.jsonl"));
    }
}
