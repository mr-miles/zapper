//! Firecracker microVMs: disks, initramfs and the VMM process.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{bail, Context, Result};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use zapper_policy::{expand_home, Engine, Mount, MountMode};

/// Guest CID; every VM has its own vsock device, so they can share it.
pub const GUEST_CID: u32 = 3;
/// Serial console output kept per session; the rest is dropped.
const CONSOLE_LIMIT: u64 = 1 << 20;

pub use zapper_policy::parse_size;

async fn run(cmd: &mut Command) -> Result<()> {
    let out = cmd
        .stdin(Stdio::null())
        .output()
        .await
        .with_context(|| format!("running {cmd:?}"))?;
    if !out.status.success() {
        bail!(
            "{cmd:?} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Writes a newc cpio archive holding the agent as `/init`.
pub fn build_initrd(agent: &Path, out: &Path) -> Result<()> {
    let bin = std::fs::read(agent).with_context(|| format!("reading agent {}", agent.display()))?;
    let mut cpio = Vec::new();
    let mut ino = 1;
    let mut entry = |name: &str, mode: u32, rdev: (u32, u32), data: &[u8]| {
        let header = format!(
            "070701{ino:08x}{mode:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}",
            0,
            0,
            1,
            0,
            data.len(),
            0,
            0,
            rdev.0,
            rdev.1,
            name.len() + 1,
            0
        );
        ino += 1;
        cpio.extend_from_slice(header.as_bytes());
        cpio.extend_from_slice(name.as_bytes());
        cpio.push(0);
        while cpio.len() % 4 != 0 {
            cpio.push(0);
        }
        cpio.extend_from_slice(data);
        while cpio.len() % 4 != 0 {
            cpio.push(0);
        }
    };
    entry("dev", 0o040755, (0, 0), &[]);
    entry("dev/console", 0o020600, (5, 1), &[]);
    entry("newroot", 0o040755, (0, 0), &[]);
    entry("init", 0o100755, (0, 0), &bin);
    entry("TRAILER!!!", 0, (0, 0), &[]);
    let tmp = out.with_extension("tmp");
    std::fs::write(&tmp, cpio)?;
    std::fs::rename(tmp, out)?;
    Ok(())
}

/// Makes the session's root disk: a private copy of the image grown to size.
pub async fn root_disk(image: &Path, out: &Path, size: u64) -> Result<()> {
    if !image.exists() {
        bail!(
            "image {} not found; build it with `zapper image build`",
            image.display()
        );
    }
    run(Command::new("cp")
        .arg("--reflink=auto")
        .arg("--sparse=always")
        .arg(image)
        .arg(out))
    .await?;
    let current = tokio::fs::metadata(out).await?.len();
    if size > current {
        let f = tokio::fs::OpenOptions::new().write(true).open(out).await?;
        f.set_len(size).await?;
        // e2fsck exits 1 when it fixed something, which is fine here.
        let _ = Command::new("e2fsck")
            .args(["-fy", "-E", "discard"])
            .arg(out)
            .stdin(Stdio::null())
            .output()
            .await;
        run(Command::new("resize2fs").arg("-f").arg(out)).await?;
    }
    Ok(())
}

/// Packs a host directory into an ext4 disk.
pub async fn mount_disk(mount: &Mount, workdir: &Path, out: &Path) -> Result<PathBuf> {
    let host = expand_home(&mount.host.replace("{workdir}", &workdir.to_string_lossy()));
    let host = PathBuf::from(host);
    if !host.is_dir() {
        bail!("mount {}: not a directory", host.display());
    }
    let du = Command::new("du").arg("-sb").arg(&host).output().await?;
    let used: u64 = String::from_utf8_lossy(&du.stdout)
        .split_whitespace()
        .next()
        .and_then(|n| n.parse().ok())
        .unwrap_or(0);
    let extra = match (&mount.extra, mount.mode) {
        (Some(e), _) => parse_size(e)?,
        (None, MountMode::Rw) => 1 << 30,
        (None, MountMode::Ro) => 64 << 20,
    };
    // ext4 metadata overhead: inodes, journal.
    let size = (used + used / 5 + extra).max(32 << 20);
    run(Command::new("mkfs.ext4")
        .args(["-q", "-F", "-E", "root_owner=1000:1000", "-d"])
        .arg(&host)
        .arg(out)
        .arg(format!("{}k", size / 1024)))
    .await?;
    Ok(host)
}

pub struct VmSpec<'a> {
    pub firecracker: &'a Path,
    pub kernel: PathBuf,
    pub initrd: &'a Path,
    pub root: PathBuf,
    /// Extra disks, attached as `/dev/vdb`, `/dev/vdc`, …
    pub disks: Vec<(PathBuf, bool)>,
    pub engine: &'a Engine,
    /// The session directory; the VMM gets its `vm` subdirectory.
    pub dir: &'a Path,
    pub id: &'a str,
    /// Give the guest a NIC, captured in the VMM's network namespace
    /// (`crate::netns`).
    pub nic: bool,
}

/// The part of a session directory the VMM sees: disks, its config,
/// metrics and sockets. Logs, recordings and the audit log stay outside it.
pub fn vm_dir(session_dir: &Path) -> PathBuf {
    session_dir.join("vm")
}

pub fn vsock_path(session_dir: &Path) -> PathBuf {
    vm_dir(session_dir).join("v.sock")
}

/// Starts the VMM. Its serial console goes to `console.log`, capped.
pub async fn start(spec: VmSpec<'_>) -> Result<Child> {
    let mut drives = vec![json!({
        "drive_id": "root", "path_on_host": spec.root, "is_root_device": false, "is_read_only": false,
    })];
    for (i, (path, ro)) in spec.disks.iter().enumerate() {
        drives.push(json!({ "drive_id": format!("d{i}"), "path_on_host": path, "is_root_device": false, "is_read_only": ro }));
    }
    let net_args = if spec.nic {
        crate::netns::boot_args()
    } else {
        String::new()
    };
    let boot_args = format!(
        "console=ttyS0 reboot=k panic=-1 pci=off quiet loglevel=3 rdinit=/init {net_args} {}",
        spec.engine.boot_args
    );
    let mut config = json!({
        "boot-source": { "kernel_image_path": spec.kernel, "initrd_path": spec.initrd, "boot_args": boot_args.trim() },
        "drives": drives,
        "machine-config": { "vcpu_count": spec.engine.vcpus, "mem_size_mib": spec.engine.memory_mib, "smt": false },
        "vsock": { "guest_cid": GUEST_CID, "uds_path": vsock_path(spec.dir) },
        "metrics": { "metrics_path": vm_dir(spec.dir).join("metrics.json") },
        "entropy": {},
    });
    if spec.nic {
        config["network-interfaces"] = json!([{ "iface_id": "eth0", "guest_mac": crate::netns::GUEST_MAC, "host_dev_name": crate::netns::TAP }]);
    }
    if !spec.kernel.exists() {
        bail!("kernel {} not found", spec.kernel.display());
    }
    let vmdir = vm_dir(spec.dir);
    let config_path = vmdir.join("vm.json");
    tokio::fs::write(&config_path, serde_json::to_vec_pretty(&config)?).await?;
    tokio::fs::write(vmdir.join("metrics.json"), b"").await?;
    // The VMM runs confined in its own namespaces, set up by this binary's
    // `__vmm` mode (`crate::vmm`).
    let shim = crate::vmm::Args {
        dir: vmdir.clone(),
        firecracker: spec.firecracker.to_path_buf(),
        config: config_path.clone(),
        id: spec.id.to_string(),
        kernel: spec.kernel.clone(),
        initrd: spec.initrd.to_path_buf(),
        nic: spec.nic,
    };
    // The daemon's own binary as it runs: /proc/self/exe stays valid when
    // the file on disk is replaced (a rebuild, `make install`), where the
    // path from current_exe() then names a deleted file and every VM after
    // it would fail to start.
    let mut cmd = Command::new("/proc/self/exe");
    cmd.args(shim.to_args());
    // The VMM dies with the daemon, even if the daemon crashes.
    unsafe {
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
            Ok(())
        });
    }
    let mut child = cmd
        .current_dir(&vmdir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| {
            format!(
                "starting the VMM (zapper __vmm) for {}",
                spec.firecracker.display()
            )
        })?;
    let console = spec.dir.join("console.log");
    for stream in [
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Unpin + Send>),
        child.stderr.take().map(|s| Box::new(s) as _),
    ]
    .into_iter()
    .flatten()
    {
        let console = console.clone();
        tokio::spawn(async move {
            let Ok(mut out) = tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&console)
                .await
            else {
                return;
            };
            let mut stream = stream;
            let mut buf = vec![0u8; 8192];
            let mut written = 0u64;
            while let Ok(n) = stream.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                if written < CONSOLE_LIMIT {
                    let _ = out.write_all(&buf[..n]).await;
                    written += n as u64;
                }
            }
        });
    }
    Ok(child)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("4GiB").unwrap(), 4 << 30);
        assert_eq!(parse_size("512M").unwrap(), 512 << 20);
        assert_eq!(parse_size("100").unwrap(), 100);
        assert!(parse_size("4 parsecs").is_err());
    }
}
