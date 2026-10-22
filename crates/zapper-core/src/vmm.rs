//! `zapper __vmm`: runs a session's Firecracker in its own user, network and
//! mount namespaces.
//!
//! The process becomes root of a new user namespace, which needs no host
//! privileges, and from there:
//!
//! - in its new network namespace, builds the guest's network (`crate::netns`)
//!   for a NIC session, or leaves it empty for a vsock session;
//! - in its new mount namespace, builds a root holding only what Firecracker
//!   uses, and `pivot_root`s into it. Files appear at their host paths, so the
//!   VM config works unchanged: the session's VMM directory (`vm/`: disks,
//!   config, metrics, sockets; read-write), the kernel, initramfs and
//!   Firecracker binary (read-only), a few devices and a private `/tmp`. The
//!   session's audit log and recordings, and the daemon user's other files,
//!   secret files included, are absent.
//!
//! Runs before any other thread exists, as `unshare(CLONE_NEWUSER)` requires.

use std::ffi::CString;
use std::io;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

use crate::netns;

/// Directory in the VMM's directory where the new root is built.
pub const ROOT: &str = ".vmroot";

pub struct Args {
    pub dir: PathBuf,
    pub firecracker: PathBuf,
    pub config: PathBuf,
    pub id: String,
    pub kernel: PathBuf,
    pub initrd: PathBuf,
    pub nic: bool,
}

impl Args {
    /// Arguments after `__vmm`, in the order `to_args` writes them.
    fn parse(args: &[String]) -> Option<Args> {
        let [dir, firecracker, config, id, kernel, initrd, net] = args else {
            return None;
        };
        Some(Args {
            dir: dir.into(),
            firecracker: firecracker.into(),
            config: config.into(),
            id: id.clone(),
            kernel: kernel.into(),
            initrd: initrd.into(),
            nic: match net.as_str() {
                "nic" => true,
                "none" => false,
                _ => return None,
            },
        })
    }

    pub fn to_args(&self) -> Vec<std::ffi::OsString> {
        let mut v: Vec<std::ffi::OsString> = vec!["__vmm".into()];
        for p in [&self.dir, &self.firecracker, &self.config] {
            v.push(p.into());
        }
        v.push(self.id.clone().into());
        v.push((&self.kernel).into());
        v.push((&self.initrd).into());
        v.push(if self.nic { "nic" } else { "none" }.into());
        v
    }
}

fn check(ret: libc::c_int, what: &str) -> Result<()> {
    if ret < 0 {
        bail!("{what}: {}", io::Error::last_os_error());
    }
    Ok(())
}

fn cstr(p: &Path) -> CString {
    CString::new(p.as_os_str().as_encoded_bytes()).expect("path without NUL")
}

fn mount(
    src: Option<&Path>,
    target: &Path,
    fstype: Option<&str>,
    flags: libc::c_ulong,
    data: Option<&str>,
) -> Result<()> {
    let src = src.map(cstr);
    let target_c = cstr(target);
    let fstype = fstype.map(|f| CString::new(f).unwrap());
    let data = data.map(|d| CString::new(d).unwrap());
    let r = unsafe {
        libc::mount(
            src.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()),
            target_c.as_ptr(),
            fstype.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()),
            flags,
            data.as_ref()
                .map_or(std::ptr::null(), |s| s.as_ptr() as *const libc::c_void),
        )
    };
    check(r, &format!("mount {}", target.display()))
}

/// Enters new user, network and mount namespaces as their root.
fn unshare() -> Result<()> {
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let flags = libc::CLONE_NEWUSER | libc::CLONE_NEWNET | libc::CLONE_NEWNS;
    if unsafe { libc::unshare(flags) } != 0 {
        let e = io::Error::last_os_error();
        bail!("unshare(user, net, mount): {e} (unprivileged user namespaces disabled? see docs/network.md)");
    }
    std::fs::write("/proc/self/setgroups", "deny").context("setgroups")?;
    std::fs::write("/proc/self/uid_map", format!("0 {uid} 1")).context("uid_map")?;
    std::fs::write("/proc/self/gid_map", format!("0 {gid} 1")).context("gid_map")?;
    Ok(())
}

/// Bind-mounts `src` at the same path under `root`.
fn bind(root: &Path, src: &Path, read_only: bool) -> Result<()> {
    let meta = std::fs::metadata(src).with_context(|| format!("bind source {}", src.display()))?;
    let target = root.join(src.strip_prefix("/").unwrap_or(src));
    if meta.is_dir() {
        std::fs::create_dir_all(&target)?;
    } else {
        std::fs::create_dir_all(target.parent().unwrap())?;
        std::fs::write(&target, b"")?;
    }
    mount(Some(src), &target, None, libc::MS_BIND, None)?;
    if read_only {
        mount(
            None,
            &target,
            None,
            libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY | libc::MS_NOSUID,
            None,
        )?;
    }
    Ok(())
}

/// Builds the minimal root and pivots into it.
fn confine(a: &Args) -> Result<()> {
    // Keep these mounts in this namespace.
    mount(
        None,
        Path::new("/"),
        None,
        libc::MS_REC | libc::MS_PRIVATE,
        None,
    )?;
    let root = a.dir.join(ROOT);
    std::fs::create_dir_all(&root)?;
    mount(
        Some(Path::new("tmpfs")),
        &root,
        Some("tmpfs"),
        libc::MS_NOSUID,
        Some("mode=0755,size=1m"),
    )?;

    // The private /tmp first: binds below it (a state directory under /tmp)
    // must land on top of it, not under it.
    let tmp = root.join("tmp");
    std::fs::create_dir_all(&tmp)?;
    mount(
        Some(Path::new("tmpfs")),
        &tmp,
        Some("tmpfs"),
        libc::MS_NOSUID | libc::MS_NODEV,
        Some("mode=1777,size=16m"),
    )?;

    bind(&root, &a.dir, false)?;
    for p in [&a.kernel, &a.initrd, &a.firecracker] {
        bind(&root, p, true)?;
    }
    let mut devices = vec!["/dev/kvm", "/dev/null", "/dev/urandom"];
    if a.nic {
        devices.push("/dev/net/tun");
    }
    for d in devices {
        bind(&root, Path::new(d), false)?;
    }
    let old = root.join(".old");
    std::fs::create_dir_all(&old)?;
    let r = unsafe {
        libc::syscall(
            libc::SYS_pivot_root,
            cstr(&root).as_ptr(),
            cstr(&old).as_ptr(),
        )
    } as libc::c_int;
    check(r, "pivot_root")?;
    std::env::set_current_dir("/")?;
    check(
        unsafe { libc::umount2(c"/.old".as_ptr(), libc::MNT_DETACH) },
        "unmount old root",
    )?;
    std::fs::remove_dir("/.old").context("removing /.old")?;
    // The root itself holds only mount points: make it read-only.
    mount(
        None,
        Path::new("/"),
        None,
        libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY | libc::MS_NOSUID,
        None,
    )?;
    std::env::set_current_dir(&a.dir)
        .with_context(|| format!("{} in the new root", a.dir.display()))?;
    Ok(())
}

/// `zapper __vmm --probe [nic]`: can this host build the namespaces, and
/// with `nic` the guest's network?
pub fn probe(nic: bool) -> i32 {
    match unshare().and_then(|_| if nic { netns::setup() } else { Ok(()) }) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("{e:#}");
            1
        }
    }
}

pub fn main(args: &[String]) -> i32 {
    let Some(a) = Args::parse(args) else {
        eprintln!("usage: zapper __vmm DIR FIRECRACKER CONFIG ID KERNEL INITRD nic|none");
        return 2;
    };
    match run(&a) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("zapper vmm: {e:#}");
            1
        }
    }
}

fn run(a: &Args) -> Result<i32> {
    let parent = unsafe { libc::getppid() };
    unshare()?;
    if a.nic {
        netns::setup()?;
    }
    confine(a)?;
    // Entering the user namespace changed credentials, which cleared the
    // parent-death signal the daemon set: set it again.
    unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) };
    if unsafe { libc::getppid() } != parent {
        return Ok(1);
    }
    if a.nic {
        netns::serve(&a.dir)?;
    }
    let status = unsafe {
        Command::new(&a.firecracker)
            .arg("--no-api")
            .arg("--config-file")
            .arg(&a.config)
            .args(["--level", "Warn", "--id", &a.id])
            .current_dir(&a.dir)
            .stdin(Stdio::null())
            .pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                Ok(())
            })
            .status()
            .with_context(|| format!("starting {}", a.firecracker.display()))?
    };
    Ok(status.code().unwrap_or(1))
}
