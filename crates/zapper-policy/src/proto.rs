//! What the office says to a sandbox's commands, and the two kinds of policy
//! item the config model names: the command a session runs and its stream's
//! frames (`ShellRequest`, `Frame`), the files it is given (`GuestFile`), an
//! exec (`ExecRequest`) and a filesystem rule (`FsRule`).

use std::collections::BTreeMap;
use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ExecRequest {
    /// Resolved executable path.
    pub path: String,
    #[serde(default)]
    pub argv: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

/// Filesystem rule, applied inside the sandbox when the session starts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FsRule {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Guest path glob, e.g. `/work/**/.env`.
    pub path: String,
    pub access: FsAccess,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FsAccess {
    /// Covered by an empty file or directory.
    Hidden,
    /// Remounted read-only.
    Ro,
}

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuestFile {
    /// Absolute guest path.
    pub path: String,
    pub content: String,
    pub mode: u32,
    /// Owned by the workload user rather than root.
    pub user_owned: bool,
}

/// The command a session runs, first on its stream.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ShellRequest {
    /// Command; empty means the profile's command.
    pub argv: Vec<String>,
    /// Allocate a pseudo-terminal of this size.
    pub tty: Option<(u16, u16)>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// The office's own maintenance (copying files in and out): run from
    /// `/`, not the profile's work directory. It runs as the sandbox user,
    /// as everything does.
    #[serde(default)]
    pub system: bool,
}

impl ShellRequest {
    /// `argv` run without a terminal (empty: the profile's command).
    pub fn exec(argv: Vec<String>) -> Self {
        Self {
            argv,
            ..Default::default()
        }
    }

    /// `argv` run for the office's own maintenance.
    pub fn system(argv: Vec<String>) -> Self {
        Self {
            argv,
            system: true,
            ..Default::default()
        }
    }
}

/// A frame on a session's stream.
#[derive(Debug, Clone, PartialEq)]
pub enum Frame {
    /// To the command: stdin. From it: stdout (and stderr with a tty).
    Data(Vec<u8>),
    /// From the command, without a tty.
    Stderr(Vec<u8>),
    /// To the command: its terminal resized to rows, cols.
    Resize(u16, u16),
    /// To the command: stdin closed.
    Eof,
    /// From the command: it exited; the last frame.
    Exit(i32),
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
