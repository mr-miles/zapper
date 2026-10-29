//! Forwards the guest's local proxy port to the host's egress proxy, in vsock
//! mode, where the guest has loopback only. The host decides what passes.

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::thread;

use zapper_proto::proto::{HOST_CID, PROXY_PORT};

use crate::sys;

pub const LOCAL: &str = "127.0.0.1:3128";

pub fn start() -> io::Result<()> {
    let listener = TcpListener::bind(LOCAL)?;
    thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            thread::spawn(move || {
                if let Ok(upstream) = sys::vsock_connect(HOST_CID, PROXY_PORT) {
                    splice(conn, upstream);
                }
            });
        }
    });
    Ok(())
}

/// Forwards host connections on vsock ports to local TCP ports, for a firm's
/// facilities. Only the host (CID 2) may connect.
pub fn inbound(forwards: &[zapper_proto::proto::Forward]) -> io::Result<()> {
    for f in forwards {
        let listener = sys::VsockListener::bind(f.vsock_port)?;
        let tcp = f.tcp_port;
        let host = if f.nic {
            zapper_proto::proto::GUEST_NIC_ADDR
        } else {
            "127.0.0.1"
        };
        thread::spawn(move || loop {
            let Ok((conn, cid)) = listener.accept() else {
                continue;
            };
            if cid != HOST_CID {
                continue;
            }
            thread::spawn(move || {
                if let Ok(local) = TcpStream::connect((host, tcp)) {
                    splice(local, conn);
                }
            });
        });
    }
    Ok(())
}

fn splice(conn: TcpStream, upstream: std::fs::File) {
    let (Ok(mut conn_r), Ok(mut up_w)) = (conn.try_clone(), upstream.try_clone()) else {
        return;
    };
    let (mut conn_w, mut up_r) = (conn, upstream);
    let t = thread::spawn(move || {
        let _ = copy(&mut conn_r, &mut up_w);
        unsafe { libc::shutdown(std::os::fd::AsRawFd::as_raw_fd(&up_w), libc::SHUT_WR) };
    });
    let _ = copy(&mut up_r, &mut conn_w);
    let _ = conn_w.shutdown(Shutdown::Write);
    let _ = t.join();
}

fn copy(r: &mut impl Read, w: &mut impl Write) -> io::Result<()> {
    let mut buf = [0u8; 16384];
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            return Ok(());
        }
        w.write_all(&buf[..n])?;
    }
}
