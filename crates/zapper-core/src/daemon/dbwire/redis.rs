//! Minimal RESP bits for the Redis/Valkey auth-relay (proposal 07): the proxy
//! issues one `AUTH` on the server leg with the host-held credential and checks
//! the reply, then the relay splices. The guest holds no password and never
//! authenticates — its commands run on the pre-authenticated server connection,
//! so there is no guest-facing handshake and nothing to fabricate.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Builds a RESP array command: `*N\r\n` then each argument as a bulk string
/// `$len\r\n<bytes>\r\n`.
fn command(args: &[&[u8]]) -> Vec<u8> {
    let mut out = format!("*{}\r\n", args.len()).into_bytes();
    for a in args {
        out.extend_from_slice(format!("${}\r\n", a.len()).as_bytes());
        out.extend_from_slice(a);
        out.extend_from_slice(b"\r\n");
    }
    out
}

/// The largest single-line reply we read for `AUTH` before giving up — a simple
/// string or error, never bulk data, so this is generous.
const MAX_REPLY: usize = 4096;

/// Reads one CRLF-terminated reply line (without the CRLF). Byte at a time,
/// which is fine for the single short `AUTH` reply.
async fn read_reply_line<S: AsyncRead + Unpin>(server: &mut S) -> Result<Vec<u8>, String> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = server
            .read(&mut byte)
            .await
            .map_err(|e| format!("reading AUTH reply: {e}"))?;
        if n == 0 {
            return Err("server closed during AUTH".into());
        }
        buf.push(byte[0]);
        if buf.ends_with(b"\r\n") {
            buf.truncate(buf.len() - 2);
            return Ok(buf);
        }
        if buf.len() > MAX_REPLY {
            return Err("AUTH reply too long".into());
        }
    }
}

/// Authenticates the server leg with `AUTH <user> <password>` (the Redis 6+ /
/// Valkey two-argument form; `user` is the ACL username, e.g. `default`) and
/// checks the reply is `+OK`. Errors on a RESP error reply (`-ERR …`) or
/// anything that is not a simple-string OK.
pub async fn authenticate<S: AsyncRead + AsyncWrite + Unpin>(
    server: &mut S,
    user: &str,
    password: &str,
) -> Result<(), String> {
    server
        .write_all(&command(&[b"AUTH", user.as_bytes(), password.as_bytes()]))
        .await
        .map_err(|e| format!("sending AUTH: {e}"))?;
    let line = read_reply_line(server).await?;
    match line.first() {
        Some(b'+') => Ok(()),
        Some(b'-') => Err(format!(
            "server rejected AUTH: {}",
            String::from_utf8_lossy(&line[1..]).trim()
        )),
        _ => Err(format!(
            "unexpected AUTH reply: {}",
            String::from_utf8_lossy(&line)
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_a_resp_array_command() {
        assert_eq!(
            command(&[b"AUTH", b"u", b"pw"]),
            b"*3\r\n$4\r\nAUTH\r\n$1\r\nu\r\n$2\r\npw\r\n"
        );
    }

    #[tokio::test]
    async fn authenticate_sends_auth_and_accepts_ok() {
        let (mut client, mut server) = tokio::io::duplex(1024);
        let expected = command(&[b"AUTH", b"app", b"s3cret"]);
        let srv = tokio::spawn(async move {
            let mut got = vec![0u8; expected.len()];
            server.read_exact(&mut got).await.unwrap();
            assert_eq!(got, expected);
            server.write_all(b"+OK\r\n").await.unwrap();
        });
        authenticate(&mut client, "app", "s3cret").await.unwrap();
        srv.await.unwrap();
    }

    #[tokio::test]
    async fn authenticate_fails_closed_on_an_error_reply() {
        let (mut client, mut server) = tokio::io::duplex(1024);
        let srv = tokio::spawn(async move {
            let mut got = [0u8; 64];
            let _ = server.read(&mut got).await.unwrap();
            server
                .write_all(b"-WRONGPASS invalid username-password pair\r\n")
                .await
                .unwrap();
        });
        let err = authenticate(&mut client, "app", "wrong").await.unwrap_err();
        assert!(err.contains("WRONGPASS"), "got: {err}");
        srv.await.unwrap();
    }
}
