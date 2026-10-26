//! Minimal CQL native-protocol bits for the Cassandra auth-relay (proposal 07).
//!
//! The guest connects with no credentials. The proxy relays the guest's
//! `OPTIONS`/`STARTUP` to the server and, when the server demands auth
//! (`AUTHENTICATE`), answers it with SASL/PLAIN using the host-held
//! credential, then fabricates a `READY` back to the guest so it proceeds as
//! if no auth were required. After the handshake the relay splices bytes; it
//! never parses CQL queries.
//!
//! Only the handshake is modelled, and frame compression is refused (the guest
//! asking for it is rejected), so bodies are always plaintext here.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

// Opcodes (a response sets the frame version's high bit; the opcode is the same
// numeric space for both directions).
const OP_ERROR: u8 = 0x00;
const OP_STARTUP: u8 = 0x01;
const OP_READY: u8 = 0x02;
const OP_AUTHENTICATE: u8 = 0x03;
const OP_OPTIONS: u8 = 0x05;
const OP_AUTH_RESPONSE: u8 = 0x0F;
const OP_AUTH_SUCCESS: u8 = 0x10;

/// The 9-byte frame header plus a cap on the body we will buffer during the
/// handshake (real frames can be far larger, but handshake frames are tiny).
const HEADER: usize = 9;
const MAX_BODY: usize = 1 << 20;

/// One CQL frame: `version`, `flags`, `stream` id, `opcode`, and the body.
#[derive(Debug, PartialEq)]
pub struct Frame {
    pub version: u8,
    pub flags: u8,
    pub stream: i16,
    pub opcode: u8,
    pub body: Vec<u8>,
}

/// Serialises a frame: the 9-byte header (`version`, `flags`, `Int16` stream,
/// `opcode`, `Int32` length) then the body.
pub fn build(version: u8, flags: u8, stream: i16, opcode: u8, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER + body.len());
    out.push(version);
    out.push(flags);
    out.extend_from_slice(&stream.to_be_bytes());
    out.push(opcode);
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(body);
    out
}

/// Reads one whole frame, buffering partial reads in `buf`.
pub(crate) async fn read_frame<S: AsyncRead + Unpin>(
    s: &mut S,
    buf: &mut Vec<u8>,
) -> Result<Frame, String> {
    let mut chunk = [0u8; 4096];
    loop {
        if buf.len() >= HEADER {
            let len = u32::from_be_bytes(buf[5..9].try_into().unwrap()) as usize;
            if len > MAX_BODY {
                return Err("CQL handshake frame too large".into());
            }
            if buf.len() >= HEADER + len {
                let frame = Frame {
                    version: buf[0],
                    flags: buf[1],
                    stream: i16::from_be_bytes(buf[2..4].try_into().unwrap()),
                    opcode: buf[4],
                    body: buf[HEADER..HEADER + len].to_vec(),
                };
                buf.drain(..HEADER + len);
                return Ok(frame);
            }
        }
        let n = s
            .read(&mut chunk)
            .await
            .map_err(|e| format!("reading CQL frame: {e}"))?;
        if n == 0 {
            return Err("connection closed during the CQL handshake".into());
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

fn read_short(b: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_be_bytes(b.get(off..off + 2)?.try_into().ok()?))
}

/// Reads a CQL `[string]` (`Int16` length + UTF-8 bytes) at `off`, returning it
/// and the offset past it.
fn read_string(b: &[u8], off: usize) -> Option<(String, usize)> {
    let len = read_short(b, off)? as usize;
    let start = off + 2;
    let end = start + len;
    let s = std::str::from_utf8(b.get(start..end)?).ok()?.to_string();
    Some((s, end))
}

/// Whether a `STARTUP` string-map body requests frame compression — which this
/// relay does not implement, so such a startup is refused.
pub(crate) fn startup_has_compression(body: &[u8]) -> bool {
    let Some(n) = read_short(body, 0) else {
        return false;
    };
    let mut off = 2;
    for _ in 0..n {
        let Some((key, after_key)) = read_string(body, off) else {
            return false;
        };
        let Some((_, after_val)) = read_string(body, after_key) else {
            return false;
        };
        if key.eq_ignore_ascii_case("COMPRESSION") {
            return true;
        }
        off = after_val;
    }
    false
}

/// A SASL/PLAIN token: `authzid \0 authcid \0 passwd`, with an empty authzid.
fn plain_token(user: &str, password: &str) -> Vec<u8> {
    let mut t = vec![0];
    t.extend_from_slice(user.as_bytes());
    t.push(0);
    t.extend_from_slice(password.as_bytes());
    t
}

/// Wraps a token as a CQL `[bytes]` value: `Int32` length then the bytes.
fn bytes_value(token: &[u8]) -> Vec<u8> {
    let mut b = (token.len() as i32).to_be_bytes().to_vec();
    b.extend_from_slice(token);
    b
}

/// Parses an `ERROR` body (`Int32` code, `[string]` message) into a message.
fn error_message(body: &[u8]) -> String {
    let Some(code) = body
        .get(0..4)
        .map(|b| i32::from_be_bytes(b.try_into().unwrap()))
    else {
        return "server reported an error".into();
    };
    match read_string(body, 4) {
        Some((msg, _)) => format!("server error {code:#x}: {msg}"),
        None => format!("server error {code:#x}"),
    }
}

/// Drives the CQL handshake: relays the guest's `OPTIONS` (if any), forwards
/// its `STARTUP`, and — when the server asks to authenticate — answers with
/// SASL/PLAIN using `user`/`password`, then sends the guest a `READY` so it
/// proceeds. Returns once the guest may splice. Refuses a `STARTUP` that asks
/// for compression, and fails closed on an `ERROR` or an unexpected opcode.
pub async fn handshake<G, S>(
    guest: &mut G,
    server: &mut S,
    user: &str,
    password: &str,
) -> Result<(), String>
where
    G: AsyncRead + AsyncWrite + Unpin,
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut gbuf = Vec::new();
    let mut sbuf = Vec::new();
    loop {
        let f = read_frame(guest, &mut gbuf).await?;
        match f.opcode {
            OP_OPTIONS => {
                // Let the client probe supported options: forward and relay back.
                server
                    .write_all(&build(f.version, f.flags, f.stream, OP_OPTIONS, &f.body))
                    .await
                    .map_err(|e| format!("forwarding OPTIONS: {e}"))?;
                let r = read_frame(server, &mut sbuf).await?;
                guest
                    .write_all(&build(r.version, r.flags, r.stream, r.opcode, &r.body))
                    .await
                    .map_err(|e| format!("relaying OPTIONS reply: {e}"))?;
            }
            OP_STARTUP => {
                if startup_has_compression(&f.body) {
                    return Err("CQL frame compression is not supported by the relay".into());
                }
                server
                    .write_all(&build(f.version, f.flags, f.stream, OP_STARTUP, &f.body))
                    .await
                    .map_err(|e| format!("forwarding STARTUP: {e}"))?;
                let r = read_frame(server, &mut sbuf).await?;
                match r.opcode {
                    // No auth required: hand the server's READY straight through.
                    OP_READY => {
                        guest
                            .write_all(&build(r.version, r.flags, r.stream, OP_READY, &r.body))
                            .await
                            .map_err(|e| format!("relaying READY: {e}"))?;
                        return Ok(());
                    }
                    OP_AUTHENTICATE => {
                        let body = bytes_value(&plain_token(user, password));
                        server
                            .write_all(&build(
                                f.version,
                                f.flags,
                                f.stream,
                                OP_AUTH_RESPONSE,
                                &body,
                            ))
                            .await
                            .map_err(|e| format!("sending AUTH_RESPONSE: {e}"))?;
                        let a = read_frame(server, &mut sbuf).await?;
                        match a.opcode {
                            OP_AUTH_SUCCESS => {
                                // The guest sent STARTUP and expects READY; give
                                // it one (response version, its own stream id).
                                guest
                                    .write_all(&build(0x80 | f.version, 0, f.stream, OP_READY, &[]))
                                    .await
                                    .map_err(|e| format!("sending READY to guest: {e}"))?;
                                return Ok(());
                            }
                            OP_ERROR => return Err(error_message(&a.body)),
                            op => {
                                return Err(format!(
                                    "unexpected opcode {op:#x} after AUTH_RESPONSE"
                                ))
                            }
                        }
                    }
                    OP_ERROR => return Err(error_message(&r.body)),
                    op => return Err(format!("unexpected opcode {op:#x} in reply to STARTUP")),
                }
            }
            op => return Err(format!("unexpected opcode {op:#x} before STARTUP")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frame_round_trips() {
        let mut bytes = build(0x04, 0, 7, OP_STARTUP, b"hello");
        // Trailing bytes from a following frame must not be consumed.
        bytes.extend_from_slice(&[0xde, 0xad]);
        let mut buf = Vec::new();
        let f = read_frame(&mut &bytes[..], &mut buf).await.unwrap();
        assert_eq!(
            f,
            Frame {
                version: 0x04,
                flags: 0,
                stream: 7,
                opcode: OP_STARTUP,
                body: b"hello".to_vec(),
            }
        );
    }

    #[test]
    fn plain_token_and_compression_detection() {
        assert_eq!(plain_token("u", "p"), b"\0u\0p");
        // A STARTUP string map with CQL_VERSION only → no compression.
        let mut body = 1u16.to_be_bytes().to_vec();
        for s in ["CQL_VERSION", "3.0.0"] {
            body.extend_from_slice(&(s.len() as u16).to_be_bytes());
            body.extend_from_slice(s.as_bytes());
        }
        assert!(!startup_has_compression(&body));
        // With a COMPRESSION entry → detected.
        let mut body = 1u16.to_be_bytes().to_vec();
        for s in ["COMPRESSION", "lz4"] {
            body.extend_from_slice(&(s.len() as u16).to_be_bytes());
            body.extend_from_slice(s.as_bytes());
        }
        assert!(startup_has_compression(&body));
    }

    fn startup_map(pairs: &[(&str, &str)]) -> Vec<u8> {
        let mut body = (pairs.len() as u16).to_be_bytes().to_vec();
        for (k, v) in pairs {
            for s in [k, v] {
                body.extend_from_slice(&(s.len() as u16).to_be_bytes());
                body.extend_from_slice(s.as_bytes());
            }
        }
        body
    }

    /// The guest sends STARTUP; a fake server demands PLAIN auth, checks the
    /// token, and succeeds; the handshake then gives the guest a READY.
    #[tokio::test]
    async fn handshake_authenticates_with_plain_and_readies_the_guest() {
        let (mut guest_real, guest_proxy) = tokio::io::duplex(4096);
        let (server_proxy, server_real) = tokio::io::duplex(4096);

        let guest = tokio::spawn(async move {
            guest_real
                .write_all(&build(
                    0x04,
                    0,
                    1,
                    OP_STARTUP,
                    &startup_map(&[("CQL_VERSION", "3.0.0")]),
                ))
                .await
                .unwrap();
            let r = read_frame(&mut guest_real, &mut Vec::new()).await.unwrap();
            assert_eq!(r.opcode, OP_READY);
            assert_eq!(r.stream, 1);
            assert_eq!(r.version & 0x80, 0x80, "response frame");
        });
        let server = tokio::spawn(async move {
            let mut sbuf = Vec::new();
            let mut server_real = server_real;
            let startup = read_frame(&mut server_real, &mut sbuf).await.unwrap();
            assert_eq!(startup.opcode, OP_STARTUP);
            server_real
                .write_all(&build(0x84, 0, startup.stream, OP_AUTHENTICATE, &{
                    let a = "org.apache.cassandra.auth.PasswordAuthenticator";
                    let mut b = (a.len() as u16).to_be_bytes().to_vec();
                    b.extend_from_slice(a.as_bytes());
                    b
                }))
                .await
                .unwrap();
            let ar = read_frame(&mut server_real, &mut sbuf).await.unwrap();
            assert_eq!(ar.opcode, OP_AUTH_RESPONSE);
            // body = [bytes] token; token = \0user\0pass
            let len = i32::from_be_bytes(ar.body[0..4].try_into().unwrap()) as usize;
            assert_eq!(&ar.body[4..4 + len], b"\0app\0s3cret");
            server_real
                .write_all(&build(
                    0x84,
                    0,
                    ar.stream,
                    OP_AUTH_SUCCESS,
                    &[0xff, 0xff, 0xff, 0xff],
                ))
                .await
                .unwrap();
        });

        let mut guest_proxy = guest_proxy;
        let mut server_proxy = server_proxy;
        handshake(&mut guest_proxy, &mut server_proxy, "app", "s3cret")
            .await
            .unwrap();
        guest.await.unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn handshake_fails_closed_on_auth_error() {
        let (mut guest_real, mut guest_proxy) = tokio::io::duplex(4096);
        let (mut server_proxy, mut server_real) = tokio::io::duplex(4096);
        let _guest = tokio::spawn(async move {
            guest_real
                .write_all(&build(
                    0x04,
                    0,
                    1,
                    OP_STARTUP,
                    &startup_map(&[("CQL_VERSION", "3.0.0")]),
                ))
                .await
                .unwrap();
            // May receive nothing; the handshake errors out.
            let _ = read_frame(&mut guest_real, &mut Vec::new()).await;
        });
        let server = tokio::spawn(async move {
            let mut sbuf = Vec::new();
            let _ = read_frame(&mut server_real, &mut sbuf).await.unwrap();
            server_real
                .write_all(&build(0x84, 0, 1, OP_AUTHENTICATE, &{
                    let a = "x";
                    let mut b = (a.len() as u16).to_be_bytes().to_vec();
                    b.extend_from_slice(a.as_bytes());
                    b
                }))
                .await
                .unwrap();
            let _ = read_frame(&mut server_real, &mut sbuf).await.unwrap();
            // ERROR: Int32 code + [string] message.
            let mut body = 0x0100i32.to_be_bytes().to_vec();
            let msg = "Bad credentials";
            body.extend_from_slice(&(msg.len() as u16).to_be_bytes());
            body.extend_from_slice(msg.as_bytes());
            server_real
                .write_all(&build(0x84, 0, 1, OP_ERROR, &body))
                .await
                .unwrap();
        });
        let err = handshake(&mut guest_proxy, &mut server_proxy, "app", "bad")
            .await
            .unwrap_err();
        assert!(err.contains("Bad credentials"), "got: {err}");
        server.await.unwrap();
    }
}
