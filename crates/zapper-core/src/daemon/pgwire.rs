//! Postgres v3 wire pieces for database credential injection (proposal 07),
//! spike: enough of the frontend/backend framing to drive an auth-terminating
//! relay — detect the `SSLRequest`, read the `StartupMessage`, frame typed
//! messages (to find where the server reaches `ReadyForQuery`), and the
//! `AuthenticationOk` the proxy sends the guest. It does NOT parse queries;
//! after auth the relay splices bytes.
//!
//! Spike: the parsers and builders here are tested and fuzzed, but nothing is
//! wired into the proxy yet (that, plus the server-side SCRAM exchange and a
//! real-database e2e, are the next steps — see
//! `docs/proposals/07-db-credential-injection.md`).
#![allow(dead_code)]

/// The 8-byte `SSLRequest` a client sends before TLS: `Int32(8)` then
/// `Int32(80877103)`.
pub const SSL_REQUEST: [u8; 8] = [0, 0, 0, 8, 0x04, 0xd2, 0x16, 0x2f];

/// `AuthenticationOk` — the one message the proxy fabricates for the guest, in
/// place of the real auth challenge. The server's own post-auth messages
/// (`ParameterStatus`, `BackendKeyData`, `ReadyForQuery`) are relayed after it.
pub const AUTH_OK: [u8; 9] = [b'R', 0, 0, 0, 8, 0, 0, 0, 0];

/// The protocol version in a v3 `StartupMessage`: 3.0.
const PROTOCOL_V3: u32 = 196608;

/// Whether `buf` begins with a Postgres `SSLRequest`.
pub fn is_ssl_request(buf: &[u8]) -> bool {
    buf.len() >= 8 && buf[..8] == SSL_REQUEST
}

/// A parsed `StartupMessage`: the parameters a client opens with.
#[derive(Debug, Clone, PartialEq)]
pub struct Startup {
    pub user: String,
    pub database: Option<String>,
    pub params: Vec<(String, String)>,
}

/// A NUL-terminated string from the front of `b`: `(before_nul, after_nul)`.
fn cstr(b: &[u8]) -> Option<(&[u8], &[u8])> {
    let i = b.iter().position(|&x| x == 0)?;
    Some((&b[..i], &b[i + 1..]))
}

/// Parses a complete v3 `StartupMessage` from the front of `buf`. `None` if it
/// is not a complete, well-formed v3 startup: too short, the length runs past
/// `buf`, the wrong protocol (an `SSLRequest`/`CancelRequest`), truncated,
/// non-UTF-8, or no `user`.
pub fn parse_startup(buf: &[u8]) -> Option<Startup> {
    let len = u32::from_be_bytes(buf.get(0..4)?.try_into().ok()?) as usize;
    // At least the 8-byte header plus the terminating NUL, and no more than
    // what is in the buffer — the whole message must be present.
    if len < 9 || len > buf.len() {
        return None;
    }
    if u32::from_be_bytes(buf[4..8].try_into().ok()?) != PROTOCOL_V3 {
        return None;
    }
    // key\0 value\0 … key\0 value\0  then an empty key as the terminator.
    let mut rest = &buf[8..len];
    let mut params = Vec::new();
    loop {
        let (key, after) = cstr(rest)?;
        if key.is_empty() {
            break;
        }
        let (val, after) = cstr(after)?;
        params.push((
            std::str::from_utf8(key).ok()?.to_string(),
            std::str::from_utf8(val).ok()?.to_string(),
        ));
        rest = after;
    }
    let user = params.iter().find(|(k, _)| k == "user")?.1.clone();
    let database = params
        .iter()
        .find(|(k, _)| k == "database")
        .map(|(_, v)| v.clone());
    Some(Startup {
        user,
        database,
        params,
    })
}

/// Frames one typed message (every message except `StartupMessage`/
/// `SSLRequest`/`CancelRequest`): `Byte1` type, `Int32` length (counting the
/// length field, not the type byte), then `length - 4` bytes of body. Returns
/// `(type, body, consumed)`, or `None` if a whole message is not yet present.
/// Used by the relay to watch for `ReadyForQuery` (`b'Z'`).
pub fn message(buf: &[u8]) -> Option<(u8, &[u8], usize)> {
    let typ = *buf.first()?;
    let len = u32::from_be_bytes(buf.get(1..5)?.try_into().ok()?) as usize;
    if len < 4 {
        return None;
    }
    let end = 1 + len;
    let body = buf.get(5..end)?;
    Some((typ, body, end))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn startup(pairs: &[(&str, &str)]) -> Vec<u8> {
        let mut body = Vec::new();
        for (k, v) in pairs {
            body.extend_from_slice(k.as_bytes());
            body.push(0);
            body.extend_from_slice(v.as_bytes());
            body.push(0);
        }
        body.push(0); // terminating empty key
        let len = (8 + body.len()) as u32;
        let mut out = len.to_be_bytes().to_vec();
        out.extend_from_slice(&PROTOCOL_V3.to_be_bytes());
        out.extend_from_slice(&body);
        out
    }

    #[test]
    fn ssl_request_is_recognised_and_not_a_startup() {
        assert!(is_ssl_request(&SSL_REQUEST));
        assert!(!is_ssl_request(&startup(&[("user", "a")])));
        assert!(!is_ssl_request(&[0, 0, 0, 8])); // too short
                                                 // An SSLRequest is not a v3 startup.
        assert_eq!(parse_startup(&SSL_REQUEST), None);
    }

    #[test]
    fn parses_a_startup_with_user_and_database() {
        let p = parse_startup(&startup(&[("user", "postgres"), ("database", "mydb")])).unwrap();
        assert_eq!(p.user, "postgres");
        assert_eq!(p.database.as_deref(), Some("mydb"));
        // An empty value (e.g. application_name) is kept, not a parse failure.
        let p = parse_startup(&startup(&[("user", "u"), ("application_name", "")])).unwrap();
        assert_eq!(p.user, "u");
        assert_eq!(p.database, None);
        assert!(p
            .params
            .iter()
            .any(|(k, v)| k == "application_name" && v.is_empty()));
    }

    #[test]
    fn rejects_malformed_startups() {
        let ok = startup(&[("user", "postgres"), ("database", "mydb")]);
        // Length runs past the buffer.
        assert_eq!(parse_startup(&ok[..ok.len() - 1]), None);
        // No `user`.
        assert_eq!(parse_startup(&startup(&[("database", "x")])), None);
        // Non-UTF-8 in a value.
        let mut bad = startup(&[("user", "x")]);
        // the value byte for "x" is the last before two NULs; flip it to 0xff
        let pos = bad.iter().rposition(|&b| b == b'x').unwrap();
        bad[pos] = 0xff;
        assert_eq!(parse_startup(&bad), None);
        // Too short to hold a header.
        assert_eq!(parse_startup(&[0, 0, 0, 9, 0, 3]), None);
    }

    #[test]
    fn frames_typed_messages() {
        // ReadyForQuery: 'Z', Int32(5), 'I'.
        let rfq = [b'Z', 0, 0, 0, 5, b'I'];
        let (typ, body, consumed) = message(&rfq).unwrap();
        assert_eq!((typ, body, consumed), (b'Z', &b"I"[..], 6));
        // A header without its body yet.
        assert_eq!(message(&[b'Z', 0, 0, 0, 5]), None);
        // Length too small to count itself.
        assert_eq!(message(&[b'Z', 0, 0, 0, 0]), None);
    }
}
