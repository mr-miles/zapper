//! TLS for inspected connections.
//!
//! Every session has its own CA, generated in memory when the session starts
//! and never written to disk; the guest trusts only that CA. Leaf
//! certificates are minted per host on first use.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context, Result};
use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, KeyPair, KeyUsagePurpose, SanType};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ClientConfig, RootCertStore, ServerConfig};

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

pub struct SessionCa {
    cert: rcgen::Certificate,
    key: KeyPair,
    pub pem: String,
    leaves: Mutex<HashMap<String, Arc<ServerConfig>>>,
}

impl SessionCa {
    pub fn new(session: &str) -> Result<Self> {
        let mut params = CertificateParams::default();
        params
            .distinguished_name
            .push(DnType::CommonName, format!("zapper session {session}"));
        params
            .distinguished_name
            .push(DnType::OrganizationName, "zapper");
        params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        let now = time::OffsetDateTime::now_utc();
        params.not_before = now - time::Duration::hours(1);
        params.not_after = now + time::Duration::days(30);
        let key = KeyPair::generate()?;
        let cert = params.self_signed(&key)?;
        let pem = cert.pem();
        Ok(Self {
            cert,
            key,
            pem,
            leaves: Mutex::new(HashMap::new()),
        })
    }

    /// Server config presenting a certificate for `host`.
    pub fn server_config(&self, host: &str) -> Result<Arc<ServerConfig>> {
        let host = host.to_ascii_lowercase();
        if let Some(c) = self.leaves.lock().unwrap().get(&host) {
            return Ok(c.clone());
        }
        let mut params = CertificateParams::default();
        params
            .distinguished_name
            .push(DnType::CommonName, host.clone());
        params.subject_alt_names = vec![match host.parse() {
            Ok(ip) => SanType::IpAddress(ip),
            Err(_) => SanType::DnsName(host.clone().try_into().context("host name")?),
        }];
        let now = time::OffsetDateTime::now_utc();
        params.not_before = now - time::Duration::hours(1);
        params.not_after = now + time::Duration::days(7);
        let key = KeyPair::generate()?;
        let cert = params.signed_by(&key, &self.cert, &self.key)?;
        let chain = vec![
            CertificateDer::from(cert.der().to_vec()),
            CertificateDer::from(self.cert.der().to_vec()),
        ];
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der()));
        let mut config = ServerConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_single_cert(chain, key)?;
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let config = Arc::new(config);
        self.leaves.lock().unwrap().insert(host, config.clone());
        Ok(config)
    }
}

/// Client config for upstream connections, verifying against the Mozilla
/// roots.
pub fn client_config() -> Arc<ClientConfig> {
    static C: OnceLock<Arc<ClientConfig>> = OnceLock::new();
    C.get_or_init(|| {
        let roots = RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        let mut config = ClientConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .expect("protocol versions")
            .with_root_certificates(roots)
            .with_no_client_auth();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Arc::new(config)
    })
    .clone()
}

/// Server name of a TLS ClientHello at the start of `buf`.
///
/// `Ok(None)`: not TLS, or TLS without SNI. `Err(())`: a TLS record that is
/// incomplete or malformed.
pub(crate) fn client_hello_sni(buf: &[u8]) -> Result<Option<String>, ()> {
    if buf.first() != Some(&0x16) {
        return Ok(None);
    }
    let rec_len = u16::from_be_bytes([*buf.get(3).ok_or(())?, *buf.get(4).ok_or(())?]) as usize;
    let rec = buf.get(5..5 + rec_len).ok_or(())?;
    let mut p = Parser(rec);
    if p.u8()? != 1 {
        return Err(());
    }
    p.take(3)?; // handshake length
    p.take(2 + 32)?; // version, random
    let n = p.u8()? as usize;
    p.take(n)?; // session id
    let n = p.u16()? as usize;
    p.take(n)?; // cipher suites
    let n = p.u8()? as usize;
    p.take(n)?; // compression
    if p.0.is_empty() {
        return Ok(None);
    }
    let n = p.u16()? as usize;
    let mut ext = Parser(p.take(n)?);
    while !ext.0.is_empty() {
        let kind = ext.u16()?;
        let n = ext.u16()? as usize;
        let mut data = Parser(ext.take(n)?);
        if kind == 0 {
            data.u16()?; // list length
            if data.u8()? != 0 {
                return Err(());
            }
            let n = data.u16()? as usize;
            let name = std::str::from_utf8(data.take(n)?).map_err(|_| ())?;
            return Ok(Some(name.to_ascii_lowercase()));
        }
    }
    Ok(None)
}

/// How many bytes the first TLS record needs, if `buf` starts one.
pub fn tls_record_len(buf: &[u8]) -> Option<usize> {
    if buf.first() == Some(&0x16) && buf.len() >= 5 {
        Some(5 + u16::from_be_bytes([buf[3], buf[4]]) as usize)
    } else {
        None
    }
}

struct Parser<'a>(&'a [u8]);

impl<'a> Parser<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], ()> {
        if self.0.len() < n {
            return Err(());
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }
    fn u8(&mut self) -> Result<u8, ()> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, ()> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mints_leaf() {
        let ca = SessionCa::new("t").unwrap();
        assert!(ca.pem.starts_with("-----BEGIN CERTIFICATE-----"));
        ca.server_config("api.example.com").unwrap();
        ca.server_config("10.0.0.1").unwrap();
    }

    #[test]
    fn parses_sni() {
        // Real ClientHello from rustls for "example.com".
        let config = client_config();
        let mut conn =
            rustls::ClientConnection::new(config, "Example.com".try_into().unwrap()).unwrap();
        let mut hello = Vec::new();
        conn.write_tls(&mut hello).unwrap();
        assert_eq!(tls_record_len(&hello), Some(hello.len()));
        assert_eq!(client_hello_sni(&hello), Ok(Some("example.com".into())));
        assert_eq!(client_hello_sni(b"SSH-2.0-OpenSSH"), Ok(None));
        assert_eq!(client_hello_sni(&hello[..20]), Err(()));
    }
}
