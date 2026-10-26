//! The guest's network, inside the VMM's network namespace (`crate::vmm`).
//!
//! The namespace has two interfaces, loopback and `tap0`, the guest's NIC,
//! and no routes to the host's network. nftables there redirects each TCP
//! connection from the guest to a local listener and each DNS query to
//! another, and drops the rest of the guest's traffic. Both listeners hand
//! their traffic to the daemon over `nic.sock` in the VMM's directory,
//! where policy applies. The capture runs outside the VM, so root in the
//! guest can't get around it.

use std::io::{self, Read, Write};
use std::net::{
    Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6, TcpListener, TcpStream, UdpSocket,
};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context, Result};

pub const GATEWAY: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 1);
pub const GUEST: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);
/// The point-to-point IPv6 link (ULA), the counterpart of GATEWAY/GUEST. Used
/// only when the host and guest kernels have IPv6; otherwise the datapath
/// stays IPv4-only.
pub const GATEWAY6: Ipv6Addr = Ipv6Addr::new(0xfd00, 0x2, 0, 0, 0, 0, 0, 1);
pub const GUEST6: Ipv6Addr = Ipv6Addr::new(0xfd00, 0x2, 0, 0, 0, 0, 0, 2);
pub const GUEST_MAC: &str = "06:00:0a:00:02:02";
pub const TAP: &str = "tap0";
const TCP_PORT: u16 = 15001;
const DNS_PORT: u16 = 15053;
/// Daemon socket in the VMM's directory.
pub const SOCKET: &str = "nic.sock";

/// Kernel command line that configures the guest's `eth0`.
pub fn boot_args() -> String {
    format!("ip={GUEST}::{GATEWAY}:255.255.255.252::eth0:off")
}

fn ruleset() -> String {
    format!(
        r#"table inet zapper {{
  chain prerouting {{
    type nat hook prerouting priority dstnat; policy accept;
    iifname "{TAP}" meta l4proto tcp redirect to :{TCP_PORT}
    iifname "{TAP}" udp dport 53 redirect to :{DNS_PORT}
  }}
  chain input {{
    type filter hook input priority filter; policy drop;
    iifname "lo" accept
    iifname "{TAP}" tcp dport {TCP_PORT} accept
    iifname "{TAP}" udp dport {DNS_PORT} accept
    ct state established,related accept
  }}
  chain forward {{
    type filter hook forward priority filter; policy drop;
  }}
}}
"#
    )
}

fn tool(name: &str) -> PathBuf {
    for dir in ["/usr/sbin", "/sbin", "/usr/bin", "/bin"] {
        let p = Path::new(dir).join(name);
        if p.is_file() {
            return p;
        }
    }
    PathBuf::from(name)
}

fn run(name: &str, args: &[&str], stdin: Option<&str>) -> Result<()> {
    let mut cmd = Command::new(tool(name));
    cmd.args(args).stdout(Stdio::null()).stderr(Stdio::piped());
    if stdin.is_some() {
        cmd.stdin(Stdio::piped());
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("running {name} (install iproute2 / nftables)"))?;
    if let Some(input) = stdin {
        child.stdin.take().unwrap().write_all(input.as_bytes())?;
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!(
            "{name} {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Builds the guest's network inside the current network namespace, which
/// must be new and owned by the current user namespace.
/// `tap_user`: the namespace uid that will open the tap, when it isn't
/// root.
pub fn setup(tap_user: Option<u32>) -> Result<()> {
    run("ip", &["link", "set", "lo", "up"], None)?;
    match tap_user {
        Some(u) => run(
            "ip",
            &["tuntap", "add", TAP, "mode", "tap", "user", &u.to_string()],
            None,
        )?,
        None => run("ip", &["tuntap", "add", TAP, "mode", "tap"], None)?,
    }
    run(
        "ip",
        &["addr", "add", &format!("{GATEWAY}/30"), "dev", TAP],
        None,
    )?;
    // IPv6 on the tap is best-effort: on a host without IPv6 this fails and the
    // datapath stays IPv4-only. The redirect rules below are family-agnostic,
    // so once the tap has this address v6 is captured the same way v4 is.
    let _ = run(
        "ip",
        &["-6", "addr", "add", &format!("{GATEWAY6}/64"), "dev", TAP],
        None,
    );
    run("ip", &["link", "set", TAP, "up"], None)?;
    run("nft", &["-f", "-"], Some(&ruleset()))?;
    Ok(())
}

/// Starts the capture listeners; they hand traffic to the daemon over the
/// session's `nic.sock`. IPv4 is required; the IPv6 listeners are best-effort,
/// started only when the tap has an IPv6 address (a host with IPv6).
pub fn serve(dir: &Path) -> Result<()> {
    let socket = dir.join(SOCKET);
    serve_tcp(
        TcpListener::bind(SocketAddrV4::new(GATEWAY, TCP_PORT))?,
        socket.clone(),
        false,
    );
    serve_dns(
        UdpSocket::bind(SocketAddrV4::new(GATEWAY, DNS_PORT))?,
        socket.clone(),
    );
    if let Ok(tcp6) = TcpListener::bind(SocketAddrV6::new(GATEWAY6, TCP_PORT, 0, 0)) {
        serve_tcp(tcp6, socket.clone(), true);
    }
    if let Ok(udp6) = UdpSocket::bind(SocketAddrV6::new(GATEWAY6, DNS_PORT, 0, 0)) {
        serve_dns(udp6, socket.clone());
    }
    Ok(())
}

/// Relays each captured TCP connection to the daemon. `v6` selects how the
/// pre-redirect destination is read.
fn serve_tcp(listener: TcpListener, socket: PathBuf, v6: bool) {
    thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let s = socket.clone();
            thread::spawn(move || {
                let _ = relay_tcp(conn, &s, v6);
            });
        }
    });
}

/// Relays each captured DNS query to the daemon and sends back its answer.
/// Address-family-agnostic: a v6 query comes in on the v6 socket and the
/// answer (an A or AAAA record) goes back the same way.
fn serve_dns(udp: UdpSocket, socket: PathBuf) {
    thread::spawn(move || {
        let mut buf = [0u8; 1500];
        loop {
            let Ok((n, peer)) = udp.recv_from(&mut buf) else {
                continue;
            };
            let (query, socket, udp) = (buf[..n].to_vec(), socket.clone(), udp.try_clone());
            thread::spawn(move || {
                if let (Ok(answer), Ok(udp)) = (relay_dns(&query, &socket), udp) {
                    let _ = udp.send_to(&answer, peer);
                }
            });
        }
    });
}

/// The destination the guest dialled, before the redirect.
fn original_dst(conn: &TcpStream, v6: bool) -> io::Result<SocketAddr> {
    const SO_ORIGINAL_DST: libc::c_int = 80;
    if v6 {
        let mut addr: libc::sockaddr_in6 = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t;
        let r = unsafe {
            libc::getsockopt(
                conn.as_raw_fd(),
                libc::SOL_IPV6,
                SO_ORIGINAL_DST,
                &mut addr as *mut _ as *mut libc::c_void,
                &mut len,
            )
        };
        if r != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(SocketAddr::V6(SocketAddrV6::new(
            Ipv6Addr::from(addr.sin6_addr.s6_addr),
            u16::from_be(addr.sin6_port),
            0,
            0,
        )))
    } else {
        let mut addr: libc::sockaddr_in = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
        let r = unsafe {
            libc::getsockopt(
                conn.as_raw_fd(),
                libc::SOL_IP,
                SO_ORIGINAL_DST,
                &mut addr as *mut _ as *mut libc::c_void,
                &mut len,
            )
        };
        if r != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::from(u32::from_be(addr.sin_addr.s_addr)),
            u16::from_be(addr.sin_port),
        )))
    }
}

fn relay_tcp(conn: TcpStream, socket: &Path, v6: bool) -> io::Result<()> {
    let dst = original_dst(&conn, v6)?;
    let mut up = UnixStream::connect(socket)?;
    // The destination's IP literal; a v6 address writes as e.g. `2001:2::1`,
    // which the daemon parses as an `IpAddr`.
    up.write_all(format!("TCP {} {}\n", dst.ip(), dst.port()).as_bytes())?;
    let (mut cr, mut uw) = (conn.try_clone()?, up.try_clone()?);
    let t = thread::spawn(move || {
        let _ = io::copy(&mut cr, &mut uw);
        let _ = uw.shutdown(std::net::Shutdown::Write);
    });
    let (mut ur, mut cw) = (up, conn);
    let _ = io::copy(&mut ur, &mut cw);
    let _ = cw.shutdown(std::net::Shutdown::Write);
    let _ = t.join();
    Ok(())
}

fn relay_dns(query: &[u8], socket: &Path) -> io::Result<Vec<u8>> {
    let mut up = UnixStream::connect(socket)?;
    up.set_read_timeout(Some(Duration::from_secs(5)))?;
    up.write_all(format!("DNS {}\n", query.len()).as_bytes())?;
    up.write_all(query)?;
    let mut len = [0u8; 2];
    up.read_exact(&mut len)?;
    let mut answer = vec![0u8; u16::from_be_bytes(len) as usize];
    up.read_exact(&mut answer)?;
    Ok(answer)
}
