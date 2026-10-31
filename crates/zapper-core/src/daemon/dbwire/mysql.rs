//! Minimal MySQL/MariaDB protocol bits for the MySQL auth-relay.
//!
//! MySQL's server speaks first, so the proxy plays both roles: it sends the
//! guest an Initial Handshake (advertising `mysql_native_password`, no TLS),
//! reads the guest's response (the guest has no password, so it is ignored),
//! and separately authenticates to the real server with the host-held
//! credential — computing the `mysql_native_password` or `caching_sha2_password`
//! scramble, upgrading the server leg to TLS mid-handshake when required — then
//! sends the guest an OK and the relay splices.
//!
//! All integers are little-endian. Only the handshake is modelled.

use ring::digest;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

const CLIENT_LONG_PASSWORD: u32 = 0x0000_0001;
const CLIENT_CONNECT_WITH_DB: u32 = 0x0000_0008;
const CLIENT_PROTOCOL_41: u32 = 0x0000_0200;
pub const CLIENT_SSL: u32 = 0x0000_0800;
const CLIENT_SECURE_CONNECTION: u32 = 0x0000_8000;
const CLIENT_PLUGIN_AUTH: u32 = 0x0008_0000;

const MAX_PACKET: u32 = 16 * 1024 * 1024;
const CHARSET_UTF8: u8 = 0x21;

/// Frames a payload: 3-byte little-endian length, 1-byte sequence, payload.
/// A 3-byte little-endian length, as MySQL packet headers carry it.
fn u24_le(b: &[u8]) -> usize {
    (b[0] as usize) | (b[1] as usize) << 8 | (b[2] as usize) << 16
}

pub fn packet(seq: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = (payload.len() as u32).to_le_bytes()[..3].to_vec();
    out.push(seq);
    out.extend_from_slice(payload);
    out
}

/// Reads one framed packet, buffering partial reads in `buf`. Returns
/// `(sequence, payload)`.
pub async fn read_packet<S: AsyncRead + Unpin>(
    s: &mut S,
    buf: &mut Vec<u8>,
) -> Result<(u8, Vec<u8>), String> {
    super::read_framed(s, buf, 4 + MAX_PACKET as usize, |b| {
        if b.len() < 4 {
            return Ok(None);
        }
        let len = u24_le(b);
        if len > MAX_PACKET as usize {
            return Err("MySQL packet too large".into());
        }
        if b.len() < 4 + len {
            return Ok(None);
        }
        Ok(Some(((b[3], b[4..4 + len].to_vec()), 4 + len)))
    })
    .await
}

fn sha1(d: &[u8]) -> Vec<u8> {
    digest::digest(&digest::SHA1_FOR_LEGACY_USE_ONLY, d)
        .as_ref()
        .to_vec()
}
fn sha256(d: &[u8]) -> Vec<u8> {
    digest::digest(&digest::SHA256, d).as_ref().to_vec()
}
fn xor(a: &[u8], b: &[u8]) -> Vec<u8> {
    a.iter().zip(b.iter()).map(|(x, y)| x ^ y).collect()
}

/// `mysql_native_password`: `SHA1(pw) XOR SHA1(salt + SHA1(SHA1(pw)))`.
fn native_scramble(password: &str, salt: &[u8]) -> Vec<u8> {
    if password.is_empty() {
        return Vec::new();
    }
    let h1 = sha1(password.as_bytes());
    let mut cat = salt[..20.min(salt.len())].to_vec();
    cat.extend_from_slice(&sha1(&h1));
    xor(&h1, &sha1(&cat))
}

/// `caching_sha2_password` fast scramble:
/// `SHA256(pw) XOR SHA256(SHA256(SHA256(pw)) + salt)`.
fn caching_sha2_scramble(password: &str, salt: &[u8]) -> Vec<u8> {
    if password.is_empty() {
        return Vec::new();
    }
    let h1 = sha256(password.as_bytes());
    let mut cat = sha256(&h1);
    cat.extend_from_slice(&salt[..20.min(salt.len())]);
    xor(&h1, &sha256(&cat))
}

/// The auth response for `plugin` (defaulting to native for an empty/unknown
/// plugin name).
pub fn auth_response(plugin: &str, password: &str, salt: &[u8]) -> Vec<u8> {
    match plugin {
        "caching_sha2_password" => caching_sha2_scramble(password, salt),
        _ => native_scramble(password, salt),
    }
}

/// A server's Initial Handshake packet (protocol 10), as the proxy sends the
/// guest and as the test's fake server sends the proxy. `CLIENT_SSL` is
/// advertised so a TLS-capable client can request encryption.
pub fn initial_handshake(scramble: &[u8; 20], plugin: &str) -> Vec<u8> {
    let caps = CLIENT_LONG_PASSWORD
        | CLIENT_PROTOCOL_41
        | CLIENT_SECURE_CONNECTION
        | CLIENT_PLUGIN_AUTH
        | CLIENT_SSL;
    let mut p = vec![10];
    p.extend_from_slice(b"8.0.0-zapper-relay\0");
    p.extend_from_slice(&1u32.to_le_bytes()); // connection id
    p.extend_from_slice(&scramble[..8]); // auth-plugin-data-part-1
    p.push(0); // filler
    p.extend_from_slice(&(caps as u16).to_le_bytes()); // capability low
    p.push(CHARSET_UTF8);
    p.extend_from_slice(&0x0002u16.to_le_bytes()); // status flags
    p.extend_from_slice(&((caps >> 16) as u16).to_le_bytes()); // capability high
    p.push(21); // auth-plugin-data length
    p.extend_from_slice(&[0u8; 10]); // reserved
    p.extend_from_slice(&scramble[8..20]); // auth-plugin-data-part-2 (12)
    p.push(0); // its NUL
    p.extend_from_slice(plugin.as_bytes());
    p.push(0);
    p
}

/// A protocol-41 OK packet.
pub fn ok_packet() -> Vec<u8> {
    vec![0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00]
}

/// A parsed Initial Handshake: the 20-byte scramble, the auth plugin, and the
/// server's capability flags.
pub struct Handshake {
    pub scramble: Vec<u8>,
    pub plugin: String,
    pub capabilities: u32,
}

/// Parses a server Initial Handshake (protocol 10).
pub fn parse_initial(p: &[u8]) -> Result<Handshake, String> {
    let bad = || "malformed MySQL initial handshake".to_string();
    if p.first() != Some(&10) {
        return Err("unsupported MySQL handshake protocol version".into());
    }
    let mut i = 1;
    i += p
        .get(i..)
        .ok_or_else(bad)?
        .iter()
        .position(|&b| b == 0)
        .ok_or_else(bad)?
        + 1; // server version
    i += 4; // connection id
    let mut scramble = p.get(i..i + 8).ok_or_else(bad)?.to_vec();
    i += 8 + 1; // part 1 + filler
    let cap_low = u16::from_le_bytes(p.get(i..i + 2).ok_or_else(bad)?.try_into().unwrap());
    i += 2 + 1 + 2; // caps low + charset + status
    let cap_high = u16::from_le_bytes(p.get(i..i + 2).ok_or_else(bad)?.try_into().unwrap());
    i += 2;
    let caps = (cap_high as u32) << 16 | cap_low as u32;
    let apdl = *p.get(i).ok_or_else(bad)? as usize;
    i += 1 + 10; // length + reserved
    let part2 = (apdl.saturating_sub(8)).max(13);
    scramble.extend_from_slice(p.get(i..i + part2).ok_or_else(bad)?);
    i += part2;
    while scramble.last() == Some(&0) {
        scramble.pop();
    }
    scramble.truncate(20);
    let plugin = if caps & CLIENT_PLUGIN_AUTH != 0 {
        let rest = p.get(i..).unwrap_or(&[]);
        let end = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
        String::from_utf8_lossy(&rest[..end]).into_owned()
    } else {
        "mysql_native_password".into()
    };
    Ok(Handshake {
        scramble,
        plugin,
        capabilities: caps,
    })
}

/// The 32-byte SSLRequest payload (a handshake-response prefix with CLIENT_SSL
/// set), sent before the TLS upgrade on the server leg.
pub fn ssl_request(database: bool) -> Vec<u8> {
    client_prefix(response_caps(true, database))
}

fn response_caps(tls: bool, database: bool) -> u32 {
    let mut caps =
        CLIENT_LONG_PASSWORD | CLIENT_PROTOCOL_41 | CLIENT_SECURE_CONNECTION | CLIENT_PLUGIN_AUTH;
    if tls {
        caps |= CLIENT_SSL;
    }
    if database {
        caps |= CLIENT_CONNECT_WITH_DB;
    }
    caps
}

fn client_prefix(caps: u32) -> Vec<u8> {
    let mut p = caps.to_le_bytes().to_vec();
    p.extend_from_slice(&MAX_PACKET.to_le_bytes());
    p.push(CHARSET_UTF8);
    p.extend_from_slice(&[0u8; 23]); // reserved
    p
}

/// The client Handshake Response (protocol 41): capability prefix, username,
/// length-prefixed auth response, optional database and the auth plugin name.
pub fn handshake_response(
    tls: bool,
    user: &str,
    auth: &[u8],
    database: Option<&str>,
    plugin: &str,
) -> Vec<u8> {
    let mut p = client_prefix(response_caps(tls, database.is_some()));
    p.extend_from_slice(user.as_bytes());
    p.push(0);
    p.push(auth.len() as u8); // CLIENT_SECURE_CONNECTION: 1-byte length
    p.extend_from_slice(auth);
    if let Some(db) = database {
        p.extend_from_slice(db.as_bytes());
        p.push(0);
    }
    p.extend_from_slice(plugin.as_bytes());
    p.push(0);
    p
}

/// Whether a client's first packet is an SSLRequest — the 32-byte
/// handshake-response prefix (capabilities, max packet, charset, reserved) with
/// `CLIENT_SSL` set and no username following, i.e. the client wants to upgrade
/// to TLS before sending its real handshake response.
pub fn is_ssl_request(packet: &[u8]) -> bool {
    packet.len() == 32
        && packet
            .get(0..4)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap()) & CLIENT_SSL != 0)
            .unwrap_or(false)
}

/// Parses an ERR packet (`0xff`, `Int16` code, `#`+SQLSTATE, message).
fn err_packet(p: &[u8]) -> String {
    let code = p
        .get(1..3)
        .map(|b| u16::from_le_bytes(b.try_into().unwrap()));
    // Skip the SQLSTATE marker '#' and 5-byte state if present.
    let msg_start = if p.get(3) == Some(&b'#') { 9 } else { 3 };
    let msg = String::from_utf8_lossy(p.get(msg_start..).unwrap_or(&[]));
    match code {
        Some(c) => format!("server error {c}: {msg}"),
        None => format!("server error: {msg}"),
    }
}

/// Completes the server-leg auth after the handshake response has been sent:
/// handles the final OK/ERR, an AuthSwitchRequest (recompute for the new
/// plugin and salt), and `caching_sha2_password`'s AuthMoreData — fast-auth
/// success, or full auth, which this relay only does over TLS (sending the
/// cleartext password) and otherwise fails closed. `is_tls` is whether the
/// server leg is encrypted.
pub async fn complete_auth<S: AsyncRead + AsyncWrite + Unpin>(
    s: &mut S,
    buf: &mut Vec<u8>,
    password: &str,
    is_tls: bool,
) -> Result<(), String> {
    loop {
        let (seq, p) = read_packet(s, buf).await?;
        match p.first() {
            Some(0x00) => return Ok(()), // OK
            Some(0xff) => return Err(err_packet(&p)),
            Some(0xfe) => {
                // AuthSwitchRequest: 0xfe, plugin C string, salt.
                let rest = &p[1..];
                let nul = rest
                    .iter()
                    .position(|&b| b == 0)
                    .ok_or("malformed AuthSwitchRequest")?;
                let plugin = String::from_utf8_lossy(&rest[..nul]).into_owned();
                let mut salt = &rest[nul + 1..];
                if salt.last() == Some(&0) {
                    salt = &salt[..salt.len() - 1];
                }
                let resp = auth_response(&plugin, password, salt);
                s.write_all(&packet(seq + 1, &resp))
                    .await
                    .map_err(|e| format!("sending auth-switch response: {e}"))?;
            }
            Some(0x01) => match p.get(1) {
                Some(0x03) => {} // caching_sha2 fast-auth success; OK follows
                Some(0x04) => {
                    if !is_tls {
                        return Err("caching_sha2_password full authentication needs a TLS server leg; mark the dbauth secret plaintext only on a trusted network or ensure the server offers TLS".into());
                    }
                    let mut cleartext = password.as_bytes().to_vec();
                    cleartext.push(0);
                    s.write_all(&packet(seq + 1, &cleartext))
                        .await
                        .map_err(|e| format!("sending full-auth password: {e}"))?;
                }
                _ => return Err("unexpected AuthMoreData during MySQL auth".into()),
            },
            _ => return Err("unexpected packet during MySQL auth".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn packet_round_trips() {
        let bytes = packet(5, b"hello");
        let mut buf = Vec::new();
        let (seq, payload) = read_packet(&mut &bytes[..], &mut buf).await.unwrap();
        assert_eq!((seq, payload), (5, b"hello".to_vec()));
    }

    #[test]
    fn initial_handshake_parses_back() {
        let scramble = *b"0123456789abcdefghij";
        let hs = parse_initial(&initial_handshake(&scramble, "caching_sha2_password")).unwrap();
        assert_eq!(hs.scramble, scramble.to_vec());
        assert_eq!(hs.plugin, "caching_sha2_password");
        assert!(hs.capabilities & CLIENT_PLUGIN_AUTH != 0);
    }

    #[test]
    fn detects_an_ssl_request_packet() {
        // 32-byte prefix with CLIENT_SSL set and no username → SSLRequest.
        assert!(is_ssl_request(&ssl_request(false)));
        // A full handshake response (has a username) is not one.
        assert!(!is_ssl_request(&handshake_response(
            true,
            "user",
            b"",
            None,
            "mysql_native_password"
        )));
        // The same 32 bytes without CLIENT_SSL is not one either.
        let mut no_ssl = ssl_request(false);
        let caps = u32::from_le_bytes(no_ssl[0..4].try_into().unwrap()) & !CLIENT_SSL;
        no_ssl[0..4].copy_from_slice(&caps.to_le_bytes());
        assert!(!is_ssl_request(&no_ssl));
    }

    #[test]
    fn scrambles_are_deterministic_and_sized() {
        let salt = b"0123456789abcdefghij";
        assert_eq!(native_scramble("pw", salt).len(), 20);
        assert_eq!(caching_sha2_scramble("pw", salt).len(), 32);
        // Empty password yields an empty response (no auth material).
        assert!(native_scramble("", salt).is_empty());
    }

    /// Pulls the auth response out of a client handshake-response payload.
    fn auth_from_response(resp: &[u8]) -> Vec<u8> {
        let mut i = 4 + 4 + 1 + 23; // caps, max packet, charset, reserved
        i += resp[i..].iter().position(|&b| b == 0).unwrap() + 1; // username
        let len = resp[i] as usize;
        resp[i + 1..i + 1 + len].to_vec()
    }

    /// A fake MySQL server requiring `plugin` with `password`; on a good
    /// scramble it sends (for caching_sha2) a fast-auth-success AuthMoreData
    /// then OK, else OK; on a bad one an ERR.
    async fn fake_mysql<S: AsyncRead + AsyncWrite + Unpin>(mut c: S, plugin: &str, password: &str) {
        let scramble = *b"ABCDEFGH0123456789ij";
        c.write_all(&packet(0, &initial_handshake(&scramble, plugin)))
            .await
            .unwrap();
        let mut buf = Vec::new();
        let (seq, resp) = read_packet(&mut c, &mut buf).await.unwrap();
        assert_eq!(seq, 1);
        let got = auth_from_response(&resp);
        if got != auth_response(plugin, password, &scramble) {
            let mut err = vec![0xff];
            err.extend_from_slice(&1045u16.to_le_bytes());
            err.extend_from_slice(b"#28000Access denied");
            c.write_all(&packet(seq + 1, &err)).await.unwrap();
            return;
        }
        if plugin == "caching_sha2_password" {
            c.write_all(&packet(seq + 1, &[0x01, 0x03])).await.unwrap();
            c.write_all(&packet(seq + 2, &ok_packet())).await.unwrap();
        } else {
            c.write_all(&packet(seq + 1, &ok_packet())).await.unwrap();
        }
    }

    async fn run_server_auth(plugin: &str, server_pw: &str, proxy_pw: &str) -> Result<(), String> {
        let (mut proxy, server) = tokio::io::duplex(8192);
        let (p, s) = (plugin.to_string(), server_pw.to_string());
        let srv = tokio::spawn(async move { fake_mysql(server, &p, &s).await });
        let mut buf = Vec::new();
        let (_seq, initial) = read_packet(&mut proxy, &mut buf).await.unwrap();
        let hs = parse_initial(&initial).unwrap();
        let auth = auth_response(&hs.plugin, proxy_pw, &hs.scramble);
        let resp = handshake_response(false, "app", &auth, Some("db"), &hs.plugin);
        proxy.write_all(&packet(1, &resp)).await.unwrap();
        let r = complete_auth(&mut proxy, &mut buf, proxy_pw, false).await;
        srv.await.unwrap();
        r
    }

    #[tokio::test]
    async fn server_auth_completes_for_caching_sha2_fast_path() {
        run_server_auth("caching_sha2_password", "pencil", "pencil")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn server_auth_completes_for_native_password() {
        run_server_auth("mysql_native_password", "pencil", "pencil")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn server_auth_fails_closed_on_a_wrong_password() {
        let err = run_server_auth("mysql_native_password", "real", "wrong")
            .await
            .unwrap_err();
        assert!(err.contains("Access denied"), "got: {err}");
    }
}
