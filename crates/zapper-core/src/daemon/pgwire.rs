//! Postgres v3 wire pieces for database credential injection (proposal 07),
//! spike: enough of the frontend/backend framing to drive an auth-terminating
//! relay — detect the `SSLRequest`, read the `StartupMessage`, frame typed
//! messages (to find where the server reaches `ReadyForQuery`), the
//! `AuthenticationOk` the proxy sends the guest, and the server-side
//! SCRAM-SHA-256 exchange ([`scram`]) the proxy runs with the host-held
//! password. It does NOT parse queries; after auth the relay splices bytes.
//!
//! Spike: the parsers, builders and the SCRAM client here are tested (SCRAM
//! against the RFC 7677 vector) and the startup parser is fuzzed, but nothing
//! is wired into the proxy yet (that dispatch, plus a real-database e2e, are
//! the next steps — see `docs/proposals/07-db-credential-injection.md`).
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

/// The server's authentication request (`Byte1('R')`, `Int32` len, `Int32`
/// subtype, then subtype-specific data), as the relay needs to drive SCRAM.
/// `body` is the message body from [`message`] — the bytes after the length
/// field, i.e. the subtype `Int32` followed by its data.
#[derive(Debug, PartialEq)]
pub enum Auth<'a> {
    /// Subtype 0: authentication complete.
    Ok,
    /// Subtype 10: the server offers SASL mechanisms (NUL-terminated names,
    /// then a final empty name). We require `SCRAM-SHA-256`.
    Sasl { mechanisms: Vec<&'a str> },
    /// Subtype 11: `AuthenticationSASLContinue`, carrying the server-first
    /// message.
    SaslContinue(&'a [u8]),
    /// Subtype 12: `AuthenticationSASLFinal`, carrying the server-final message.
    SaslFinal(&'a [u8]),
    /// Any other subtype (cleartext/MD5/GSS/…): unsupported by this relay.
    Other(u32),
}

/// Parses an `Authentication*` message body (the bytes after the length field
/// of an `R` message). `None` if it is truncated or a mechanism name is not
/// UTF-8.
pub fn parse_auth(body: &[u8]) -> Option<Auth<'_>> {
    let subtype = u32::from_be_bytes(body.get(0..4)?.try_into().ok()?);
    let rest = &body[4..];
    Some(match subtype {
        0 => Auth::Ok,
        10 => {
            let mut names = Vec::new();
            let mut b = rest;
            loop {
                let (name, after) = cstr(b)?;
                if name.is_empty() {
                    break;
                }
                names.push(std::str::from_utf8(name).ok()?);
                b = after;
            }
            Auth::Sasl { mechanisms: names }
        }
        11 => Auth::SaslContinue(rest),
        12 => Auth::SaslFinal(rest),
        other => Auth::Other(other),
    })
}

/// SCRAM-SHA-256 client exchange (RFC 5802 / RFC 7677), as the proxy
/// authenticates to the real server with the host-held password. Pure — no I/O
/// and the nonce is supplied, so it is unit-tested against the RFC 7677 vector;
/// the relay drives it over the server leg and generates a random nonce.
pub mod scram {
    use base64::Engine;
    use ring::{digest, hmac, pbkdf2};
    use std::num::NonZeroU32;

    const SHA256_LEN: usize = 32;
    /// `base64("n,,")` — the GS2 channel-binding header for "no binding", which
    /// is what `c=` carries in the client-final message.
    const GS2_HEADER_B64: &str = "biws";

    fn b64() -> base64::engine::general_purpose::GeneralPurpose {
        base64::engine::general_purpose::STANDARD
    }

    fn hmac(key: &[u8], msg: &[u8]) -> [u8; SHA256_LEN] {
        let tag = hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, key), msg);
        tag.as_ref().try_into().expect("HMAC-SHA256 is 32 bytes")
    }

    fn sha256(data: &[u8]) -> [u8; SHA256_LEN] {
        digest::digest(&digest::SHA256, data)
            .as_ref()
            .try_into()
            .expect("SHA-256 is 32 bytes")
    }

    /// A SCRAM exchange in progress: built with the client-first message, then
    /// advanced with the server-first message to produce the client-final
    /// message and the server signature to verify.
    pub struct Client {
        password: String,
        client_nonce: String,
        /// `n=<user>,r=<client-nonce>` — the client-first-bare, which the
        /// `AuthMessage` concatenates.
        client_first_bare: String,
    }

    /// The client-final message plus the `ServerSignature` the server must
    /// echo in its `v=` for the exchange to be trusted.
    pub struct Final {
        /// `c=biws,r=…,p=…` — sent in the `SASLResponse`.
        pub message: String,
        /// The expected `v=` value (base64), compared against the server-final.
        pub server_signature: String,
    }

    impl Client {
        /// `user` is the SCRAM username (Postgres ignores it and uses the
        /// startup user, so the relay passes `""`, as libpq does); `nonce` is a
        /// fresh random client nonce.
        pub fn new(user: &str, password: &str, nonce: &str) -> Self {
            Self {
                password: password.to_string(),
                client_nonce: nonce.to_string(),
                client_first_bare: format!("n={user},r={nonce}"),
            }
        }

        /// The client-first message for the `SASLInitialResponse`: the `n,,`
        /// GS2 header (no channel binding) then the client-first-bare.
        pub fn client_first(&self) -> String {
            format!("n,,{}", self.client_first_bare)
        }

        /// Consumes the server-first message (`r=<nonce>,s=<salt>,i=<iters>`)
        /// and produces the client-final message and the expected server
        /// signature. `Err` on a malformed server-first, a nonce that does not
        /// extend ours (a tampered or wrong server), or absurd iterations.
        pub fn client_final(&self, server_first: &[u8]) -> Result<Final, String> {
            let sf = std::str::from_utf8(server_first)
                .map_err(|_| "server-first is not UTF-8".to_string())?;
            let (mut nonce, mut salt_b64, mut iters) = (None, None, None);
            for field in sf.split(',') {
                match field.split_at(field.find('=').ok_or("server-first field has no =")?) {
                    ("r", v) => nonce = Some(&v[1..]),
                    ("s", v) => salt_b64 = Some(&v[1..]),
                    ("i", v) => iters = Some(&v[1..]),
                    // m= (mandatory extension) would mean we must fail; any
                    // other field is ignored per RFC 5802.
                    ("m", _) => return Err("server demands an unsupported extension".into()),
                    _ => {}
                }
            }
            let nonce = nonce.ok_or("server-first has no nonce")?;
            let salt_b64 = salt_b64.ok_or("server-first has no salt")?;
            let iters: u32 = iters
                .ok_or("server-first has no iteration count")?
                .parse()
                .map_err(|_| "iteration count is not a number")?;
            // The server nonce must extend the client nonce we sent — otherwise
            // the server is not answering our challenge.
            if !nonce.starts_with(&self.client_nonce) || nonce.len() == self.client_nonce.len() {
                return Err("server nonce does not extend the client nonce".into());
            }
            let iterations = NonZeroU32::new(iters).ok_or("iteration count must be positive")?;
            // PBKDF2 cost is attacker-influenced only via a hostile server; cap
            // it so a bogus server can't pin a CPU.
            if iters > 1_000_000 {
                return Err("iteration count is implausibly large".into());
            }
            let salt = b64()
                .decode(salt_b64)
                .map_err(|_| "salt is not valid base64")?;

            let mut salted = [0u8; SHA256_LEN];
            pbkdf2::derive(
                pbkdf2::PBKDF2_HMAC_SHA256,
                iterations,
                &salt,
                self.password.as_bytes(),
                &mut salted,
            );
            let client_key = hmac(&salted, b"Client Key");
            let stored_key = sha256(&client_key);
            let server_key = hmac(&salted, b"Server Key");

            let client_final_bare = format!("c={GS2_HEADER_B64},r={nonce}");
            let auth_message = format!("{},{sf},{client_final_bare}", self.client_first_bare);
            let client_sig = hmac(&stored_key, auth_message.as_bytes());
            let proof: Vec<u8> = client_key
                .iter()
                .zip(client_sig.iter())
                .map(|(a, b)| a ^ b)
                .collect();
            let server_sig = hmac(&server_key, auth_message.as_bytes());

            Ok(Final {
                message: format!("{client_final_bare},p={}", b64().encode(proof)),
                server_signature: b64().encode(server_sig),
            })
        }

        /// Checks the server-final message (`v=<ServerSignature>`) against the
        /// signature [`client_final`](Self::client_final) computed. `Err` on an
        /// error field (`e=…`), a missing or non-matching `v=`.
        pub fn verify_server_final(final_: &Final, server_final: &[u8]) -> Result<(), String> {
            let sf = std::str::from_utf8(server_final)
                .map_err(|_| "server-final is not UTF-8".to_string())?;
            for field in sf.split(',') {
                if let Some(v) = field.strip_prefix("v=") {
                    return if v == final_.server_signature {
                        Ok(())
                    } else {
                        Err("server signature does not verify".into())
                    };
                }
                if let Some(e) = field.strip_prefix("e=") {
                    return Err(format!("server rejected authentication: {e}"));
                }
            }
            Err("server-final has no verifier".into())
        }
    }
}

/// Builds a `SASLInitialResponse` (frontend `Byte1('p')`): the mechanism name
/// (a C string), then `Int32` length of the initial response and its bytes.
pub fn sasl_initial_response(mechanism: &str, initial: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(mechanism.as_bytes());
    body.push(0);
    body.extend_from_slice(&(initial.len() as u32).to_be_bytes());
    body.extend_from_slice(initial);
    frame(b'p', &body)
}

/// Builds a `SASLResponse` (frontend `Byte1('p')`): just the response bytes.
pub fn sasl_response(data: &[u8]) -> Vec<u8> {
    frame(b'p', data)
}

/// Wraps `body` as a typed message: the type byte, the `Int32` length (counting
/// itself), then the body. Inverse of [`message`].
fn frame(typ: u8, body: &[u8]) -> Vec<u8> {
    let mut out = vec![typ];
    out.extend_from_slice(&((body.len() + 4) as u32).to_be_bytes());
    out.extend_from_slice(body);
    out
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

    /// `frame`/`message` are inverses, and the SASL builders carry the body a
    /// server would read back.
    #[test]
    fn frames_round_trip_and_sasl_builders_carry_their_body() {
        let msg = frame(b'p', b"hello");
        let (typ, body, consumed) = message(&msg).unwrap();
        assert_eq!((typ, body, consumed), (b'p', &b"hello"[..], msg.len()));

        let init = sasl_initial_response("SCRAM-SHA-256", b"n,,n=,r=abc");
        let (typ, body, _) = message(&init).unwrap();
        assert_eq!(typ, b'p');
        let (mech, after) = cstr(body).unwrap();
        assert_eq!(mech, b"SCRAM-SHA-256");
        let n = u32::from_be_bytes(after[0..4].try_into().unwrap()) as usize;
        assert_eq!(&after[4..4 + n], b"n,,n=,r=abc");

        let resp = sasl_response(b"c=biws,r=abc,p=xyz");
        let (typ, body, _) = message(&resp).unwrap();
        assert_eq!((typ, body), (b'p', &b"c=biws,r=abc,p=xyz"[..]));
    }

    #[test]
    fn parses_authentication_messages() {
        // AuthenticationOk body: Int32(0).
        assert_eq!(parse_auth(&[0, 0, 0, 0]), Some(Auth::Ok));
        // AuthenticationSASL: Int32(10), "SCRAM-SHA-256\0\0".
        let mut sasl = vec![0, 0, 0, 10];
        sasl.extend_from_slice(b"SCRAM-SHA-256\0\0");
        assert_eq!(
            parse_auth(&sasl),
            Some(Auth::Sasl {
                mechanisms: vec!["SCRAM-SHA-256"]
            })
        );
        // Continue/Final carry their payload; an unknown subtype is Other.
        let mut cont = vec![0, 0, 0, 11];
        cont.extend_from_slice(b"r=abc,s=salt,i=4096");
        assert_eq!(
            parse_auth(&cont),
            Some(Auth::SaslContinue(b"r=abc,s=salt,i=4096"))
        );
        assert_eq!(parse_auth(&[0, 0, 0, 5]), Some(Auth::Other(5)));
        // Truncated subtype.
        assert_eq!(parse_auth(&[0, 0]), None);
    }

    /// The RFC 7677 §3 worked example pins the whole computation: client-first,
    /// client proof, and the server signature we verify.
    #[test]
    fn scram_matches_rfc7677_vector() {
        let c = scram::Client::new("user", "pencil", "rOprNGfwEbeRWgbNEkqO");
        assert_eq!(c.client_first(), "n,,n=user,r=rOprNGfwEbeRWgbNEkqO");

        let server_first =
            b"r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096";
        let fin = c.client_final(server_first).unwrap();
        assert_eq!(
            fin.message,
            "c=biws,r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,\
             p=dHzbZapWIk4jUhN+Ute9ytag9zjfMHgsqmmiz7AndVQ="
        );
        assert_eq!(
            fin.server_signature,
            "6rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4="
        );

        // The matching server-final verifies; a wrong one and an error do not.
        let ok = format!("v={}", fin.server_signature);
        assert!(scram::Client::verify_server_final(&fin, ok.as_bytes()).is_ok());
        assert!(scram::Client::verify_server_final(&fin, b"v=AAAA").is_err());
        assert!(scram::Client::verify_server_final(&fin, b"e=other-error").is_err());
    }

    #[test]
    fn scram_rejects_a_server_that_does_not_echo_our_nonce() {
        let c = scram::Client::new("", "pencil", "clientnonce");
        // Server nonce must start with the client nonce and be longer.
        assert!(c
            .client_final(b"r=wrongnonce,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096")
            .is_err());
        assert!(c
            .client_final(b"r=clientnonce,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096")
            .is_err());
        // Missing fields and a non-numeric iteration count are rejected.
        assert!(c
            .client_final(b"s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096")
            .is_err());
        assert!(c
            .client_final(b"r=clientnonceX,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=lots")
            .is_err());
    }
}
