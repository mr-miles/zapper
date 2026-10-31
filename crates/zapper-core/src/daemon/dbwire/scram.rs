//! SCRAM client exchange (RFC 5802 / RFC 7677), as the proxy authenticates to
//! the real server with the host-held password. Supports SCRAM-SHA-256 and
//! SCRAM-SHA-512. Pure — no I/O and the nonce is supplied, so it is unit-tested
//! against the RFC 7677 vector; the relay drives it over the server leg and
//! generates a random nonce.

use base64::Engine;
use ring::{digest, hmac, pbkdf2};
use std::num::NonZeroU32;

/// `base64("n,,")` — the GS2 channel-binding header for "no binding", which
/// is what `c=` carries in the client-final message.
const GS2_HEADER_B64: &str = "biws";

/// The hash a SCRAM mechanism is built on.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Hash {
    Sha256,
    Sha512,
}

impl Hash {
    fn digest(self, data: &[u8]) -> Vec<u8> {
        let alg = match self {
            Hash::Sha256 => &digest::SHA256,
            Hash::Sha512 => &digest::SHA512,
        };
        digest::digest(alg, data).as_ref().to_vec()
    }
    fn hmac(self, key: &[u8], msg: &[u8]) -> Vec<u8> {
        let alg = match self {
            Hash::Sha256 => hmac::HMAC_SHA256,
            Hash::Sha512 => hmac::HMAC_SHA512,
        };
        hmac::sign(&hmac::Key::new(alg, key), msg).as_ref().to_vec()
    }
    fn pbkdf2(self, iters: NonZeroU32, salt: &[u8], password: &[u8]) -> Vec<u8> {
        let (alg, len) = match self {
            Hash::Sha256 => (pbkdf2::PBKDF2_HMAC_SHA256, 32),
            Hash::Sha512 => (pbkdf2::PBKDF2_HMAC_SHA512, 64),
        };
        let mut out = vec![0u8; len];
        pbkdf2::derive(alg, iters, salt, password, &mut out);
        out
    }
}

fn b64() -> base64::engine::general_purpose::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

/// A SCRAM exchange in progress: built with the client-first message, then
/// advanced with the server-first message to produce the client-final
/// message and the server signature to verify.
pub struct Client {
    hash: Hash,
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
        Self::with_hash(Hash::Sha256, user, password, nonce)
    }

    /// As [`new`](Self::new) but choosing the hash (SCRAM-SHA-256 or
    /// SCRAM-SHA-512).
    pub fn with_hash(hash: Hash, user: &str, password: &str, nonce: &str) -> Self {
        Self {
            hash,
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

        let salted = self
            .hash
            .pbkdf2(iterations, &salt, self.password.as_bytes());
        let client_key = self.hash.hmac(&salted, b"Client Key");
        let stored_key = self.hash.digest(&client_key);
        let server_key = self.hash.hmac(&salted, b"Server Key");

        let client_final_bare = format!("c={GS2_HEADER_B64},r={nonce}");
        let auth_message = format!("{},{sf},{client_final_bare}", self.client_first_bare);
        let client_sig = self.hash.hmac(&stored_key, auth_message.as_bytes());
        let proof: Vec<u8> = client_key
            .iter()
            .zip(client_sig.iter())
            .map(|(a, b)| a ^ b)
            .collect();
        let server_sig = self.hash.hmac(&server_key, auth_message.as_bytes());

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
