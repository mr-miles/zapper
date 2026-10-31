//! Persistent terminals: a holder process per terminal keeps its pty, and
//! the latest of its output, while no host connection is attached, so its
//! program keeps running when the host goes away; the host attaches again by
//! the terminal's id, and is sent what it missed.
//!
//! PID 1 starts the program as for any terminal (privileges, the exec
//! filter, reaping) and hands the holder the pty master and each host
//! connection over a `SOCK_SEQPACKET` socketpair, a message each. The holder
//! is this binary (`zapper-agent hold`), as root with `no_new_privs`, in a
//! session of its own: it never forks or execs, and a bug in it ends its
//! terminal, not the VM.

use std::collections::{BTreeMap, VecDeque};
use std::ffi::CString;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use zapper_proto::proto::{Frame, GuestEvent, MAX_FRAME, MAX_PERSISTENT_TERMINALS};

use crate::control::Control;
use crate::init::wait_child;
use crate::sys;

/// Output kept for a host that attaches later.
const KEPT: usize = 256 << 10;
/// How long a holder whose program has ended waits for a host to attach and
/// be told so.
const AFTER_EXIT: Duration = Duration::from_secs(24 * 3600);
/// The fd a holder is given its socket to PID 1 on.
const SOCKET_FD: RawFd = 3;

/// What PID 1 tells a holder, a message each.
#[derive(Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Msg {
    /// With the pty master: the program and the terminal's size.
    Start { pid: i32, rows: u16, cols: u16 },
    /// With a host connection: attach it, at the attaching terminal's size
    /// if it gives one (a host that only hangs up doesn't).
    Attach { size: Option<(u16, u16)> },
    /// The program exited.
    Exit { code: i32 },
}

// PID 1's side.

struct Holder {
    sock: OwnedFd,
}

fn holders() -> &'static Mutex<BTreeMap<u64, Holder>> {
    static H: OnceLock<Mutex<BTreeMap<u64, Holder>>> = OnceLock::new();
    H.get_or_init(Default::default)
}

fn send(sock: RawFd, msg: &Msg, fd: Option<RawFd>) -> io::Result<()> {
    let data = serde_json::to_vec(msg).map_err(io::Error::other)?;
    sys::send_msg(sock, &data, fd)
}

/// Starts terminal `id`'s holder for the program `pid` on pty `master`, and
/// attaches `conn`, its first connection. Forwards the program's exit to it,
/// and reports it to the host as any terminal's.
pub fn start(
    id: u64,
    pid: i32,
    master: File,
    size: (u16, u16),
    conn: &File,
    control: &Control,
) -> Result<()> {
    if holders().lock().unwrap().len() >= MAX_PERSISTENT_TERMINALS {
        bail!("{MAX_PERSISTENT_TERMINALS} persistent terminals are open; end one first");
    }
    let (ours, theirs) = sys::seqpacket_pair()?;
    let holder = spawn(&theirs).context("starting the terminal's holder")?;
    drop(theirs);
    let sock = ours.as_raw_fd();
    send(
        sock,
        &Msg::Start {
            pid,
            rows: size.0,
            cols: size.1,
        },
        Some(master.as_raw_fd()),
    )?;
    send(
        sock,
        &Msg::Attach { size: Some(size) },
        Some(conn.as_raw_fd()),
    )?;
    // The holder has its own now.
    drop(master);
    holders().lock().unwrap().insert(id, Holder { sock: ours });
    std::thread::spawn(move || {
        let _ = wait_child(holder).recv();
        holders().lock().unwrap().remove(&id);
    });
    let control = control.clone();
    std::thread::spawn(move || {
        let code = wait_child(pid).recv().unwrap_or(255);
        if let Some(h) = holders().lock().unwrap().get(&id) {
            let _ = send(h.sock.as_raw_fd(), &Msg::Exit { code }, None);
        }
        control.event(GuestEvent::ShellExit {
            id,
            exit_code: code,
        });
    });
    Ok(())
}

/// The persistent terminals open, for a host connecting again.
pub fn ids() -> Vec<u64> {
    holders().lock().unwrap().keys().copied().collect()
}

/// Hands `conn` to terminal `id`'s holder; it takes over from a connection
/// attached already.
pub fn attach(id: u64, conn: &File, size: Option<(u16, u16)>) -> Result<()> {
    let h = holders().lock().unwrap();
    let Some(h) = h.get(&id) else {
        bail!("no persistent terminal {id}");
    };
    send(
        h.sock.as_raw_fd(),
        &Msg::Attach { size },
        Some(conn.as_raw_fd()),
    )?;
    Ok(())
}

/// Forks and execs this binary as `hold`, with `sock` as its fd 3, in a
/// session of its own and with `no_new_privs`.
fn spawn(sock: &OwnedFd) -> Result<i32> {
    // All prepared before the fork: after it, async-signal-safe calls only.
    let exe = sys::cstr("/proc/self/exe");
    let argv: Vec<CString> = ["zapper-agent", "hold"]
        .iter()
        .map(|a| sys::cstr(a))
        .collect();
    let envp: Vec<CString> = vec![];
    let (argv_p, envp_p) = (sys::c_array(&argv), sys::c_array(&envp));
    let fd = sock.as_raw_fd();
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        bail!("fork: {}", io::Error::last_os_error());
    }
    if pid == 0 {
        unsafe {
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigprocmask(libc::SIG_SETMASK, &set, std::ptr::null_mut());
            libc::setsid();
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) < 0
                || libc::dup2(fd, SOCKET_FD) < 0
            {
                libc::_exit(126);
            }
            // Nothing else of PID 1's, whether or not it is close-on-exec.
            libc::syscall(libc::SYS_close_range, SOCKET_FD + 1, u32::MAX, 0);
            libc::execve(exe.as_ptr(), argv_p.as_ptr(), envp_p.as_ptr());
            libc::_exit(127);
        }
    }
    Ok(pid)
}

// The holder's side.

struct State {
    /// The latest output, and where in all of it it starts.
    kept: VecDeque<u8>,
    start: u64,
    /// The host connection attached, if any, and its generation, so a
    /// connection taken over doesn't detach its successor.
    conn: Option<(u64, File)>,
    generation: u64,
    size: (u16, u16),
    /// The program's exit code, once it has exited.
    exited: Option<i32>,
    /// The pty gave its last output.
    drained: bool,
}

impl State {
    fn end(&self) -> u64 {
        self.start + self.kept.len() as u64
    }

    fn keep(&mut self, data: &[u8]) {
        self.kept.extend(data);
        let over = self.kept.len().saturating_sub(KEPT);
        self.kept.drain(..over);
        self.start += over as u64;
    }

    /// Sends `frame` to the attached connection; on failure it detaches.
    fn send(&mut self, frame: &Frame) {
        if let Some((_, conn)) = self.conn.as_mut() {
            let ok = conn.write_all(&frame.encode()).is_ok();
            if !ok {
                self.detach();
            }
        }
    }

    fn detach(&mut self) {
        if let Some((_, conn)) = self.conn.take() {
            unsafe { libc::shutdown(conn.as_raw_fd(), libc::SHUT_RDWR) };
        }
    }

    /// Whether the terminal is over and the attached host has been told.
    fn finished(&self) -> bool {
        self.exited.is_some() && self.drained
    }
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    master: File,
    pid: i32,
    /// The holder's work is done: the process exits (a test notes it).
    done: Box<dyn Fn() + Send + Sync>,
}

/// `zapper-agent hold`: a holder's life, on the socket PID 1 gave it.
pub fn run() -> i32 {
    let sock = unsafe { OwnedFd::from_raw_fd(SOCKET_FD) };
    match hold(sock, Box::new(|| std::process::exit(0))) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("zapper-agent hold: {e:#}");
            1
        }
    }
}

fn hold(sock: OwnedFd, done: Box<dyn Fn() + Send + Sync>) -> Result<()> {
    let fd = sock.as_raw_fd();
    let recv = || -> Result<Option<(Msg, Option<OwnedFd>)>> {
        match sys::recv_msg(fd, 4096)? {
            None => Ok(None),
            Some((data, file)) => Ok(Some((serde_json::from_slice(&data)?, file))),
        }
    };
    let Some((Msg::Start { pid, rows, cols }, Some(master))) = recv()? else {
        bail!("expected the terminal first");
    };
    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            kept: VecDeque::new(),
            start: 0,
            conn: None,
            generation: 0,
            size: (rows, cols),
            exited: None,
            drained: false,
        }),
        changed: Condvar::new(),
        master: File::from(master),
        pid,
        done,
    });
    {
        let s = shared.clone();
        std::thread::spawn(move || pump(&s));
    }
    loop {
        match recv()? {
            Some((Msg::Attach { size }, Some(conn))) => {
                attach_conn(&shared, File::from(conn), size)
            }
            Some((Msg::Exit { code }, _)) => {
                let mut st = shared.state.lock().unwrap();
                st.exited = Some(code);
                // Output still in the pty, or held by a background job, gets
                // half a second, as on any terminal.
                let (st, _) = shared
                    .changed
                    .wait_timeout_while(st, Duration::from_millis(500), |st| !st.drained)
                    .unwrap();
                let mut st = st;
                st.drained = true;
                finish_if_told(&shared, &mut st);
                if st.conn.is_none() {
                    // Nobody attached to be told: wait a while for someone.
                    let s = shared.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(AFTER_EXIT);
                        (s.done)();
                    });
                }
            }
            Some(_) => {}
            // PID 1 is gone: so is the VM.
            None => return Ok(()),
        }
    }
}

/// Once the program has exited and its output is out: tells the attached
/// host, and the holder's work is done.
fn finish_if_told(s: &Shared, st: &mut State) {
    if !st.finished() {
        return;
    }
    if let (Some(code), true) = (st.exited, st.conn.is_some()) {
        st.send(&Frame::Exit(code));
        st.detach();
        (s.done)();
    }
}

/// The pty's output into what is kept, and to the attached host.
fn pump(s: &Shared) {
    let mut master = &s.master;
    let mut buf = vec![0u8; MAX_FRAME];
    loop {
        match master.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let mut st = s.state.lock().unwrap();
                st.keep(&buf[..n]);
                st.send(&Frame::Data(buf[..n].to_vec()));
            }
        }
    }
    let mut st = s.state.lock().unwrap();
    st.drained = true;
    s.changed.notify_all();
    finish_if_told(s, &mut st);
}

/// A host attaches: told where its output starts, sent what is kept, then
/// live output; the one attached before is closed. The program redraws at
/// the new terminal's size.
fn attach_conn(s: &Arc<Shared>, conn: File, size: Option<(u16, u16)>) {
    sys::set_send_timeout(conn.as_raw_fd(), 10);
    let Ok(reader) = conn.try_clone() else {
        return;
    };
    let generation = {
        let mut st = s.state.lock().unwrap();
        st.detach();
        st.generation += 1;
        let generation = st.generation;
        st.conn = Some((generation, conn));
        let (from, to) = (st.start, st.end());
        st.send(&Frame::Attached { from, to });
        let kept: Vec<u8> = st.kept.iter().copied().collect();
        for chunk in kept.chunks(MAX_FRAME) {
            st.send(&Frame::Data(chunk.to_vec()));
        }
        // A full-screen program redraws when its size changes: make it
        // change, even to the size it had.
        if let Some(size) = size {
            let fd = s.master.as_raw_fd();
            if st.size == size && size.0 > 1 {
                sys::set_winsize(fd, size.0 - 1, size.1);
            }
            sys::set_winsize(fd, size.0, size.1);
            st.size = size;
        }
        finish_if_told(s, &mut st);
        generation
    };
    let s = s.clone();
    std::thread::spawn(move || input(&s, reader, generation));
}

/// A connection's frames to the terminal, until it closes: then, unless it
/// was taken over, the holder detaches and the program keeps running.
fn input(s: &Shared, mut conn: File, generation: u64) {
    let mut master = &s.master;
    loop {
        match Frame::read_from(&mut conn) {
            Ok(Some(Frame::Data(d))) => {
                let _ = master.write_all(&d);
            }
            Ok(Some(Frame::Resize(rows, cols))) => {
                sys::set_winsize(s.master.as_raw_fd(), rows, cols);
                s.state.lock().unwrap().size = (rows, cols);
            }
            Ok(Some(Frame::Eof)) => {
                let _ = master.write_all(&[4]);
            }
            Ok(Some(Frame::Hangup)) => unsafe {
                libc::kill(-s.pid, libc::SIGHUP);
            },
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break,
        }
    }
    let mut st = s.state.lock().unwrap();
    if st.conn.as_ref().is_some_and(|(g, _)| *g == generation) {
        st.conn = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Frames from `conn` until `pred` holds for the ones read, or 5 s.
    fn read_until(conn: &mut UnixStream, pred: impl Fn(&[Frame]) -> bool) -> Vec<Frame> {
        conn.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut got = Vec::new();
        while !pred(&got) {
            match Frame::read_from(conn) {
                Ok(Some(f)) => got.push(f),
                _ => break,
            }
        }
        got
    }

    fn output(frames: &[Frame]) -> Vec<u8> {
        frames
            .iter()
            .filter_map(|f| match f {
                Frame::Data(d) => Some(d.clone()),
                _ => None,
            })
            .flatten()
            .collect()
    }

    fn has(frames: &[Frame], text: &str) -> bool {
        String::from_utf8_lossy(&output(frames)).contains(text)
    }

    /// A holder takes nothing before its terminal: a connection first is
    /// refused, and it ends.
    #[test]
    fn a_holder_takes_its_terminal_first() {
        let (ours, theirs) = sys::seqpacket_pair().unwrap();
        let (_host, guest) = UnixStream::pair().unwrap();
        send(
            ours.as_raw_fd(),
            &Msg::Attach { size: None },
            Some(guest.as_raw_fd()),
        )
        .unwrap();
        let e = hold(theirs, Box::new(|| {})).unwrap_err();
        assert!(e.to_string().contains("terminal first"), "{e}");
    }

    /// A second terminal sent to a holder is ignored: what is attached is
    /// the first one's output only.
    #[test]
    fn a_holder_holds_one_terminal() {
        let (first, other) = (sys::openpty(24, 80).unwrap(), sys::openpty(24, 80).unwrap());
        let (mut program, mut intruder) = (File::from(first.slave), File::from(other.slave));
        let (ours, theirs) = sys::seqpacket_pair().unwrap();
        std::thread::spawn(move || {
            let _ = hold(theirs, Box::new(|| {}));
        });
        let sock = ours.as_raw_fd();
        for master in [&first.master, &other.master] {
            let start = Msg::Start {
                pid: i32::MAX,
                rows: 24,
                cols: 80,
            };
            send(sock, &start, Some(master.as_raw_fd())).unwrap();
        }
        let (mut host, guest) = UnixStream::pair().unwrap();
        send(sock, &Msg::Attach { size: None }, Some(guest.as_raw_fd())).unwrap();
        intruder.write_all(b"intruder\n").unwrap();
        program.write_all(b"program\n").unwrap();
        let f = read_until(&mut host, |f| has(f, "program"));
        assert!(has(&f, "program"), "{f:?}");
        assert!(!has(&f, "intruder"), "{f:?}");
    }

    /// A holder in this process, on a real pty whose other side the test
    /// writes as the program would: it keeps output while nobody is
    /// attached, replays it to the next host, passes input on, and tells the
    /// attached host when the program exits.
    #[test]
    fn a_terminal_outlives_its_connection() {
        let pty = sys::openpty(24, 80).unwrap();
        let mut program = File::from(pty.slave);
        let (ours, theirs) = sys::seqpacket_pair().unwrap();
        let done = Arc::new(AtomicBool::new(false));
        let d = done.clone();
        std::thread::spawn(move || {
            let _ = hold(theirs, Box::new(move || d.store(true, Ordering::SeqCst)));
        });
        let sock = ours.as_raw_fd();
        let attach = |rows| {
            let (host, guest) = UnixStream::pair().unwrap();
            send(
                sock,
                &Msg::Attach {
                    size: Some((rows, 80)),
                },
                Some(guest.as_raw_fd()),
            )
            .unwrap();
            host
        };
        // A pid of no process: the holder signals it only on a hangup.
        send(
            sock,
            &Msg::Start {
                pid: i32::MAX,
                rows: 24,
                cols: 80,
            },
            Some(pty.master.as_raw_fd()),
        )
        .unwrap();

        let mut first = attach(24);
        let f = read_until(&mut first, |f| !f.is_empty());
        assert_eq!(f[0], Frame::Attached { from: 0, to: 0 });
        program.write_all(b"one\n").unwrap();
        assert!(has(&read_until(&mut first, |f| has(f, "one")), "one"));

        // The host goes away; the program goes on writing.
        drop(first);
        std::thread::sleep(Duration::from_millis(100));
        program.write_all(b"two\n").unwrap();
        std::thread::sleep(Duration::from_millis(100));

        // The next host is told where the output starts, and gets it all.
        let mut second = attach(30);
        let f = read_until(&mut second, |f| has(f, "two"));
        assert!(
            matches!(f[0], Frame::Attached { from: 0, to } if to > 0),
            "{f:?}"
        );
        assert!(has(&f, "one") && has(&f, "two"), "{f:?}");
        // Its keystrokes reach the program.
        // A whole line: the pty passes input on by line.
        Frame::Data(b"typed\n".to_vec())
            .write_to(&mut second)
            .unwrap();
        let mut buf = [0u8; 64];
        let mut seen = Vec::new();
        program
            .try_clone()
            .unwrap()
            .read(&mut buf)
            .map(|n| seen.extend_from_slice(&buf[..n]))
            .unwrap();
        assert!(String::from_utf8_lossy(&seen).contains("typed"), "{seen:?}");

        // A third takes over: the second is closed.
        let mut third = attach(30);
        read_until(&mut third, |f| !f.is_empty());
        // What it had been sent (the line's echo), then the end of it.
        read_until(&mut second, |_| false);
        assert!(matches!(Frame::read_from(&mut second), Ok(None)));

        // The program exits: the attached host is told, and the holder is done.
        drop(program);
        send(sock, &Msg::Exit { code: 3 }, None).unwrap();
        let f = read_until(&mut third, |f| f.contains(&Frame::Exit(3)));
        assert!(f.contains(&Frame::Exit(3)), "{f:?}");
        std::thread::sleep(Duration::from_millis(50));
        assert!(done.load(Ordering::SeqCst));
    }

    #[test]
    fn what_is_kept_is_the_latest() {
        let mut st = State {
            kept: VecDeque::new(),
            start: 0,
            conn: None,
            generation: 0,
            size: (24, 80),
            exited: None,
            drained: false,
        };
        st.keep(&vec![b'a'; KEPT]);
        st.keep(b"tail");
        assert_eq!(st.kept.len(), KEPT);
        assert_eq!(st.start, 4);
        assert_eq!(st.end(), KEPT as u64 + 4);
        assert!(st.kept.iter().rev().take(4).eq(b"liat".iter()));
    }
}
