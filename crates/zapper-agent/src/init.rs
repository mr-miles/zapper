//! PID 1: brings the guest up from the session spec.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use globset::GlobBuilder;
use zapper_policy::proto::{GuestEvent, GuestMsg, GuestSpec};
use zapper_policy::FsAccess;

use crate::control::Control;
use crate::{acct, forward, shell, sys, usage};

pub const UID: u32 = 1000;
const EMPTY_FILE: &str = "/run/zapper/empty";
const CA_PATH: &str = "/usr/local/share/ca-certificates/zapper.crt";
const CA_BUNDLE: &str = "/etc/ssl/certs/ca-certificates.crt";

pub fn run() -> ! {
    if let Err(e) = early() {
        eprintln!("zapper-agent: early setup: {e:#}");
        sys::power_off();
    }
    let (control, spec) = match Control::connect() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("zapper-agent: {e:#}");
            sys::power_off();
        }
    };
    if let Err(e) = setup(&control, &spec) {
        let error = format!("{e:#}");
        eprintln!("zapper-agent: setup: {error}");
        control.send(&GuestMsg::Failed { error });
        std::thread::sleep(Duration::from_secs(1));
        sys::power_off();
    }
    // Only now: setup commands above wait for their own children.
    start_reaper();
    control.send(&GuestMsg::Ready);
    shell::serve(control, spec)
}

/// Started from the initramfs: mount the session's root disk and make it `/`.
/// The agent keeps running from the initramfs copy of its binary.
fn switch_root() -> Result<()> {
    sys::mount("devtmpfs", "/dev", "devtmpfs", libc::MS_NOSUID, "mode=0755")?;
    sys::mount("/dev/vda", "/newroot", "ext4", 0, "").context("root disk")?;
    sys::mount("/dev", "/newroot/dev", "", libc::MS_MOVE, "")?;
    std::env::set_current_dir("/newroot")?;
    sys::mount(".", "/", "", libc::MS_MOVE, "")?;
    sys::check(unsafe { libc::chroot(c".".as_ptr()) })?;
    std::env::set_current_dir("/")?;
    Ok(())
}

fn early() -> Result<()> {
    use libc::{MS_NODEV, MS_NOEXEC, MS_NOSUID};
    switch_root()?;
    sys::mount(
        "proc",
        "/proc",
        "proc",
        MS_NOSUID | MS_NODEV | MS_NOEXEC,
        "",
    )?;
    sys::mount(
        "sysfs",
        "/sys",
        "sysfs",
        MS_NOSUID | MS_NODEV | MS_NOEXEC,
        "",
    )?;
    sys::mount(
        "devpts",
        "/dev/pts",
        "devpts",
        MS_NOSUID | MS_NOEXEC,
        "gid=5,mode=0620,ptmxmode=0666",
    )?;
    let _ = fs::remove_file("/dev/ptmx");
    std::os::unix::fs::symlink("pts/ptmx", "/dev/ptmx")?;
    sys::mount(
        "tmpfs",
        "/dev/shm",
        "tmpfs",
        MS_NOSUID | MS_NODEV,
        "mode=1777",
    )?;
    sys::mount("tmpfs", "/run", "tmpfs", MS_NOSUID | MS_NODEV, "mode=0755")?;
    sys::mount("tmpfs", "/tmp", "tmpfs", MS_NOSUID | MS_NODEV, "mode=1777")?;
    sys::mount(
        "cgroup2",
        "/sys/fs/cgroup",
        "cgroup2",
        MS_NOSUID | MS_NODEV | MS_NOEXEC,
        "",
    )?;
    fs::create_dir_all("/run/zapper")?;
    fs::set_permissions("/run/zapper", fs::Permissions::from_mode(0o700))?;
    fs::write(EMPTY_FILE, b"")?;
    sys::loopback_up().context("loopback")?;
    Ok(())
}

fn setup(control: &Control, spec: &GuestSpec) -> Result<()> {
    sys::sethostname(&spec.hostname);
    fs::write("/etc/hostname", format!("{}\n", spec.hostname))?;
    fs::write(
        "/etc/hosts",
        format!("127.0.0.1 localhost {}\n::1 localhost\n", spec.hostname),
    )?;
    if spec.nic {
        // The kernel configured eth0 (`ip=` on the command line).
        fs::write("/etc/resolv.conf", "nameserver 10.0.2.1\n")?;
    } else {
        fs::write(
            "/etc/resolv.conf",
            "# No DNS: egress goes through the proxy at 127.0.0.1:3128.\n",
        )?;
    }
    ensure_user(&spec.user, &spec.home).context("creating user")?;
    if spec.sudo {
        ensure_sudo(&spec.user).context("agent.sudo")?;
    }

    for d in &spec.disks {
        let flags =
            if d.read_only { libc::MS_RDONLY } else { 0 } | libc::MS_NOSUID | libc::MS_NODEV;
        sys::mount(&d.device, &d.path, "ext4", flags, "")
            .with_context(|| format!("disk {}", d.device))?;
        // mkfs leaves an empty lost+found in each copied folder; it isn't
        // the user's.
        if !d.read_only {
            let _ = fs::remove_dir(Path::new(&d.path).join("lost+found"));
        }
        // Directories the mount created under the user's home are the user's.
        let home = Path::new(&spec.home);
        let mut p = Path::new(&d.path).parent();
        while let Some(dir) = p {
            if !dir.starts_with(home) || dir == home {
                break;
            }
            sys::chown(dir, UID, UID);
            p = dir.parent();
        }
    }
    install_self().context("installing zapper-agent in the guest")?;
    for f in &spec.files {
        write_file(&f.path, &f.content, f.mode, f.user_owned)
            .with_context(|| format!("file {}", f.path))?;
    }
    if !spec.ca_pem.is_empty() {
        write_file(CA_PATH, &spec.ca_pem, 0o644, false)?;
        let mut bundle = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(CA_BUNDLE)?;
        bundle.write_all(spec.ca_pem.as_bytes())?;
    }
    for rule in &spec.fs {
        for path in expand(&rule.path)? {
            apply_fs(&path, rule.access)
                .with_context(|| format!("fs rule {:?} on {}", rule.path, path.display()))?;
        }
    }
    acct::start(control.clone());
    usage::start(control.clone());
    if !spec.nic {
        forward::start().context("proxy forwarder")?;
    }
    forward::inbound(&spec.forwards).context("facility forwards")?;

    for (i, cmd) in spec.setup.iter().enumerate() {
        let out = Command::new("/bin/sh")
            .args(["-c", cmd])
            .env_clear()
            .envs(&spec.env)
            .env("HOME", "/root")
            .env("PATH", shell::DEFAULT_PATH)
            .current_dir("/")
            .output()
            .with_context(|| format!("setup step {i}"))?;
        let mut output = String::from_utf8_lossy(&out.stdout).into_owned();
        output.push_str(&String::from_utf8_lossy(&out.stderr));
        let tail = output.len().saturating_sub(8192);
        let ok = out.status.success();
        control.event(GuestEvent::Setup {
            step: cmd.clone(),
            ok,
            output: output[tail..].to_string(),
        });
        if !ok {
            bail!("setup step {cmd:?} failed with {}", out.status);
        }
    }
    Ok(())
}

/// Copies this binary to `/usr/local/bin/zapper-agent`, also as `office`:
/// firm workers run it as their MCP server and command line.
fn install_self() -> Result<()> {
    let to = Path::new("/usr/local/bin/zapper-agent");
    fs::create_dir_all("/usr/local/bin")?;
    let _ = fs::remove_file(to);
    fs::copy("/proc/self/exe", to)?;
    fs::set_permissions(to, fs::Permissions::from_mode(0o755))?;
    sys::chown(to, 0, 0);
    #[cfg(feature = "office")]
    {
        let link = Path::new("/usr/local/bin/office");
        let _ = fs::remove_file(link);
        std::os::unix::fs::symlink("zapper-agent", link)?;
    }
    Ok(())
}

/// Makes `user` the only account with uid/gid 1000.
fn ensure_user(user: &str, home: &str) -> Result<()> {
    let shell = if Path::new("/bin/bash").exists() {
        "/bin/bash"
    } else {
        "/bin/sh"
    };
    let keep = |line: &str, id_field: usize| {
        let f: Vec<&str> = line.split(':').collect();
        f.first() != Some(&user) && f.get(id_field) != Some(&"1000")
    };
    let rewrite = |path: &str, id_field: usize, entry: String| -> Result<()> {
        let old = fs::read_to_string(path).unwrap_or_default();
        let mut lines: Vec<String> = old
            .lines()
            .filter(|l| keep(l, id_field))
            .map(str::to_string)
            .collect();
        lines.push(entry);
        fs::write(path, lines.join("\n") + "\n")?;
        Ok(())
    };
    rewrite(
        "/etc/passwd",
        2,
        format!("{user}:x:{UID}:{UID}::{home}:{shell}"),
    )?;
    rewrite("/etc/group", 2, format!("{user}:x:{UID}:"))?;
    if Path::new("/etc/shadow").exists() {
        // Shadow lines have no uid; drop by name only.
        let old = fs::read_to_string("/etc/shadow")?;
        let mut lines: Vec<String> = old
            .lines()
            .filter(|l| !l.starts_with(&format!("{user}:")))
            .map(str::to_string)
            .collect();
        lines.push(format!("{user}:!:19000::::::"));
        fs::write("/etc/shadow", lines.join("\n") + "\n")?;
    }
    fs::create_dir_all(home)?;
    if let Ok(skel) = fs::read_dir("/etc/skel") {
        for entry in skel.flatten() {
            let to = Path::new(home).join(entry.file_name());
            if !to.exists() && entry.file_type().is_ok_and(|t| t.is_file()) {
                fs::copy(entry.path(), &to)?;
                sys::chown(&to, UID, UID);
            }
        }
    }
    sys::chown(Path::new(home), UID, UID);
    fs::set_permissions(home, fs::Permissions::from_mode(0o750))?;
    Ok(())
}

/// Lets `user` run anything as root without a password (`agent.sudo`).
fn ensure_sudo(user: &str) -> Result<()> {
    if !["/usr/bin/sudo", "/usr/local/bin/sudo", "/bin/sudo"]
        .iter()
        .any(|p| Path::new(p).exists())
    {
        bail!("the image has no sudo; install it (apt-get install sudo)");
    }
    fs::create_dir_all("/etc/sudoers.d")?;
    // sudo reads the drop-in only when sudoers includes the directory.
    let main = fs::read_to_string("/etc/sudoers").unwrap_or_default();
    if !main.lines().any(|l| {
        let l = l.trim();
        l.starts_with("#includedir /etc/sudoers.d") || l.starts_with("@includedir /etc/sudoers.d")
    }) {
        let mut main = main;
        main.push_str("\n@includedir /etc/sudoers.d\n");
        fs::write("/etc/sudoers", main)?;
        fs::set_permissions("/etc/sudoers", fs::Permissions::from_mode(0o440))?;
    }
    let drop_in = "/etc/sudoers.d/zapper";
    fs::write(
        drop_in,
        // Nothing but this rule: sudo-rs, which Ubuntu 26.04 can use instead
        // of sudo, rejects settings such as `requiretty`.
        format!("{user} ALL=(ALL:ALL) NOPASSWD: ALL\n"),
    )?;
    fs::set_permissions(drop_in, fs::Permissions::from_mode(0o440))?;
    sys::chown(Path::new(drop_in), 0, 0);
    Ok(())
}

fn write_file(path: &str, content: &str, mode: u32, user_owned: bool) -> Result<()> {
    let path = Path::new(path);
    if let Some(parent) = path.parent() {
        // Create missing parents, owned like the file.
        let mut missing = Vec::new();
        let mut p = parent;
        while !p.exists() {
            missing.push(p.to_path_buf());
            p = p.parent().unwrap_or(Path::new("/"));
        }
        fs::create_dir_all(parent)?;
        if user_owned {
            for dir in missing {
                sys::chown(&dir, UID, UID);
            }
        }
    }
    let _ = fs::remove_file(path);
    fs::write(path, content)?;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    if user_owned {
        sys::chown(path, UID, UID);
    }
    Ok(())
}

/// Paths matching a glob, found by walking from its literal prefix without
/// following symlinks.
fn expand(pattern: &str) -> Result<Vec<PathBuf>> {
    let glob = GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()?
        .compile_matcher();
    let prefix: PathBuf = Path::new(pattern)
        .components()
        .take_while(|c| {
            !c.as_os_str()
                .to_string_lossy()
                .contains(['*', '?', '[', '{'])
        })
        .collect();
    let mut found = Vec::new();
    let mut stack = vec![prefix];
    let mut visited = 0;
    while let Some(p) = stack.pop() {
        visited += 1;
        if visited > 200_000 {
            bail!("{pattern}: too many files to scan");
        }
        let Ok(meta) = fs::symlink_metadata(&p) else {
            continue;
        };
        if glob.is_match(&p) {
            found.push(p.clone());
            continue;
        }
        if meta.is_dir() {
            if let Ok(entries) = fs::read_dir(&p) {
                stack.extend(entries.flatten().map(|e| e.path()));
            }
        }
    }
    Ok(found)
}

fn apply_fs(path: &Path, access: FsAccess) -> Result<()> {
    let p = path.to_string_lossy();
    let meta = fs::symlink_metadata(path)?;
    match access {
        FsAccess::Hidden if meta.is_dir() => {
            sys::mount("tmpfs", &p, "tmpfs", libc::MS_RDONLY, "size=4k,mode=0555")?
        }
        FsAccess::Hidden => sys::bind(EMPTY_FILE, &p, true)?,
        FsAccess::Ro => sys::bind(&p, &p, true)?,
    }
    Ok(())
}

// Child reaping. PID 1 inherits every orphan, so one thread waits for all
// children and hands the statuses of watched ones to their waiters.

#[derive(Default)]
struct Reaper {
    waiters: HashMap<i32, Sender<i32>>,
    /// Statuses of children that exited before anyone waited for them.
    early: HashMap<i32, i32>,
}

fn reaper() -> &'static Mutex<Reaper> {
    static R: OnceLock<Mutex<Reaper>> = OnceLock::new();
    R.get_or_init(Default::default)
}

fn start_reaper() {
    std::thread::spawn(|| loop {
        let mut status = 0;
        let pid = unsafe { libc::waitpid(-1, &mut status, 0) };
        if pid < 0 {
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }
        let code = if libc::WIFEXITED(status) {
            libc::WEXITSTATUS(status)
        } else {
            128 + libc::WTERMSIG(status)
        };
        let mut r = reaper().lock().unwrap();
        match r.waiters.remove(&pid) {
            Some(tx) => drop(tx.send(code)),
            None => {
                if r.early.len() > 4096 {
                    r.early.clear();
                }
                r.early.insert(pid, code);
            }
        }
    });
}

/// Receives the exit code of a child. Call with the pid from `fork`.
pub fn wait_child(pid: i32) -> Receiver<i32> {
    let (tx, rx) = channel();
    let mut r = reaper().lock().unwrap();
    match r.early.remove(&pid) {
        Some(code) => drop(tx.send(code)),
        None => drop(r.waiters.insert(pid, tx)),
    }
    rx
}
