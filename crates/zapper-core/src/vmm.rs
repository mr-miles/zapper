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
const ROOT: &str = ".vmroot";

pub struct Args {
    pub dir: PathBuf,
    pub firecracker: PathBuf,
    pub config: PathBuf,
    pub id: String,
    pub kernel: PathBuf,
    pub initrd: PathBuf,
    pub nic: bool,
    /// The daemon maps a subordinate uid into the namespace, which
    /// Firecracker runs as (`crate::subids`); else the namespace's root is
    /// the daemon user, and so is Firecracker.
    pub sub: bool,
}

impl Args {
    /// Arguments after `__vmm`, in the order `to_args` writes them.
    fn parse(args: &[String]) -> Option<Args> {
        let [dir, firecracker, config, id, kernel, initrd, net, ids] = args else {
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
            sub: match ids.as_str() {
                "sub" => true,
                "own" => false,
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
        v.push(if self.sub { "sub" } else { "own" }.into());
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

/// Enters new user, network and mount namespaces as their root: the
/// daemon user. With `sub`, the daemon writes the id maps (it maps a
/// subordinate id as well, which only its setuid helpers may), and this
/// waits for them.
fn unshare(sub: bool) -> Result<()> {
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let flags = libc::CLONE_NEWUSER | libc::CLONE_NEWNET | libc::CLONE_NEWNS;
    if unsafe { libc::unshare(flags) } != 0 {
        let e = io::Error::last_os_error();
        bail!("unshare(user, net, mount): {e} (unprivileged user namespaces disabled? see docs/network.md)");
    }
    if sub {
        for _ in 0..2000 {
            if !std::fs::read_to_string("/proc/self/uid_map")?
                .trim()
                .is_empty()
                && !std::fs::read_to_string("/proc/self/gid_map")?
                    .trim()
                    .is_empty()
            {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        bail!("the daemon didn't map this VMM's ids");
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

/// The namespace's uid that Firecracker runs as with `sub`: the session's
/// subordinate uid on the host. 0 is the daemon user.
const SUB_UID: u32 = 1;

/// Lets Firecracker, as `SUB_UID` in the daemon's group, use the session's
/// VM directory: create its sockets there, use the disks, the config, the
/// metrics and the daemon's listening sockets. Nothing outside it.
fn share_with_firecracker(dir: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o770))
        .with_context(|| format!("{}", dir.display()))?;
    for e in std::fs::read_dir(dir)?.flatten() {
        let p = e.path();
        let meta = std::fs::symlink_metadata(&p)?;
        if meta.file_type().is_symlink() || meta.is_dir() {
            continue;
        }
        let mode = meta.permissions().mode() & 0o7777;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode((mode | 0o660) & !0o007))
            .with_context(|| format!("{}", p.display()))?;
    }
    Ok(())
}

/// Handles a `zapper __vmm …` re-exec: `Some(exit code)` when `args` is one,
/// `None` otherwise. The daemon starts each session's VMM by re-exec'ing the
/// binary this way (`crate::daemon::vm`). `main` must call this first thing,
/// before any thread starts, since the shim unshares namespaces, which has
/// to happen single-threaded.
pub fn shim(args: &[String]) -> Option<i32> {
    if args.get(1).map(String::as_str) != Some("__vmm") {
        return None;
    }
    Some(match args.get(2).map(String::as_str) {
        Some("--probe") => probe(args.get(3).map(String::as_str) == Some("nic")),
        _ => main(&args[2..]),
    })
}

/// `zapper __vmm --probe [nic]`: can this host build the namespaces, and
/// with `nic` the guest's network?
fn probe(nic: bool) -> i32 {
    match unshare(false).and_then(|_| if nic { netns::setup(None) } else { Ok(()) }) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("{e:#}");
            1
        }
    }
}

fn main(args: &[String]) -> i32 {
    let Some(a) = Args::parse(args) else {
        eprintln!("usage: zapper __vmm DIR FIRECRACKER CONFIG ID KERNEL INITRD nic|none sub|own");
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
    unshare(a.sub)?;
    if a.nic {
        // Firecracker opens the tap as its own uid, without capabilities:
        // the tap is that uid's.
        netns::setup(a.sub.then_some(SUB_UID))?;
    }
    confine(a)?;
    if a.sub {
        share_with_firecracker(&a.dir)?;
    }
    // Entering the user namespace changed credentials, which cleared the
    // parent-death signal the daemon set: set it again.
    unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) };
    if unsafe { libc::getppid() } != parent {
        return Ok(1);
    }
    if a.nic {
        netns::serve(&a.dir)?;
    }
    let sub = a.sub;
    let status = unsafe {
        Command::new(&a.firecracker)
            .arg("--no-api")
            .arg("--config-file")
            .arg(&a.config)
            .args(["--level", "Warn", "--id", &a.id])
            .current_dir(&a.dir)
            .stdin(Stdio::null())
            .pre_exec(move || {
                if sub {
                    // The session's own uid, no capabilities; the daemon's
                    // group, for the session's files and sockets.
                    libc::umask(0o007);
                    if libc::setresuid(SUB_UID, SUB_UID, SUB_UID) != 0 {
                        return Err(io::Error::last_os_error());
                    }
                    if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                        return Err(io::Error::last_os_error());
                    }
                }
                // After any change of uid, which clears it.
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                Ok(())
            })
            .status()
            .with_context(|| format!("starting {}", a.firecracker.display()))?
    };
    Ok(status.code().unwrap_or(1))
}
