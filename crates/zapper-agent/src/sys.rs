//! Thin wrappers over the syscalls the agent needs.

use std::ffi::CString;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;

pub fn check(ret: libc::c_int) -> io::Result<libc::c_int> {
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(ret)
    }
}

pub fn cstr(s: &str) -> CString {
    CString::new(s.replace('\0', "")).unwrap()
}

/// Opens a vsock stream to `cid:port`.
pub fn vsock_connect(cid: u32, port: u32) -> io::Result<File> {
    unsafe {
        let fd = check(libc::socket(
            libc::AF_VSOCK,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
            0,
        ))?;
        let fd = OwnedFd::from_raw_fd(fd);
        let addr = vsock_addr(cid, port);
        check(libc::connect(
            fd.as_raw_fd(),
            &addr as *const _ as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_vm>() as u32,
        ))?;
        Ok(File::from(fd))
    }
}

fn vsock_addr(cid: u32, port: u32) -> libc::sockaddr_vm {
    let mut addr: libc::sockaddr_vm = unsafe { std::mem::zeroed() };
    addr.svm_family = libc::AF_VSOCK as u16;
    addr.svm_cid = cid;
    addr.svm_port = port;
    addr
}

pub struct VsockListener(OwnedFd);

impl VsockListener {
    pub fn bind(port: u32) -> io::Result<Self> {
        unsafe {
            let fd = check(libc::socket(
                libc::AF_VSOCK,
                libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
                0,
            ))?;
            let fd = OwnedFd::from_raw_fd(fd);
            let addr = vsock_addr(libc::VMADDR_CID_ANY, port);
            check(libc::bind(
                fd.as_raw_fd(),
                &addr as *const _ as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_vm>() as u32,
            ))?;
            check(libc::listen(fd.as_raw_fd(), 16))?;
            Ok(Self(fd))
        }
    }

    /// Accepts a connection, returning it with the peer's CID.
    pub fn accept(&self) -> io::Result<(File, u32)> {
        unsafe {
            let mut addr: libc::sockaddr_vm = std::mem::zeroed();
            let mut len = std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t;
            let fd = check(libc::accept4(
                self.0.as_raw_fd(),
                &mut addr as *mut _ as *mut libc::sockaddr,
                &mut len,
                libc::SOCK_CLOEXEC,
            ))?;
            Ok((File::from_raw_fd(fd), addr.svm_cid))
        }
    }
}

pub fn mount(
    src: &str,
    target: &str,
    fstype: &str,
    flags: libc::c_ulong,
    data: &str,
) -> io::Result<()> {
    let _ = std::fs::create_dir_all(target);
    let (s, t, f, d) = (cstr(src), cstr(target), cstr(fstype), cstr(data));
    let data_ptr = if data.is_empty() {
        std::ptr::null()
    } else {
        d.as_ptr() as *const libc::c_void
    };
    let fstype_ptr = if fstype.is_empty() {
        std::ptr::null()
    } else {
        f.as_ptr()
    };
    check(unsafe { libc::mount(s.as_ptr(), t.as_ptr(), fstype_ptr, flags, data_ptr) })
        .map(drop)
        .map_err(|e| io::Error::new(e.kind(), format!("mount {target}: {e}")))
}

/// Bind-mounts `src` over `target`, read-only if asked.
pub fn bind(src: &str, target: &str, read_only: bool) -> io::Result<()> {
    mount(src, target, "", libc::MS_BIND, "")?;
    if read_only {
        mount(
            "",
            target,
            "",
            libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY,
            "",
        )?;
    }
    Ok(())
}

/// Brings the loopback interface up.
pub fn loopback_up() -> io::Result<()> {
    unsafe {
        let fd = check(libc::socket(
            libc::AF_INET,
            libc::SOCK_DGRAM | libc::SOCK_CLOEXEC,
            0,
        ))?;
        let fd = OwnedFd::from_raw_fd(fd);
        let mut req: libc::ifreq = std::mem::zeroed();
        for (i, b) in b"lo".iter().enumerate() {
            req.ifr_name[i] = *b as libc::c_char;
        }
        check(libc::ioctl(
            fd.as_raw_fd(),
            libc::SIOCGIFFLAGS as _,
            &mut req,
        ))?;
        req.ifr_ifru.ifru_flags |= (libc::IFF_UP | libc::IFF_RUNNING) as i16;
        check(libc::ioctl(fd.as_raw_fd(), libc::SIOCSIFFLAGS as _, &req))?;
        Ok(())
    }
}

pub struct Pty {
    pub master: File,
    pub slave: OwnedFd,
}

pub fn openpty(rows: u16, cols: u16) -> io::Result<Pty> {
    let mut master = 0;
    let mut slave = 0;
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    unsafe {
        check(libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            &ws,
        ))?;
        libc::fcntl(master, libc::F_SETFD, libc::FD_CLOEXEC);
        libc::fcntl(slave, libc::F_SETFD, libc::FD_CLOEXEC);
        Ok(Pty {
            master: File::from_raw_fd(master),
            slave: OwnedFd::from_raw_fd(slave),
        })
    }
}

pub fn set_winsize(fd: RawFd, rows: u16, cols: u16) {
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    unsafe { libc::ioctl(fd, libc::TIOCSWINSZ as _, &ws) };
}

pub fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    check(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) })?;
    unsafe { Ok((OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1]))) }
}

/// Sends one fd over a unix socket. Async-signal-safe.
pub unsafe fn send_fd(sock: RawFd, fd: RawFd) -> libc::c_int {
    let mut byte = 0u8;
    let mut iov = libc::iovec {
        iov_base: &mut byte as *mut u8 as *mut libc::c_void,
        iov_len: 1,
    };
    let mut cmsg_buf = [0u64; 4];
    let mut msg: libc::msghdr = std::mem::zeroed();
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = libc::CMSG_SPACE(4) as _;
    let cmsg = libc::CMSG_FIRSTHDR(&msg);
    (*cmsg).cmsg_level = libc::SOL_SOCKET;
    (*cmsg).cmsg_type = libc::SCM_RIGHTS;
    (*cmsg).cmsg_len = libc::CMSG_LEN(4) as _;
    *(libc::CMSG_DATA(cmsg) as *mut libc::c_int) = fd;
    libc::sendmsg(sock, &msg, 0) as libc::c_int
}

pub fn recv_fd(sock: RawFd) -> io::Result<OwnedFd> {
    unsafe {
        let mut byte = 0u8;
        let mut iov = libc::iovec {
            iov_base: &mut byte as *mut u8 as *mut libc::c_void,
            iov_len: 1,
        };
        let mut cmsg_buf = [0u64; 4];
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = libc::CMSG_SPACE(4) as _;
        if libc::recvmsg(sock, &mut msg, libc::MSG_CMSG_CLOEXEC) <= 0 {
            return Err(io::Error::other("no fd received"));
        }
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null() || (*cmsg).cmsg_type != libc::SCM_RIGHTS {
            return Err(io::Error::other("no fd received"));
        }
        Ok(OwnedFd::from_raw_fd(
            *(libc::CMSG_DATA(cmsg) as *const libc::c_int),
        ))
    }
}

/// A pair of connected `SOCK_SEQPACKET` sockets: each message arrives whole,
/// with the fd sent with it.
pub fn seqpacket_pair() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    check(unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
            0,
            fds.as_mut_ptr(),
        )
    })?;
    unsafe { Ok((OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1]))) }
}

/// Sends one message on a `SOCK_SEQPACKET` socket, with `fd` if given.
pub fn send_msg(sock: RawFd, data: &[u8], fd: Option<RawFd>) -> io::Result<()> {
    unsafe {
        let mut iov = libc::iovec {
            iov_base: data.as_ptr() as *mut libc::c_void,
            iov_len: data.len(),
        };
        let mut cmsg_buf = [0u64; 4];
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        if let Some(fd) = fd {
            msg.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
            msg.msg_controllen = libc::CMSG_SPACE(4) as _;
            let cmsg = libc::CMSG_FIRSTHDR(&msg);
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN(4) as _;
            *(libc::CMSG_DATA(cmsg) as *mut libc::c_int) = fd;
        }
        if libc::sendmsg(sock, &msg, libc::MSG_NOSIGNAL) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Receives one message of at most `max` bytes from a `SOCK_SEQPACKET`
/// socket, and the fd sent with it; `None` when the other end has closed.
pub fn recv_msg(sock: RawFd, max: usize) -> io::Result<Option<(Vec<u8>, Option<OwnedFd>)>> {
    let mut data = vec![0u8; max];
    unsafe {
        let mut iov = libc::iovec {
            iov_base: data.as_mut_ptr() as *mut libc::c_void,
            iov_len: data.len(),
        };
        let mut cmsg_buf = [0u64; 4];
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = libc::CMSG_SPACE(4) as _;
        let n = libc::recvmsg(sock, &mut msg, libc::MSG_CMSG_CLOEXEC);
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        if n == 0 {
            return Ok(None);
        }
        data.truncate(n as usize);
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        let fd = (!cmsg.is_null() && (*cmsg).cmsg_type == libc::SCM_RIGHTS)
            .then(|| OwnedFd::from_raw_fd(*(libc::CMSG_DATA(cmsg) as *const libc::c_int)));
        Ok(Some((data, fd)))
    }
}

/// Makes a write to socket `fd` give up after `secs` seconds.
pub fn set_send_timeout(fd: RawFd, secs: i64) {
    let tv = libc::timeval {
        tv_sec: secs,
        tv_usec: 0,
    };
    unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_SNDTIMEO,
            &tv as *const libc::timeval as *const libc::c_void,
            std::mem::size_of::<libc::timeval>() as libc::socklen_t,
        );
    }
}

// seccomp user notification.

const SECCOMP_SET_MODE_FILTER: libc::c_uint = 1;
const SECCOMP_FILTER_FLAG_NEW_LISTENER: libc::c_ulong = 1 << 3;
const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
const SECCOMP_RET_USER_NOTIF: u32 = 0x7fc0_0000;
const SECCOMP_RET_KILL_PROCESS: u32 = 0x8000_0000;
pub const SECCOMP_USER_NOTIF_FLAG_CONTINUE: u32 = 1;
const IOCTL_NOTIF_RECV: libc::c_ulong = 0xc050_2100;
const IOCTL_NOTIF_SEND: libc::c_ulong = 0xc018_2101;
const IOCTL_NOTIF_ID_VALID: libc::c_ulong = 0x4008_2102;

/// AUDIT_ARCH_* of the native syscall table.
#[cfg(target_arch = "x86_64")]
const ARCH: u32 = 0xc000_003e;
#[cfg(target_arch = "aarch64")]
const ARCH: u32 = 0xc000_00b7;

/// BPF program: notify on execve/execveat, kill on a foreign arch or (on
/// x86_64) an x32 syscall number, so neither can be used to dodge it, and
/// allow the rest.
pub fn exec_filter() -> Vec<libc::sock_filter> {
    const LD_W_ABS: u16 = 0x20;
    const JEQ_K: u16 = 0x15;
    const JGE_K: u16 = 0x35;
    const RET_K: u16 = 0x06;
    let op = |code, jt, jf, k| libc::sock_filter { code, jt, jf, k };
    let mut prog = vec![
        op(LD_W_ABS, 0, 0, 4), // seccomp_data.arch
        op(JEQ_K, 1, 0, ARCH),
        op(RET_K, 0, 0, SECCOMP_RET_KILL_PROCESS),
        op(LD_W_ABS, 0, 0, 0), // seccomp_data.nr
    ];
    if cfg!(target_arch = "x86_64") {
        prog.push(op(JGE_K, 4, 0, 0x4000_0000)); // x32 → kill
    }
    prog.extend([
        op(JEQ_K, 2, 0, libc::SYS_execve as u32),
        op(JEQ_K, 1, 0, libc::SYS_execveat as u32),
        op(RET_K, 0, 0, SECCOMP_RET_ALLOW),
        op(RET_K, 0, 0, SECCOMP_RET_USER_NOTIF),
        op(RET_K, 0, 0, SECCOMP_RET_KILL_PROCESS),
    ]);
    prog
}

/// Installs `prog` and returns the listener fd. Async-signal-safe.
pub unsafe fn install_filter(prog: &[libc::sock_filter]) -> libc::c_int {
    let fprog = libc::sock_fprog {
        len: prog.len() as u16,
        filter: prog.as_ptr() as *mut _,
    };
    libc::syscall(
        libc::SYS_seccomp,
        SECCOMP_SET_MODE_FILTER,
        SECCOMP_FILTER_FLAG_NEW_LISTENER,
        &fprog,
    ) as libc::c_int
}

#[repr(C)]
#[derive(Default)]
pub struct SeccompData {
    pub nr: i32,
    pub arch: u32,
    pub ip: u64,
    pub args: [u64; 6],
}

#[repr(C)]
#[derive(Default)]
pub struct SeccompNotif {
    pub id: u64,
    pub pid: u32,
    pub flags: u32,
    pub data: SeccompData,
}

#[repr(C)]
struct SeccompNotifResp {
    id: u64,
    val: i64,
    error: i32,
    flags: u32,
}

pub fn notif_recv(fd: RawFd) -> io::Result<SeccompNotif> {
    let mut n = SeccompNotif::default();
    check(unsafe { libc::ioctl(fd, IOCTL_NOTIF_RECV as _, &mut n) })?;
    Ok(n)
}

pub fn notif_valid(fd: RawFd, id: u64) -> bool {
    unsafe { libc::ioctl(fd, IOCTL_NOTIF_ID_VALID as _, &id) == 0 }
}

/// Lets the syscall run (`errno` 0) or fails it with `errno`.
pub fn notif_send(fd: RawFd, id: u64, errno: i32) -> io::Result<()> {
    let resp = SeccompNotifResp {
        id,
        val: 0,
        error: -errno,
        flags: if errno == 0 {
            SECCOMP_USER_NOTIF_FLAG_CONTINUE
        } else {
            0
        },
    };
    check(unsafe { libc::ioctl(fd, IOCTL_NOTIF_SEND as _, &resp) }).map(drop)
}

/// Reads a NUL-terminated string from another process's memory.
pub fn read_cstring(mem: &File, addr: u64, max: usize) -> io::Result<String> {
    use std::os::unix::fs::FileExt;
    let mut out = Vec::new();
    let mut buf = [0u8; 256];
    let mut at = addr;
    while out.len() < max {
        let n = mem.read_at(&mut buf, at)?;
        if n == 0 {
            break;
        }
        if let Some(end) = buf[..n].iter().position(|&b| b == 0) {
            out.extend_from_slice(&buf[..end]);
            return Ok(String::from_utf8_lossy(&out).into_owned());
        }
        out.extend_from_slice(&buf[..n]);
        at += n as u64;
    }
    out.truncate(max);
    Ok(String::from_utf8_lossy(&out).into_owned())
}

pub fn read_u64(mem: &File, addr: u64) -> io::Result<u64> {
    use std::os::unix::fs::FileExt;
    let mut buf = [0u8; 8];
    mem.read_exact_at(&mut buf, addr)?;
    Ok(u64::from_ne_bytes(buf))
}

pub fn sethostname(name: &str) {
    unsafe { libc::sethostname(name.as_ptr() as *const libc::c_char, name.len()) };
}

pub fn acct(path: &str) -> io::Result<()> {
    let p = cstr(path);
    check(unsafe { libc::acct(p.as_ptr()) }).map(drop)
}

/// Ends the VM. Firecracker has no power-off device: a powered-off guest
/// just halts and the VMM keeps running. A reboot with `reboot=k` on the
/// kernel command line makes Firecracker exit at once.
pub fn power_off() -> ! {
    unsafe {
        libc::sync();
        libc::reboot(libc::RB_AUTOBOOT);
    }
    std::process::exit(0)
}

pub fn chown(path: &Path, uid: u32, gid: u32) {
    let p = cstr(&path.to_string_lossy());
    unsafe { libc::lchown(p.as_ptr(), uid, gid) };
}

pub fn c_array(items: &[CString]) -> Vec<*const libc::c_char> {
    items
        .iter()
        .map(|s| s.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect()
}
