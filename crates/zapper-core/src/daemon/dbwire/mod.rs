//! The database wire layer (proposal 07): a per-protocol codec module for each
//! supported database — the parsing/building and the auth handshake it drives —
//! plus the shared plumbing they all use. `proxy::relay` is the datapath that
//! orchestrates these; nothing here knows about the proxy.
//!
//! The shared plumbing: every relay reads length-prefixed messages off a socket
//! the same way — accumulate bytes, try to frame one whole message, read more
//! when short, bound the buffer and fail closed on a close. [`read_framed`] is
//! that loop, once; each protocol supplies only how to frame one message.

pub(crate) mod cassandra;
pub(crate) mod kafka;
pub(crate) mod mongodb;
pub(crate) mod mysql;
pub(crate) mod pgwire;
pub(crate) mod redis;
pub(crate) mod tds;

use tokio::io::{AsyncRead, AsyncReadExt};

/// Reads from `stream` into `buf` until `frame` yields one whole message.
///
/// `frame` inspects the buffered bytes and returns:
/// - `Ok(Some((value, consumed)))` — a message is present; the loop drains
///   `consumed` bytes and returns `value`.
/// - `Ok(None)` — more bytes are needed.
/// - `Err(_)` — the bytes are malformed (e.g. a length past any sane bound).
///
/// It fails closed if the peer closes before a whole message arrives, or if the
/// buffer would grow past `max` without one — so a protocol's own early length
/// check (returning `Err`) is what bounds a legitimate large frame; `max` is
/// the backstop against a peer that never completes one.
pub(crate) async fn read_framed<S, T>(
    stream: &mut S,
    buf: &mut Vec<u8>,
    max: usize,
    mut frame: impl FnMut(&[u8]) -> Result<Option<(T, usize)>, String>,
) -> Result<T, String>
where
    S: AsyncRead + Unpin,
{
    let mut chunk = [0u8; 4096];
    loop {
        if let Some((value, consumed)) = frame(buf)? {
            buf.drain(..consumed);
            return Ok(value);
        }
        if buf.len() > max {
            return Err("handshake message too large".into());
        }
        let n = stream
            .read(&mut chunk)
            .await
            .map_err(|e| format!("reading from the connection: {e}"))?;
        if n == 0 {
            return Err("connection closed during the handshake".into());
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// The server half of a SCRAM-SHA-256 exchange, for the relays' handshake
/// tests — shared so each protocol's fake server supplies only its own framing.
#[cfg(test)]
pub(crate) mod scram_test {
    use base64::Engine as _;
    use ring::{digest, hmac, pbkdf2};
    use std::num::NonZeroU32;

    fn b64() -> base64::engine::general_purpose::GeneralPurpose {
        base64::engine::general_purpose::STANDARD
    }
    fn hmac256(key: &[u8], msg: &[u8]) -> [u8; 32] {
        hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, key), msg)
            .as_ref()
            .try_into()
            .unwrap()
    }
    fn sha256(d: &[u8]) -> [u8; 32] {
        digest::digest(&digest::SHA256, d)
            .as_ref()
            .try_into()
            .unwrap()
    }

    /// A fake SCRAM-SHA-256 server bound to a password and a client-first
    /// message: it yields the server-first, and from a client-final it checks
    /// the proof and produces the server-final verifier.
    pub(crate) struct Server {
        password: String,
        salt: Vec<u8>,
        iters: u32,
        client_first_bare: String,
        server_nonce: String,
        server_first: String,
    }

    impl Server {
        pub(crate) fn new(password: &str, client_first: &str) -> Self {
            let bare = client_first.strip_prefix("n,,").unwrap().to_string();
            let client_nonce = bare.split("r=").nth(1).unwrap().to_string();
            let server_nonce = format!("{client_nonce}scramtestsrv");
            let salt = b"scramtestsalt1234".to_vec();
            let iters = 4096;
            let server_first = format!("r={server_nonce},s={},i={iters}", b64().encode(&salt));
            Self {
                password: password.into(),
                salt,
                iters,
                client_first_bare: bare,
                server_nonce,
                server_first,
            }
        }

        pub(crate) fn server_first(&self) -> &str {
            &self.server_first
        }

        /// Checks the client-final's proof; `Ok(server-final)` if it verifies
        /// (so a wrong-password test gets `Err` and sends its own error).
        pub(crate) fn server_final(&self, client_final: &str) -> Result<String, ()> {
            let proof = b64()
                .decode(client_final.split(",p=").nth(1).unwrap())
                .unwrap();
            let mut salted = [0u8; 32];
            pbkdf2::derive(
                pbkdf2::PBKDF2_HMAC_SHA256,
                NonZeroU32::new(self.iters).unwrap(),
                &self.salt,
                self.password.as_bytes(),
                &mut salted,
            );
            let client_key = hmac256(&salted, b"Client Key");
            let stored_key = sha256(&client_key);
            let client_final_bare = format!("c=biws,r={}", self.server_nonce);
            let auth = format!(
                "{},{},{client_final_bare}",
                self.client_first_bare, self.server_first
            );
            let client_sig = hmac256(&stored_key, auth.as_bytes());
            let recovered: Vec<u8> = proof.iter().zip(&client_sig).map(|(a, b)| a ^ b).collect();
            if sha256(&recovered) != stored_key {
                return Err(());
            }
            let server_key = hmac256(&salted, b"Server Key");
            Ok(format!(
                "v={}",
                b64().encode(hmac256(&server_key, auth.as_bytes()))
            ))
        }
    }
}
