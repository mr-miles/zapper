//! Host ↔ guest protocol over Firecracker vsock.
//!
//! The guest connects to the host on [`CONTROL_PORT`] once, at boot, and the
//! two sides exchange [`HostMsg`] / [`GuestMsg`] as JSON lines. The host only
//! accepts the first connection on that port, which the agent makes before any
//! workload runs, so a workload cannot pose as the agent.
//!
//! The host opens one connection to the guest's [`SHELL_PORT`] per terminal.
//! It starts with a JSON [`ShellRequest`] line, then both sides send
//! [`Frame`]s.
//!
//! The guest reaches the egress proxy on the host's [`PROXY_PORT`]; the agent
//! forwards `127.0.0.1:3128` in the guest to it.

use std::collections::BTreeMap;
use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

use crate::{ExecRequest, FsRule};

/// Vsock CID of the host.
pub const HOST_CID: u32 = 2;
/// Guest → host control channel.
pub const CONTROL_PORT: u32 = 1000;
/// Guest → host egress proxy.
pub const PROXY_PORT: u32 = 3128;
/// Host → guest terminals.
pub const SHELL_PORT: u32 = 1024;
/// Guest → host MCP: the daemon's gateway to the servers registered for the
/// session. A connection names a server on its first line, then carries MCP
/// as on stdio: one JSON-RPC message per line, each way.
pub const MCP_PORT: u32 = 1100;
/// The NIC's point-to-point link, for the host (its namespace's end of the
/// tap, `zapper_core::netns`) and the guest agent alike: the gateway, which
/// is also the guest's DNS server, and the guest. IPv6 (a ULA) only where
/// the host and guest kernels have it.
pub const NIC_GATEWAY: std::net::Ipv4Addr = std::net::Ipv4Addr::new(10, 0, 2, 1);
pub const NIC_GUEST: std::net::Ipv4Addr = std::net::Ipv4Addr::new(10, 0, 2, 2);
pub const NIC_GATEWAY6: std::net::Ipv6Addr = std::net::Ipv6Addr::new(0xfd00, 0x2, 0, 0, 0, 0, 0, 1);
pub const NIC_GUEST6: std::net::Ipv6Addr = std::net::Ipv6Addr::new(0xfd00, 0x2, 0, 0, 0, 0, 0, 2);
/// Longest MCP message from the guest.
pub const MAX_MCP_LINE: usize = 128 * 1024;

/// A tool's own name from the name an agent calls it by: Claude Code and pi
/// call a tool of MCP server `office` `mcp__office__mail_send`.
pub fn mcp_tool_name(name: &str) -> &str {
    name.strip_prefix("mcp__")
        .and_then(|rest| rest.split_once("__"))
        .map_or(name, |(_, tool)| tool)
}

/// Longest JSON line either side accepts.
pub const MAX_LINE: usize = 256 * 1024;
/// Largest frame payload.
pub const MAX_FRAME: usize = 64 * 1024;

/// Everything the guest needs to set itself up, sent once after it connects.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GuestSpec {
    pub session: String,
    /// The guest has a NIC (`eth0`, configured by the kernel); DNS is at the
    /// gateway. Otherwise only the vsock proxy forwarder.
    pub nic: bool,
    pub hostname: String,
    pub user: String,
    pub root: bool,
    pub home: String,
    pub workdir: String,
    /// Environment of every process started over the shell port.
    pub env: BTreeMap<String, String>,
    pub disks: Vec<GuestDisk>,
    pub files: Vec<GuestFile>,
    pub fs: Vec<FsRule>,
    /// Shell commands run as root after setup, before the workload.
    pub setup: Vec<String>,
    /// PEM of the proxy's CA, installed in the system trust store.
    pub ca_pem: String,
    /// Ask the host before every exec of the workload.
    pub supervise_exec: bool,
    /// Host → guest vsock ports forwarded to local TCP ports, for a firm's
    /// facilities. Only the host may connect.
    #[serde(default)]
    pub forwards: Vec<Forward>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Forward {
    pub vsock_port: u32,
    pub tcp_port: u16,
    /// Connect to the guest's own NIC address instead of loopback: services
    /// such as a Kubernetes load balancer only answer there.
    #[serde(default)]
    pub nic: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuestDisk {
    /// Block device, e.g. `/dev/vdb`.
    pub device: String,
    pub path: String,
    pub read_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuestFile {
    /// Absolute guest path.
    pub path: String,
    pub content: String,
    pub mode: u32,
    /// Owned by the workload user rather than root.
    pub user_owned: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostMsg {
    Init {
        spec: Box<GuestSpec>,
        /// For a persistent session: what the agent, when its control
        /// connection is lost, sends back on a new one (`GuestMsg::Resume`),
        /// to be taken for itself; kept in PID 1's memory only. Without it,
        /// a lost connection powers the guest off.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resume: Option<String>,
    },
    ExecReply {
        id: u64,
        allow: bool,
    },
    Shutdown,
    /// First on a control connection taken again (`GuestMsg::Resume`): the
    /// host has it, and the agent asks again what was waiting.
    Resumed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GuestMsg {
    /// First on a control connection made again, after the first was lost
    /// (the daemon restarted): the token from `Init`, and the persistent
    /// terminals the guest keeps.
    Resume {
        token: String,
        terminals: Vec<u64>,
    },
    /// Setup finished; terminals can be opened.
    Ready,
    /// Setup failed; the session is unusable.
    Failed {
        error: String,
    },
    /// Asks whether the workload may exec. The process waits for the reply.
    ExecCheck {
        id: u64,
        pid: i32,
        uid: u32,
        req: ExecRequest,
    },
    Event {
        event: GuestEvent,
    },
}

/// Instrumentation from inside the guest.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum GuestEvent {
    /// A setup step's outcome.
    Setup {
        step: String,
        ok: bool,
        output: String,
    },
    /// Process exit, from kernel process accounting.
    Exit {
        pid: u32,
        ppid: u32,
        uid: u32,
        comm: String,
        exit_code: i32,
        /// Terminating signal, if any.
        signal: Option<i32>,
        elapsed_ms: u64,
        user_ms: u64,
        sys_ms: u64,
        /// Average memory use in KiB.
        mem_kib: u64,
        /// Characters read and written.
        io_bytes: u64,
    },
    /// Shell port session ended.
    ShellExit { id: u64, exit_code: i32 },
    /// Periodic whole-guest usage.
    Usage {
        load1: f64,
        mem_used_kib: u64,
        procs: u32,
        disk_used_kib: u64,
    },
}

/// First line on a shell connection.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ShellRequest {
    pub id: u64,
    /// Command; empty means the profile's command.
    pub argv: Vec<String>,
    /// Allocate a pseudo-terminal of this size.
    pub tty: Option<(u16, u16)>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Run as root without exec supervision, for the host's own maintenance
    /// (copying files out). Only the host can open shell connections.
    #[serde(default)]
    pub system: bool,
    /// Keep the program running when this connection closes, to attach to
    /// it again (`attach`): a terminal (`tty`) only. A holder process keeps
    /// its terminal and the latest of its output meanwhile.
    #[serde(default)]
    pub persist: bool,
    /// Attach to the persistent terminal `id`, which is running, instead of
    /// starting a program: `argv`, `env` and `system` are ignored, and `tty`
    /// is the attaching terminal's size. A connection attached already is
    /// closed: the new one takes over.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attach: Option<u64>,
}

impl ShellRequest {
    /// `argv` run without a terminal, under exec supervision (empty: the
    /// profile's command).
    pub fn exec(argv: Vec<String>) -> Self {
        Self {
            argv,
            ..Default::default()
        }
    }

    /// `argv` run as root without exec supervision, for the host's own
    /// maintenance.
    pub fn system(argv: Vec<String>) -> Self {
        Self {
            argv,
            system: true,
            ..Default::default()
        }
    }

    /// Attaching to the persistent terminal `id`.
    pub fn attach(id: u64) -> Self {
        Self {
            attach: Some(id),
            ..Default::default()
        }
    }
}

/// Most terminals a guest keeps persistent at once.
pub const MAX_PERSISTENT_TERMINALS: usize = 16;

/// A frame on a shell connection.
#[derive(Debug, Clone, PartialEq)]
pub enum Frame {
    /// Host → guest: stdin. Guest → host: stdout (and stderr with a tty).
    Data(Vec<u8>),
    /// Guest → host without a tty.
    Stderr(Vec<u8>),
    /// Host → guest: terminal resized to rows, cols.
    Resize(u16, u16),
    /// Host → guest: stdin closed.
    Eof,
    /// Guest → host: the command exited; last frame.
    Exit(i32),
    /// Guest → host, first on a persistent terminal's connection: the output
    /// that follows starts at byte `from` of all the terminal has written
    /// (the oldest it still keeps), then live; `to` is where it was then.
    Attached { from: u64, to: u64 },
    /// Host → guest: end the persistent terminal on purpose (its program
    /// gets `SIGHUP`), rather than detaching from it.
    Hangup,
}

impl Frame {
    /// The frame on the wire: its kind, its body's length, its body.
    pub fn encode(&self) -> Vec<u8> {
        let (kind, body): (u8, Vec<u8>) = match self {
            Frame::Data(d) => (0, d.clone()),
            Frame::Stderr(d) => (1, d.clone()),
            Frame::Resize(r, c) => (2, [r.to_be_bytes(), c.to_be_bytes()].concat()),
            Frame::Eof => (3, Vec::new()),
            Frame::Exit(code) => (4, code.to_be_bytes().to_vec()),
            Frame::Attached { from, to } => (5, [from.to_be_bytes(), to.to_be_bytes()].concat()),
            Frame::Hangup => (6, Vec::new()),
        };
        let mut buf = Vec::with_capacity(5 + body.len());
        buf.push(kind);
        buf.extend_from_slice(&(body.len() as u32).to_be_bytes());
        buf.extend_from_slice(&body);
        buf
    }

    pub fn write_to(&self, w: &mut impl Write) -> io::Result<()> {
        w.write_all(&self.encode())
    }

    /// What a frame's header says: its kind and its body's length, refused
    /// past `MAX_FRAME`. The one check of it, for every reader.
    pub fn header(header: &[u8; 5]) -> io::Result<(u8, usize)> {
        let len = u32::from_be_bytes(header[1..].try_into().unwrap()) as usize;
        if len > MAX_FRAME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "frame too large",
            ));
        }
        Ok((header[0], len))
    }

    /// Parses a frame from its header and body.
    pub fn decode(kind: u8, body: Vec<u8>) -> io::Result<Frame> {
        let bad = || io::Error::new(io::ErrorKind::InvalidData, "bad frame");
        Ok(match kind {
            0 => Frame::Data(body),
            1 => Frame::Stderr(body),
            2 if body.len() == 4 => Frame::Resize(
                u16::from_be_bytes([body[0], body[1]]),
                u16::from_be_bytes([body[2], body[3]]),
            ),
            3 => Frame::Eof,
            4 if body.len() == 4 => Frame::Exit(i32::from_be_bytes(body.try_into().unwrap())),
            5 if body.len() == 16 => Frame::Attached {
                from: u64::from_be_bytes(body[..8].try_into().unwrap()),
                to: u64::from_be_bytes(body[8..].try_into().unwrap()),
            },
            6 => Frame::Hangup,
            _ => return Err(bad()),
        })
    }

    /// Reads one frame; `None` on a clean end of stream.
    pub fn read_from(r: &mut impl Read) -> io::Result<Option<Frame>> {
        let mut header = [0u8; 5];
        match r.read_exact(&mut header) {
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            other => other?,
        }
        let (kind, len) = Frame::header(&header)?;
        let mut body = vec![0; len];
        r.read_exact(&mut body)?;
        Frame::decode(kind, body).map(Some)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The NIC's addresses as the guest writes them (resolv.conf, `ip`) and
    /// the host configures them: one link.
    #[test]
    fn the_nic_link_addresses() {
        assert_eq!(NIC_GATEWAY.to_string(), "10.0.2.1");
        assert_eq!(NIC_GUEST.to_string(), "10.0.2.2");
        assert_eq!(NIC_GATEWAY6.to_string(), "fd00:2::1");
        assert_eq!(NIC_GUEST6.to_string(), "fd00:2::2");
    }

    #[test]
    fn a_tool_is_named_without_its_server() {
        assert_eq!(mcp_tool_name("mcp__office__mail_send"), "mail_send");
        assert_eq!(mcp_tool_name("mcp__github__create_issue"), "create_issue");
        assert_eq!(mcp_tool_name("Edit"), "Edit");
        assert_eq!(mcp_tool_name("mcp__broken"), "mcp__broken");
    }

    #[test]
    fn frames_roundtrip() {
        let frames = [
            Frame::Data(b"hi".to_vec()),
            Frame::Resize(24, 80),
            Frame::Eof,
            Frame::Exit(-3),
            Frame::Attached {
                from: 7,
                to: 1 << 40,
            },
            Frame::Hangup,
        ];
        let mut buf = Vec::new();
        for f in &frames {
            f.write_to(&mut buf).unwrap();
        }
        let mut r = &buf[..];
        for f in &frames {
            assert_eq!(Frame::read_from(&mut r).unwrap().as_ref(), Some(f));
        }
        assert_eq!(Frame::read_from(&mut r).unwrap(), None);
    }
}
