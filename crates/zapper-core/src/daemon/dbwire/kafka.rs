//! Minimal Kafka protocol bits for the Kafka auth-relay (proposal 07): the
//! proxy authenticates the server leg with SASL — `SCRAM-SHA-256` (reusing
//! [`super::pgwire::scram`]), `PLAIN`, or `OAUTHBEARER` (a bearer token) —
//! carried over `SaslHandshake`/`SaslAuthenticate` (KIP-152), then the relay
//! splices. The guest holds no credential and connects as a plaintext Kafka
//! client; its requests run on the pre-authenticated connection.
//!
//! All integers are big-endian. Only the non-flexible `SaslHandshake` v1 and
//! `SaslAuthenticate` v1 request/response headers are used, so there are no
//! tagged fields to encode.

use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

use super::pgwire::scram;

const API_SASL_HANDSHAKE: i16 = 17;
const API_SASL_AUTHENTICATE: i16 = 36;
const MAX_RESPONSE: usize = 100 * 1024 * 1024;

fn put_string(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as i16).to_be_bytes());
    out.extend_from_slice(s.as_bytes());
}

fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    out.extend_from_slice(&(b.len() as i32).to_be_bytes());
    out.extend_from_slice(b);
}

/// A request: `Int32` size, then the v1 request header (api key, api version,
/// correlation id, a null client id) and the body.
fn request(api_key: i16, api_version: i16, correlation: i32, body: &[u8]) -> Vec<u8> {
    let mut hdr = Vec::new();
    hdr.extend_from_slice(&api_key.to_be_bytes());
    hdr.extend_from_slice(&api_version.to_be_bytes());
    hdr.extend_from_slice(&correlation.to_be_bytes());
    hdr.extend_from_slice(&(-1i16).to_be_bytes()); // client id: null
    hdr.extend_from_slice(body);
    let mut out = (hdr.len() as i32).to_be_bytes().to_vec();
    out.extend_from_slice(&hdr);
    out
}

/// Reads one response and returns its body (the bytes after the `Int32` size
/// and the `Int32` correlation id of the v0 response header).
pub(crate) async fn read_response<S: AsyncRead + Unpin>(
    s: &mut S,
    buf: &mut Vec<u8>,
) -> Result<Vec<u8>, String> {
    super::read_framed(s, buf, 4 + MAX_RESPONSE, |b| {
        if b.len() < 4 {
            return Ok(None);
        }
        let size = i32::from_be_bytes(b[0..4].try_into().unwrap());
        if !(4..=MAX_RESPONSE as i32).contains(&size) {
            return Err("implausible Kafka response size".into());
        }
        let size = size as usize;
        if b.len() < 4 + size {
            return Ok(None);
        }
        // Body is the bytes after the size and the correlation id.
        Ok(Some((b[8..4 + size].to_vec(), 4 + size)))
    })
    .await
}

fn get_i16(b: &[u8], off: usize) -> Option<i16> {
    Some(i16::from_be_bytes(b.get(off..off + 2)?.try_into().ok()?))
}

/// Reads a Kafka `BYTES` field (`Int32` length, or -1 for null) at `off`.
fn get_bytes(b: &[u8], off: usize) -> Option<Vec<u8>> {
    let len = i32::from_be_bytes(b.get(off..off + 4)?.try_into().ok()?);
    if len < 0 {
        return Some(Vec::new());
    }
    b.get(off + 4..off + 4 + len as usize).map(<[u8]>::to_vec)
}

/// The SASL/OAUTHBEARER initial client response (RFC 7628):
/// `n,, ^A auth=Bearer <token> ^A ^A`.
fn oauthbearer_token(token: &str) -> Vec<u8> {
    format!("n,,\x01auth=Bearer {token}\x01\x01").into_bytes()
}

/// Parses a `SaslAuthenticate` response body: `Int16` error code, nullable
/// error message, then the auth `BYTES`. Returns the auth bytes, or an error
/// with the broker's message.
pub(crate) fn auth_bytes(body: &[u8]) -> Result<Vec<u8>, String> {
    let code = get_i16(body, 0).ok_or("truncated SaslAuthenticate response")?;
    // Nullable error message string: Int16 length, -1 for null.
    let mlen = get_i16(body, 2).ok_or("truncated SaslAuthenticate response")?;
    let mut off = 4;
    let message = if mlen >= 0 {
        let m = body
            .get(off..off + mlen as usize)
            .map(|s| String::from_utf8_lossy(s).into_owned());
        off += mlen as usize;
        m
    } else {
        None
    };
    if code != 0 {
        return Err(format!(
            "broker rejected SASL ({code}): {}",
            message.unwrap_or_default()
        ));
    }
    get_bytes(body, off).ok_or_else(|| "truncated SaslAuthenticate auth bytes".into())
}

/// Authenticates the server leg with the chosen SASL `mechanism`. `user`/
/// `password` are the credential (for `OAUTHBEARER` the password is the bearer
/// token; the user is unused). Fails closed on a refused mechanism, a rejected
/// exchange, or a SCRAM mismatch.
pub async fn authenticate<S: AsyncRead + AsyncWrite + Unpin>(
    server: &mut S,
    mechanism: &str,
    user: &str,
    password: &str,
    nonce: &str,
) -> Result<(), String> {
    let mut buf = Vec::new();

    // SaslHandshake: announce the mechanism.
    let mut body = Vec::new();
    put_string(&mut body, mechanism);
    server
        .write_all(&request(API_SASL_HANDSHAKE, 1, 0, &body))
        .await
        .map_err(|e| format!("sending SaslHandshake: {e}"))?;
    let resp = read_response(server, &mut buf).await?;
    match get_i16(&resp, 0) {
        Some(0) => {}
        Some(code) => {
            return Err(format!(
                "broker refused SASL mechanism {mechanism} (error {code})"
            ))
        }
        None => return Err("truncated SaslHandshake response".into()),
    }

    let send_auth = |corr: i32, token: Vec<u8>| {
        let mut b = Vec::new();
        put_bytes(&mut b, &token);
        request(API_SASL_AUTHENTICATE, 1, corr, &b)
    };

    match mechanism {
        "PLAIN" => {
            server
                .write_all(&send_auth(1, super::sasl_plain_token(user, password)))
                .await
                .map_err(|e| format!("sending SASL PLAIN: {e}"))?;
            auth_bytes(&read_response(server, &mut buf).await?)?;
        }
        "OAUTHBEARER" => {
            server
                .write_all(&send_auth(1, oauthbearer_token(password)))
                .await
                .map_err(|e| format!("sending SASL OAUTHBEARER: {e}"))?;
            auth_bytes(&read_response(server, &mut buf).await?)?;
        }
        "SCRAM-SHA-256" | "SCRAM-SHA-512" => {
            let hash = if mechanism == "SCRAM-SHA-512" {
                scram::Hash::Sha512
            } else {
                scram::Hash::Sha256
            };
            let client = scram::Client::with_hash(hash, user, password, nonce);
            server
                .write_all(&send_auth(1, client.client_first().into_bytes()))
                .await
                .map_err(|e| format!("sending SCRAM client-first: {e}"))?;
            let server_first = auth_bytes(&read_response(server, &mut buf).await?)?;
            let fin = client.client_final(&server_first)?;
            server
                .write_all(&send_auth(2, fin.message.clone().into_bytes()))
                .await
                .map_err(|e| format!("sending SCRAM client-final: {e}"))?;
            let server_final = auth_bytes(&read_response(server, &mut buf).await?)?;
            scram::Client::verify_server_final(&fin, &server_final)?;
        }
        other => return Err(format!("unsupported SASL mechanism {other}")),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    #[test]
    fn tokens_have_the_sasl_shapes() {
        assert_eq!(super::super::sasl_plain_token("u", "p"), b"\0u\0p");
        assert_eq!(oauthbearer_token("tok"), b"n,,\x01auth=Bearer tok\x01\x01");
    }

    /// Reads one request, returning (api_key, correlation, body-after-header).
    async fn read_request<S: AsyncRead + Unpin>(
        s: &mut S,
        buf: &mut Vec<u8>,
    ) -> (i16, i32, Vec<u8>) {
        let mut chunk = [0u8; 4096];
        loop {
            if buf.len() >= 4 {
                let size = i32::from_be_bytes(buf[0..4].try_into().unwrap()) as usize;
                if buf.len() >= 4 + size {
                    let msg = buf[4..4 + size].to_vec();
                    buf.drain(..4 + size);
                    let api_key = i16::from_be_bytes(msg[0..2].try_into().unwrap());
                    let corr = i32::from_be_bytes(msg[4..8].try_into().unwrap());
                    // header: api_key(2) api_ver(2) corr(4) client_id(nullable str)
                    let clen = i16::from_be_bytes(msg[8..10].try_into().unwrap());
                    let body_start = if clen < 0 { 10 } else { 10 + clen as usize };
                    return (api_key, corr, msg[body_start..].to_vec());
                }
            }
            let n = s.read(&mut chunk).await.unwrap();
            assert!(n > 0, "unexpected EOF");
            buf.extend_from_slice(&chunk[..n]);
        }
    }

    fn response(correlation: i32, body: &[u8]) -> Vec<u8> {
        let mut inner = correlation.to_be_bytes().to_vec();
        inner.extend_from_slice(body);
        let mut out = (inner.len() as i32).to_be_bytes().to_vec();
        out.extend_from_slice(&inner);
        out
    }

    fn handshake_ok() -> Vec<u8> {
        // error_code 0, then an array with one mechanism.
        let mut b = 0i16.to_be_bytes().to_vec();
        b.extend_from_slice(&1i32.to_be_bytes());
        b.extend_from_slice(&("SCRAM-SHA-256".len() as i16).to_be_bytes());
        b.extend_from_slice(b"SCRAM-SHA-256");
        b
    }

    fn auth_ok(auth: &[u8]) -> Vec<u8> {
        let mut b = 0i16.to_be_bytes().to_vec(); // error code
        b.extend_from_slice(&(-1i16).to_be_bytes()); // null error message
        put_bytes(&mut b, auth);
        b.extend_from_slice(&0i64.to_be_bytes()); // session lifetime
        b
    }

    #[tokio::test]
    async fn plain_authentication_sends_the_token_and_succeeds() {
        let (mut client, server) = tokio::io::duplex(4096);
        let srv = tokio::spawn(async move {
            let mut server = server;
            let mut buf = Vec::new();
            let (api, corr, body) = read_request(&mut server, &mut buf).await;
            assert_eq!(api, API_SASL_HANDSHAKE);
            // body = string(mechanism)
            let mlen = i16::from_be_bytes(body[0..2].try_into().unwrap()) as usize;
            assert_eq!(&body[2..2 + mlen], b"PLAIN");
            server
                .write_all(&response(corr, &handshake_ok()))
                .await
                .unwrap();
            let (api, corr, body) = read_request(&mut server, &mut buf).await;
            assert_eq!(api, API_SASL_AUTHENTICATE);
            assert_eq!(get_bytes(&body, 0).unwrap(), b"\0app\0s3cret");
            server
                .write_all(&response(corr, &auth_ok(b"")))
                .await
                .unwrap();
        });
        authenticate(&mut client, "PLAIN", "app", "s3cret", "")
            .await
            .unwrap();
        srv.await.unwrap();
    }

    #[tokio::test]
    async fn scram_authentication_completes_over_sasl_authenticate() {
        use super::super::scram_test;
        let (mut client, server) = tokio::io::duplex(8192);
        let password = "pencil";
        let srv = tokio::spawn(async move {
            let mut server = server;
            let mut buf = Vec::new();
            let (_api, corr, _body) = read_request(&mut server, &mut buf).await;
            server
                .write_all(&response(corr, &handshake_ok()))
                .await
                .unwrap();

            // client-first
            let (_api, corr, body) = read_request(&mut server, &mut buf).await;
            let client_first = String::from_utf8(get_bytes(&body, 0).unwrap()).unwrap();
            let scram = scram_test::Server::new(password, &client_first);
            server
                .write_all(&response(corr, &auth_ok(scram.server_first().as_bytes())))
                .await
                .unwrap();

            // client-final
            let (_api, corr, body) = read_request(&mut server, &mut buf).await;
            let client_final = String::from_utf8(get_bytes(&body, 0).unwrap()).unwrap();
            let server_final = scram.server_final(&client_final).expect("proof verifies");
            server
                .write_all(&response(corr, &auth_ok(server_final.as_bytes())))
                .await
                .unwrap();
        });
        authenticate(&mut client, "SCRAM-SHA-256", "app", password, "kafkanonce")
            .await
            .unwrap();
        srv.await.unwrap();
    }

    #[tokio::test]
    async fn refused_mechanism_fails_closed() {
        let (mut client, server) = tokio::io::duplex(4096);
        let srv = tokio::spawn(async move {
            let mut server = server;
            let mut buf = Vec::new();
            let (_api, corr, _body) = read_request(&mut server, &mut buf).await;
            // error_code 33 (UNSUPPORTED_SASL_MECHANISM), empty mechanism list.
            let mut b = 33i16.to_be_bytes().to_vec();
            b.extend_from_slice(&0i32.to_be_bytes());
            server.write_all(&response(corr, &b)).await.unwrap();
        });
        let err = authenticate(&mut client, "PLAIN", "u", "p", "")
            .await
            .unwrap_err();
        assert!(err.contains("refused SASL mechanism"), "got: {err}");
        srv.await.unwrap();
    }
}
