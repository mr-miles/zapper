//! Firecracker microVMs: disks, initramfs and the VMM process.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{bail, Context, Result};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use zapper_policy::{expand_home, Engine, Mount, MountMode};

use super::session::SessionDir;

/// Guest CID; every VM has its own vsock device, so they can share it.
pub const GUEST_CID: u32 = 3;
// Serial console output kept per session; the rest is dropped.
use crate::vmm::CONSOLE_LIMIT;

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
    /// The session's directory layout; the VMM works under its `vm`
    /// subdirectory.
    pub dir: &'a SessionDir,
    pub id: &'a str,
    /// Give the guest a NIC, captured in the VMM's network namespace
    /// (`crate::netns`).
    pub nic: bool,
    /// Run Firecracker as a subordinate uid of its own (`daemon.vmm_subids`).
    pub subids: bool,
    /// A persistent session's: the VMM outlives the daemon (`vmm::Args`).
    pub durable: bool,
}

/// What a daemon needs to know of a durable VMM it didn't start: its pid,
/// and the subordinate id its Firecracker runs as. In `vmm.json`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct Vmm {
    pub pid: u32,
    pub subid: Option<u32>,
}

/// The VMM's `oom_score_adj`, as written to `/proc/self/oom_score_adj`:
/// well above the daemon's (0), so the kernel's OOM killer picks a VM first.
const VMM_OOM_SCORE_ADJ: &[u8] = b"500";

/// Sets this process's `oom_score_adj` to [`VMM_OOM_SCORE_ADJ`], between
/// fork and exec: raising one's own score needs no privilege, and
/// Firecracker inherits it. Only async-signal-safe calls.
fn raise_oom_score() {
    unsafe {
        let fd = libc::open(
            c"/proc/self/oom_score_adj".as_ptr(),
            libc::O_WRONLY | libc::O_CLOEXEC,
        );
        if fd >= 0 {
            libc::write(
                fd,
                VMM_OOM_SCORE_ADJ.as_ptr().cast(),
                VMM_OOM_SCORE_ADJ.len(),
            );
            libc::close(fd);
        }
    }
}

/// A session's VM: the VMM this daemon started, or a persistent session's,
/// started by an earlier daemon and adopted, known by its pid only.
pub enum VmProc {
    Child(Child),
    Adopted { pid: u32, session: String },
}

impl VmProc {
    /// How it ended, once it has.
    pub fn exited(&mut self) -> Option<String> {
        match self {
            VmProc::Child(c) => c
                .try_wait()
                .ok()
                .flatten()
                .map(|s| format!("vm exited ({s})")),
            VmProc::Adopted { pid, session } => {
                (!is_vmm_of(*pid, session)).then(|| "vm exited".to_string())
            }
        }
    }

    /// Waits up to `grace` for it to end (its guest powering off), then
    /// kills it.
    pub async fn stop_within(&mut self, grace: std::time::Duration) {
        match self {
            VmProc::Child(c) => {
                if tokio::time::timeout(grace, c.wait()).await.is_err() {
                    let _ = c.kill().await;
                }
            }
            VmProc::Adopted { pid, session } => {
                let until = std::time::Instant::now() + grace;
                while is_vmm_of(*pid, session) && std::time::Instant::now() < until {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
                if is_vmm_of(*pid, session) {
                    unsafe { libc::kill(*pid as i32, libc::SIGKILL) };
                }
            }
        }
    }
}

/// Whether `pid` runs the VMM of session `id`: `zapper __vmm`, with the id
/// among its arguments (not a pid reused since).
pub fn is_vmm_of(pid: u32, id: &str) -> bool {
    let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
        return false;
    };
    let args: Vec<&[u8]> = cmdline.split(|b| *b == 0).collect();
    args.contains(&&b"__vmm"[..]) && args.contains(&id.as_bytes())
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
        "vsock": { "guest_cid": GUEST_CID, "uds_path": spec.dir.vsock() },
        "metrics": { "metrics_path": spec.dir.metrics() },
        "entropy": {},
    });
    if spec.nic {
        config["network-interfaces"] = json!([{ "iface_id": "eth0", "guest_mac": crate::netns::GUEST_MAC, "host_dev_name": crate::netns::TAP }]);
    }
    if !spec.kernel.exists() {
        bail!("kernel {} not found", spec.kernel.display());
    }
    let vmdir = spec.dir.vm();
    let config_path = spec.dir.vm_config();
    tokio::fs::write(&config_path, serde_json::to_vec_pretty(&config)?).await?;
    tokio::fs::write(spec.dir.metrics(), b"").await?;
    // The VMM runs confined in its own namespaces, set up by this binary's
    // `__vmm` mode (`crate::vmm`).
    // Firecracker as a uid of the session's own where the host has
    // subordinate ids; else as the daemon user (`zapper check` says why).
    let ids = spec.subids.then(crate::subids::host).and_then(Result::ok);
    let shim = crate::vmm::Args {
        dir: vmdir.clone(),
        firecracker: spec.firecracker.to_path_buf(),
        config: config_path.clone(),
        id: spec.id.to_string(),
        kernel: spec.kernel.clone(),
        initrd: spec.initrd.to_path_buf(),
        nic: spec.nic,
        sub: ids.is_some(),
        durable: spec.durable,
    };
    // The daemon's own binary as it runs: /proc/self/exe stays valid when
    // the file on disk is replaced (a rebuild, `make install`), where the
    // path from current_exe() then names a deleted file and every VM after
    // it would fail to start.
    let mut cmd = Command::new("/proc/self/exe");
    cmd.args(shim.to_args());
    // When the host runs out of memory, the kernel kills a VM before the
    // daemon, which would end every session.
    if spec.durable {
        // Outlives the daemon: a session of its own (a signal to the
        // daemon's terminal or process group isn't its), its console
        // written by itself, capped, as no daemon may be there to read it.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                raise_oom_score();
                Ok(())
            });
        }
        let console = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(spec.dir.console())?;
        cmd.stdout(console.try_clone()?).stderr(console);
    } else {
        // The VMM dies with the daemon, even if the daemon crashes.
        unsafe {
            cmd.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                raise_oom_score();
                Ok(())
            });
        }
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    }
    let mut child = cmd
        .current_dir(&vmdir)
        .stdin(Stdio::null())
        .kill_on_drop(!spec.durable)
        .spawn()
        .with_context(|| {
            format!(
                "starting the VMM (zapper __vmm) for {}",
                spec.firecracker.display()
            )
        })?;
    // Firecracker as a uid of the session's own: the VMM waits for its map.
    let mut subid = None;
    if let Some(ids) = ids {
        let mapped = match (crate::subids::lease(ids, spec.id), child.id()) {
            (Ok(n), Some(pid)) => crate::subids::map(ids, pid, n).await.map(|_| n),
            (Err(e), _) => Err(e),
            (_, None) => Err(anyhow::anyhow!("the VMM exited at once")),
        };
        match mapped {
            Ok(n) => subid = Some(n),
            Err(e) => {
                let _ = child.kill().await;
                crate::subids::release(spec.id);
                return Err(e.context("mapping the VMM's subordinate ids"));
            }
        }
    }
    if spec.durable {
        let vmm = Vmm {
            pid: child.id().unwrap_or(0),
            subid,
        };
        crate::util::write_private(&spec.dir.vmm(), &serde_json::to_vec(&vmm)?)?;
    }
    let console = spec.dir.console();
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
