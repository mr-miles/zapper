//! Terminals over vsock, with every exec of the workload checked by the host.

use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs::File;
use std::io::{BufReader, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use zapper_proto::proto::{
    Frame, GuestEvent, GuestSpec, ShellRequest, HOST_CID, MAX_FRAME, MAX_LINE, SHELL_PORT,
};
use zapper_proto::ExecRequest;

use crate::control::Control;
use crate::init::{wait_child, UID};
use crate::sys;

pub const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

pub fn serve(control: Control, spec: GuestSpec) -> ! {
    let listener = match sys::VsockListener::bind(SHELL_PORT) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("zapper-agent: shell listener: {e}");
            sys::power_off();
        }
    };
    let spec = Arc::new(spec);
    loop {
        let Ok((conn, cid)) = listener.accept() else {
            continue;
        };
        // Only the host may open terminals: a guest process could otherwise
        // reach this port over vsock loopback.
        if cid != HOST_CID {
            eprintln!("zapper-agent: refused shell connection from cid {cid}");
            continue;
        }
        let (control, spec) = (control.clone(), spec.clone());
        std::thread::spawn(move || {
            if let Err(e) = session(conn, &control, &spec) {
                eprintln!("zapper-agent: shell: {e:#}");
            }
        });
    }
}

fn session(conn: File, control: &Control, spec: &GuestSpec) -> Result<()> {
    // The request line a byte at a time: what follows it may go to a
    // persistent terminal's holder, untouched.
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    while line.len() < MAX_LINE {
        match (&conn).read(&mut byte)? {
            0 => break,
            _ if byte[0] == b'\n' => break,
            _ => line.push(byte[0]),
        }
    }
    let req: ShellRequest = serde_json::from_slice(&line).context("shell request")?;
    if let Some(id) = req.attach {
        if let Err(e) = crate::holder::attach(id, &conn, req.tty) {
            let mut conn = &conn;
            let _ =
                conn.write_all(&Frame::Stderr(format!("zapper: {e:#}\r\n").into_bytes()).encode());
            let _ = conn.write_all(&Frame::Exit(127).encode());
        }
        return Ok(());
    }
    let mut reader = BufReader::new(conn.try_clone()?);
    let writer = Arc::new(Mutex::new(conn));
    let send = {
        let writer = writer.clone();
        move |f: Frame| writer.lock().unwrap().write_all(&f.encode()).is_ok()
    };

    let mut env = spec.env.clone();
    env.extend(req.env.clone());
    let as_root = spec.root || req.system;
    let (home, user) = if as_root {
        ("/root".to_string(), "root".to_string())
    } else {
        (spec.home.clone(), spec.user.clone())
    };
    env.entry("PATH".into())
        .or_insert_with(|| DEFAULT_PATH.into());
    env.insert("HOME".into(), home);
    env.insert("USER".into(), user.clone());
    env.insert("LOGNAME".into(), user);
    env.entry("SHELL".into())
        .or_insert_with(|| "/bin/bash".into());
    if req.tty.is_some() {
        env.entry("TERM".into())
            .or_insert_with(|| "xterm-256color".into());
    }
    let mut child = match spawn(&req, &env, spec) {
        Ok(c) => c,
        Err(e) => {
            send(Frame::Stderr(format!("zapper: {e:#}\r\n").into_bytes()));
            send(Frame::Exit(127));
            return Ok(());
        }
    };
    if let Some(listener) = child.notify {
        let control = control.clone();
        std::thread::spawn(move || supervise(listener, control));
    }
    // A persistent terminal goes to its holder, which keeps it when the
    // host goes away; PID 1 still reaps the program, and reports its exit.
    if let (true, Some(size)) = (req.persist, req.tty) {
        let Some(master) = child.input.take() else {
            bail!("a terminal without a pty");
        };
        let conn = writer.lock().unwrap();
        if let Err(e) = crate::holder::start(req.id, child.pid, master, size, &conn, control) {
            drop(conn);
            send(Frame::Stderr(format!("zapper: {e:#}\r\n").into_bytes()));
            unsafe { libc::kill(-child.pid, libc::SIGHUP) };
            send(Frame::Exit(127));
        }
        return Ok(());
    }
    let exited = wait_child(child.pid);

    // Output pumps.
    let mut pumps = Vec::new();
    for (fd, stderr) in child.outputs {
        let send = send.clone();
        pumps.push(std::thread::spawn(move || {
            let mut f = fd;
            let mut buf = vec![0u8; MAX_FRAME];
            loop {
                match f.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let data = buf[..n].to_vec();
                        if !send(if stderr {
                            Frame::Stderr(data)
                        } else {
                            Frame::Data(data)
                        }) {
                            break;
                        }
                    }
                }
            }
        }));
    }

    // Input pump; ends when the host closes the connection.
    let pid = child.pid;
    let mut input = child.input;
    let master_fd = child.master_fd;
    std::thread::spawn(move || {
        loop {
            match Frame::read_from(&mut reader) {
                Ok(Some(Frame::Data(d))) => {
                    if let Some(i) = input.as_mut() {
                        let _ = i.write_all(&d);
                    }
                }
                Ok(Some(Frame::Resize(rows, cols))) => {
                    if let Some(fd) = master_fd {
                        sys::set_winsize(fd, rows, cols);
                    }
                }
                Ok(Some(Frame::Eof)) => {
                    if master_fd.is_some() {
                        if let Some(i) = input.as_mut() {
                            let _ = i.write_all(&[4]);
                        }
                    } else {
                        input = None;
                    }
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => {
                    // Host went away: hang up the whole session.
                    unsafe { libc::kill(-pid, libc::SIGHUP) };
                    break;
                }
            }
        }
    });

    let code = exited.recv().unwrap_or(255);
    // Background processes may hold the terminal open; don't wait for them.
    let deadline = std::time::Instant::now() + Duration::from_millis(500);
    for p in pumps {
        while !p.is_finished() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    send(Frame::Exit(code));
    control.event(GuestEvent::ShellExit {
        id: req.id,
        exit_code: code,
    });
    let w = writer.lock().unwrap();
    unsafe { libc::shutdown(w.as_raw_fd(), libc::SHUT_RDWR) };
    Ok(())
}

struct Child {
    pid: i32,
    /// Readable ends and whether each is stderr.
    outputs: Vec<(File, bool)>,
    input: Option<File>,
    master_fd: Option<RawFd>,
    notify: Option<OwnedFd>,
}

/// Finds an executable on `PATH`, like a shell would.
fn which(cmd: &str, path: &str) -> Option<PathBuf> {
    if cmd.contains('/') {
        return Some(PathBuf::from(cmd));
    }
    path.split(':').map(|d| Path::new(d).join(cmd)).find(|p| {
        use std::os::unix::fs::PermissionsExt;
        p.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    })
}

fn spawn(req: &ShellRequest, env: &BTreeMap<String, String>, spec: &GuestSpec) -> Result<Child> {
    if req.argv.is_empty() {
        bail!("no command");
    }
    let exe = which(
        &req.argv[0],
        env.get("PATH").map(String::as_str).unwrap_or(DEFAULT_PATH),
    )
    .with_context(|| format!("{}: command not found", req.argv[0]))?;
    // Everything the child needs is prepared before fork: after it, only
    // async-signal-safe calls.
    let exe = sys::cstr(&exe.to_string_lossy());
    let argv: Vec<CString> = req.argv.iter().map(|a| sys::cstr(a)).collect();
    let envp: Vec<CString> = env
        .iter()
        .map(|(k, v)| sys::cstr(&format!("{k}={v}")))
        .collect();
    let (argv_p, envp_p) = (sys::c_array(&argv), sys::c_array(&envp));
    let cwd = sys::cstr(if spec.workdir.is_empty() {
        "/"
    } else {
        &spec.workdir
    });
    let filter = sys::exec_filter();
    let drop_privs = !spec.root && !req.system;
    // Said by the child if exec fails; prepared here, as the child may only
    // make async-signal-safe calls.
    let denied = format!(
        "zapper: {}: exec denied by policy (zapper events shows the rule)\r\n",
        req.argv[0]
    );
    let failed = format!("zapper: {}: exec failed\r\n", req.argv[0]);

    let pty = match req.tty {
        Some((rows, cols)) => Some(sys::openpty(rows, cols)?),
        None => None,
    };
    let pipes = if pty.is_none() {
        Some((sys::pipe()?, sys::pipe()?, sys::pipe()?))
    } else {
        None
    };
    let fd_pair = if spec.supervise_exec && !req.system {
        let mut fds = [0; 2];
        sys::check(unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
                0,
                fds.as_mut_ptr(),
            )
        })?;
        unsafe { Some((OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1]))) }
    } else {
        None
    };
    let (stdin_fd, stdout_fd, stderr_fd) = match (&pty, &pipes) {
        (Some(p), _) => (
            p.slave.as_raw_fd(),
            p.slave.as_raw_fd(),
            p.slave.as_raw_fd(),
        ),
        (None, Some((i, o, e))) => (i.0.as_raw_fd(), o.1.as_raw_fd(), e.1.as_raw_fd()),
        _ => unreachable!(),
    };
    let child_sock = fd_pair.as_ref().map(|(_, c)| c.as_raw_fd());

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        bail!("fork: {}", std::io::Error::last_os_error());
    }
    if pid == 0 {
        unsafe {
            let fail = |code: i32| -> ! { libc::_exit(code) };
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigprocmask(libc::SIG_SETMASK, &set, std::ptr::null_mut());
            for sig in [
                libc::SIGPIPE,
                libc::SIGINT,
                libc::SIGQUIT,
                libc::SIGTERM,
                libc::SIGHUP,
                libc::SIGCHLD,
            ] {
                libc::signal(sig, libc::SIG_DFL);
            }
            libc::setsid();
            if pty.is_some() {
                libc::ioctl(stdin_fd, libc::TIOCSCTTY as _, 0);
            }
            if libc::dup2(stdin_fd, 0) < 0
                || libc::dup2(stdout_fd, 1) < 0
                || libc::dup2(stderr_fd, 2) < 0
            {
                fail(126);
            }
            if libc::chdir(cwd.as_ptr()) < 0 {
                libc::chdir(c"/".as_ptr());
            }
            if drop_privs {
                // Raw syscalls: libc wrappers may try to sync other threads.
                if libc::syscall(libc::SYS_setgroups, 0, std::ptr::null::<libc::gid_t>()) < 0
                    || libc::syscall(libc::SYS_setresgid, UID, UID, UID) < 0
                    || libc::syscall(libc::SYS_setresuid, UID, UID, UID) < 0
                {
                    fail(126);
                }
            }
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) < 0 {
                fail(126);
            }
            if let Some(sock) = child_sock {
                let fd = sys::install_filter(&filter);
                if fd < 0 || sys::send_fd(sock, fd) < 0 {
                    fail(126);
                }
                libc::close(fd);
            }
            libc::execve(exe.as_ptr(), argv_p.as_ptr(), envp_p.as_ptr());
            let msg = if *libc::__errno_location() == libc::EACCES {
                &denied
            } else {
                &failed
            };
            libc::write(2, msg.as_ptr() as *const libc::c_void, msg.len());
            fail(126);
        }
    }

    let notify = match fd_pair {
        Some((parent, child)) => {
            drop(child);
            Some(sys::recv_fd(parent.as_raw_fd()).context("receiving seccomp listener")?)
        }
        None => None,
    };
    Ok(match (pty, pipes) {
        (Some(p), _) => {
            drop(p.slave);
            let master_fd = p.master.as_raw_fd();
            let out = p.master.try_clone()?;
            Child {
                pid,
                outputs: vec![(out, false)],
                input: Some(p.master),
                master_fd: Some(master_fd),
                notify,
            }
        }
        (None, Some((i, o, e))) => Child {
            pid,
            outputs: vec![(File::from(o.0), false), (File::from(e.0), true)],
            input: Some(File::from(i.1)),
            master_fd: None,
            notify,
        },
        _ => unreachable!(),
    })
}

/// Answers seccomp notifications for one shell's process tree until no
/// process uses the filter any more.
fn supervise(listener: OwnedFd, control: Control) {
    let fd = listener.as_raw_fd();
    let listener = Arc::new(listener);
    loop {
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        if unsafe { libc::poll(&mut pfd, 1, -1) } < 0 {
            continue;
        }
        if pfd.revents & libc::POLLHUP != 0 {
            return;
        }
        let Ok(n) = sys::notif_recv(fd) else { continue };
        let (control, listener) = (control.clone(), listener.clone());
        std::thread::spawn(move || {
            let fd = listener.as_raw_fd();
            let errno = match read_exec(&n) {
                Some(req) if sys::notif_valid(fd, n.id) => {
                    let uid = proc_uid(n.pid).unwrap_or(u32::MAX);
                    if control.check_exec(n.pid as i32, uid, req) {
                        0
                    } else {
                        libc::EACCES
                    }
                }
                _ => libc::EACCES,
            };
            let _ = sys::notif_send(fd, n.id, errno);
        });
    }
}

/// Reads the exec target and arguments from the stopped process.
fn read_exec(n: &sys::SeccompNotif) -> Option<ExecRequest> {
    let pid = n.pid;
    let mem = File::open(format!("/proc/{pid}/mem")).ok()?;
    let a = n.data.args;
    let (dirfd, path_ptr, argv_ptr, flags) = if n.data.nr as i64 == libc::SYS_execve {
        (libc::AT_FDCWD as i64, a[0], a[1], 0)
    } else {
        (a[0] as i32 as i64, a[1], a[2], a[4] as i32)
    };
    let raw = sys::read_cstring(&mem, path_ptr, 4096).ok()?;
    let mut argv = Vec::new();
    let mut total = 0;
    for i in 0..1024u64 {
        let p = sys::read_u64(&mem, argv_ptr + i * 8).ok()?;
        if p == 0 {
            break;
        }
        let s = sys::read_cstring(&mem, p, 4096).ok()?;
        total += s.len();
        argv.push(s);
        if total > 64 * 1024 {
            argv.push("…".into());
            break;
        }
    }
    let cwd = std::fs::read_link(format!("/proc/{pid}/cwd")).ok();
    let base = if dirfd == libc::AT_FDCWD as i64 {
        cwd.clone()
    } else {
        std::fs::read_link(format!("/proc/{pid}/fd/{dirfd}")).ok()
    };
    let path = if raw.is_empty() && flags & libc::AT_EMPTY_PATH != 0 {
        base.unwrap_or_default()
    } else if raw.starts_with('/') {
        PathBuf::from(&raw)
    } else {
        base.unwrap_or_default().join(&raw)
    };
    let path = std::fs::canonicalize(&path).unwrap_or(path);
    Some(ExecRequest {
        path: path.to_string_lossy().into_owned(),
        argv,
        cwd: cwd.map(|c| c.to_string_lossy().into_owned()),
    })
}

fn proc_uid(pid: u32) -> Option<u32> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status.lines().find_map(|l| {
        l.strip_prefix("Uid:")?
            .split_whitespace()
            .next()?
            .parse()
            .ok()
    })
}
