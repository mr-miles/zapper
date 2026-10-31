//! TLS for inspected connections.
//!
//! Every session has its own CA, made when the session is; the guest trusts
//! only that CA. A plain session's lives in memory alone; a persistent
//! session's key is kept in its `ca.json` (0600), so the guest goes on
//! trusting it after a restart (`SessionCa::kept`). Leaf certificates are
//! minted per host on first use.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context, Result};
use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, KeyPair, KeyUsagePurpose, SanType};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ClientConfig, RootCertStore, ServerConfig};

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

struct SessionCa {
    cert: rcgen::Certificate,
    key: KeyPair,
    pub pem: String,
}

/// A leaf certificate minted by a [`SessionCa`]: the chain (leaf then CA) and
/// the leaf's private key, ready for a rustls server config.
struct Leaf {
    pub chain: Vec<CertificateDer<'static>>,
    pub key: PrivateKeyDer<'static>,
}

impl SessionCa {
    pub fn new(session: &str) -> Result<Self> {
        let now = time::OffsetDateTime::now_utc();
        Self::with_key(session, KeyPair::generate()?, now, time::Duration::days(30))
    }

    /// The CA of a persistent session, kept in `path` (its key and when it
    /// was made, 0600), made there the first time: the same CA after the
    /// daemon restarts, so the guest, which trusts it by its name and key,
    /// goes on trusting what it signs. Valid for ten years, as the session
    /// may be started again for long.
    pub fn kept(session: &str, path: &std::path::Path) -> Result<Self> {
        #[derive(serde::Serialize, serde::Deserialize)]
        struct Kept {
            key: String,
            made: i64,
        }
        let life = time::Duration::days(3650);
        // Made afresh only when there is none: one that can't be read is an
        // error, not replaced, as the guest trusts that one.
        match std::fs::read(path) {
            Ok(b) => {
                let k: Kept = serde_json::from_slice(&b).context("the session's CA")?;
                let made = time::OffsetDateTime::from_unix_timestamp(k.made)?;
                return Self::with_key(session, KeyPair::from_pem(&k.key)?, made, life);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(anyhow::Error::new(e).context(format!("reading {}", path.display())))
            }
        }
        let key = KeyPair::generate()?;
        let made = time::OffsetDateTime::now_utc();
        let kept = Kept {
            key: key.serialize_pem(),
            made: made.unix_timestamp(),
        };
        crate::util::write_private(path, &serde_json::to_vec(&kept)?)?;
        Self::with_key(session, key, made, life)
    }

    fn with_key(
        session: &str,
        key: KeyPair,
        made: time::OffsetDateTime,
        life: time::Duration,
    ) -> Result<Self> {
        let mut params = CertificateParams::default();
        params
            .distinguished_name
            .push(DnType::CommonName, format!("zapper session {session}"));
        params
            .distinguished_name
            .push(DnType::OrganizationName, "zapper");
        params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        params.not_before = made - time::Duration::hours(1);
        params.not_after = made + life;
        let cert = params.self_signed(&key)?;
        let pem = cert.pem();
        Ok(Self { cert, key, pem })
    }

    /// Mints a leaf certificate for `host` (given lowercased), signed by this
    /// CA. Assembling it into a server config is the caller's concern
    /// ([`server_config`]).
    pub fn leaf(&self, host: &str) -> Result<Leaf> {
        let mut params = CertificateParams::default();
        params
            .distinguished_name
            .push(DnType::CommonName, host.to_string());
        params.subject_alt_names = vec![match host.parse() {
            Ok(ip) => SanType::IpAddress(ip),
            Err(_) => SanType::DnsName(host.to_string().try_into().context("host name")?),
        }];
        let now = time::OffsetDateTime::now_utc();
        params.not_before = now - time::Duration::hours(1);
        params.not_after = now + time::Duration::days(7);
        let key = KeyPair::generate()?;
        let cert = params.signed_by(&key, &self.cert, &self.key)?;
        Ok(Leaf {
            chain: vec![
                CertificateDer::from(cert.der().to_vec()),
                CertificateDer::from(self.cert.der().to_vec()),
            ],
            key: PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
        })
    }
}

/// A session's TLS: its own CA, and the per-host server configs minted from
/// it for inspected connections, cached on first use. The single owner of a
/// session's guest-facing TLS state.
pub(crate) struct SessionTls {
    ca: SessionCa,
    /// Each host's config, and when its leaf was minted.
    configs: Mutex<HashMap<String, (Arc<ServerConfig>, std::time::Instant)>>,
}

/// What a terminated connection offers the guest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Alpn {
    /// HTTP/2 or HTTP/1.1, as the session's config offers.
    Any,
    /// HTTP/1.1 alone (a service's plain side speaks only that).
    Http1,
}

/// How long a minted leaf (valid for a week) is used before another is.
const LEAF_USE: std::time::Duration = std::time::Duration::from_secs(6 * 24 * 3600);
/// Most hosts' configs kept at once: the names are the guest's, so a rule
/// allowing a whole domain mustn't let it grow without end.
const MAX_LEAVES: usize = 1024;

impl SessionTls {
    pub fn new(session: &str) -> Result<Self> {
        Ok(Self {
            ca: SessionCa::new(session)?,
            configs: Mutex::new(HashMap::new()),
        })
    }

    /// A persistent session's, with its CA kept in `path` ([`SessionCa::kept`]).
    pub fn kept(session: &str, path: &std::path::Path) -> Result<Self> {
        Ok(Self {
            ca: SessionCa::kept(session, path)?,
            configs: Mutex::new(HashMap::new()),
        })
    }

    /// The CA certificate the guest is given to trust (PEM).
    pub fn pem(&self) -> &str {
        &self.ca.pem
    }

    /// The CA's public key, which the guest's trust rests on.
    #[cfg(test)]
    pub fn public_key(&self) -> Vec<u8> {
        self.ca.key.public_key_der()
    }

    /// Terminates the guest's TLS for `host`: the session CA's leaf for it,
    /// offering `alpn`; the handshake within 15 seconds; and a server name,
    /// if the guest sends one, that is `host`, so a connection decided for
    /// one name can't be used for another. The one place the guest's TLS is
    /// ended (an inspected connection, a service behind the daemon's TLS, a
    /// database relay's in-band TLS).
    pub async fn accept<IO>(
        &self,
        io: IO,
        host: &str,
        alpn: Alpn,
    ) -> std::result::Result<tokio_rustls::server::TlsStream<IO>, String>
    where
        IO: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        let mut config = self.server_config(host).map_err(|e| format!("{e:#}"))?;
        if let Alpn::Http1 = alpn {
            let mut c = (*config).clone();
            c.alpn_protocols = vec![b"http/1.1".to_vec()];
            config = Arc::new(c);
        }
        let accept = tokio_rustls::TlsAcceptor::from(config).accept(io);
        let tls = tokio::time::timeout(std::time::Duration::from_secs(15), accept)
            .await
            .map_err(|_| "TLS from guest: no handshake within 15s".to_string())?
            .map_err(|e| format!("TLS from guest: {e}"))?;
        if let Some(sni) = tls.get_ref().1.server_name() {
            if !sni.eq_ignore_ascii_case(host) {
                return Err(format!("TLS server name {sni} does not match {host}"));
            }
        }
        Ok(tls)
    }

    /// The server config for terminating a connection to `host`: a leaf
    /// minted from the CA and assembled on first use, then cached, and
    /// minted again before it expires (a session can run for weeks).
    pub fn server_config(&self, host: &str) -> Result<Arc<ServerConfig>> {
        let host = host.to_ascii_lowercase();
        if let Some((c, at)) = self.configs.lock().unwrap().get(&host) {
            if at.elapsed() < LEAF_USE {
                return Ok(c.clone());
            }
        }
        let config = server_config(self.ca.leaf(&host)?)?;
        let mut configs = self.configs.lock().unwrap();
        if configs.len() >= MAX_LEAVES {
            configs.retain(|_, (_, at)| at.elapsed() < LEAF_USE);
            if configs.len() >= MAX_LEAVES {
                configs.clear();
            }
        }
        configs.insert(host, (config.clone(), std::time::Instant::now()));
        Ok(config)
    }

    #[cfg(test)]
    pub fn cached(&self) -> usize {
        self.configs.lock().unwrap().len()
    }
}

/// Server config for an inspected connection, presenting `leaf`. The mirror
/// of [`client_config`] for the guest-facing side.
fn server_config(leaf: Leaf) -> Result<Arc<ServerConfig>> {
    let mut config = ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(leaf.chain, leaf.key)?;
    // Offer HTTP/2 and HTTP/1.1; the guest's client picks. h2 lets us inspect
    // gRPC and other HTTP/2 traffic rather than forcing a downgrade.
    config.alpn_protocols = h2_then_http11();
    Ok(Arc::new(config))
}

/// TLS to an upstream `host` over `stream`, its certificate checked against
/// the Mozilla roots and its name: the proxy's side of an inspected
/// connection, and a database relay's server leg.
pub(super) async fn connect<S>(
    stream: S,
    host: &str,
) -> std::result::Result<tokio_rustls::client::TlsStream<S>, String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let name = rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|e| format!("{host}: {e}"))?;
    tokio_rustls::TlsConnector::from(client_config())
        .connect(name, stream)
        .await
        .map_err(|e| format!("TLS to {host}: {e}"))
}

/// The ALPN protocols an inspected connection offers, on either side: HTTP/2
/// first, then HTTP/1.1.
fn h2_then_http11() -> Vec<Vec<u8>> {
    vec![b"h2".to_vec(), b"http/1.1".to_vec()]
}

/// Whether a TLS connection negotiated HTTP/2 by ALPN.
pub(super) fn is_h2(conn: &rustls::CommonState) -> bool {
    conn.alpn_protocol() == Some(b"h2")
}

/// Whether `buf` starts a TLS handshake record (`0x16`): a ClientHello, as
/// the first byte a TLS client sends.
pub(super) fn starts_handshake(buf: &[u8]) -> bool {
    buf.first() == Some(&0x16)
}

/// Client config for upstream connections, verifying against the Mozilla
/// roots.
pub(super) fn client_config() -> Arc<ClientConfig> {
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
        // Offer HTTP/2 and HTTP/1.1 upstream; we speak to the real server in
        // whatever it negotiates, independently of the guest-facing protocol.
        config.alpn_protocols = h2_then_http11();
        Arc::new(config)
    })
    .clone()
}

/// Server name of a TLS ClientHello at the start of `buf`.
///
/// `Ok(None)`: not TLS, or TLS without SNI. `Err(())`: a TLS record that is
/// incomplete or malformed.
pub(crate) fn client_hello_sni(buf: &[u8]) -> Result<Option<String>, ()> {
    if !starts_handshake(buf) {
        return Ok(None);
    }
    let rec = buf.get(5..tls_record_len(buf).ok_or(())?).ok_or(())?;
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
    if starts_handshake(buf) && buf.len() >= 5 {
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
    /// A kept CA that can't be read is an error, and stays as it was.
    #[test]
    fn an_unreadable_kept_ca_isnt_replaced() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("zapper-ca-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ca.json");
        let first = SessionCa::kept("abc234", &path).unwrap();
        let kept = std::fs::read(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read(&path).is_ok() {
            return; // as root: nothing is unreadable
        }
        assert!(SessionCa::kept("abc234", &path).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), kept, "left as it was");
        let again = SessionCa::kept("abc234", &path).unwrap();
        assert_eq!(again.key.public_key_der(), first.key.public_key_der());
        let _ = std::fs::remove_dir_all(&dir);
    }

    use super::*;

    #[test]
    fn mints_leaf() {
        let ca = SessionCa::new("t").unwrap();
        assert!(ca.pem.starts_with("-----BEGIN CERTIFICATE-----"));
        server_config(ca.leaf("api.example.com").unwrap()).unwrap();
        server_config(ca.leaf("10.0.0.1").unwrap()).unwrap();
    }

    #[test]
    fn session_tls_caches_a_config_per_host_case_insensitively() {
        let tls = SessionTls::new("t").unwrap();
        assert!(tls.pem().starts_with("-----BEGIN CERTIFICATE-----"));
    }

    /// Leaves for the guest's names are kept within a bound.
    #[test]
    fn the_leaf_cache_is_bounded() {
        let tls = SessionTls::new("abc234").unwrap();
        for i in 0..MAX_LEAVES + 10 {
            tls.server_config(&format!("h{i}.example")).unwrap();
        }
        assert!(tls.cached() <= MAX_LEAVES, "{}", tls.cached());
        let a = tls.server_config("API.example.com").unwrap();
        let b = tls.server_config("api.example.com").unwrap();
        assert!(
            Arc::ptr_eq(&a, &b),
            "the same host, any case, is one cached config"
        );
        let c = tls.server_config("other.example").unwrap();
        assert!(
            !Arc::ptr_eq(&a, &c),
            "a different host is a different config"
        );
    }

    /// Both the guest-facing and upstream configs offer HTTP/2 and HTTP/1.1,
    /// so inspected connections can speak either, h2 preferred.
    #[test]
    fn alpn_offers_h2_then_http11() {
        let ca = SessionCa::new("t").unwrap();
        let sc = server_config(ca.leaf("api.example").unwrap()).unwrap();
        assert_eq!(sc.alpn_protocols, [b"h2".to_vec(), b"http/1.1".to_vec()]);
        assert_eq!(
            client_config().alpn_protocols,
            [b"h2".to_vec(), b"http/1.1".to_vec()]
        );
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

    /// A client that takes any certificate, so what is tested is the
    /// server's side alone.
    #[derive(Debug)]
    struct AnyCert;

    impl rustls::client::danger::ServerCertVerifier for AnyCert {
        fn verify_server_cert(
            &self,
            _: &CertificateDer<'_>,
            _: &[CertificateDer<'_>],
            _: &rustls::pki_types::ServerName<'_>,
            _: &[u8],
            _: rustls::pki_types::UnixTime,
        ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        }
        fn verify_tls12_signature(
            &self,
            _: &[u8],
            _: &CertificateDer<'_>,
            _: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }
        fn verify_tls13_signature(
            &self,
            _: &[u8],
            _: &CertificateDer<'_>,
            _: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }
        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            rustls::crypto::ring::default_provider()
                .signature_verification_algorithms
                .supported_schemes()
        }
    }

    /// The guest's TLS is ended for the host the connection is for: a
    /// server name naming another is refused, as is a handshake that never
    /// comes; the host's own (or none) is taken.
    #[tokio::test(start_paused = true)]
    async fn the_guests_tls_is_ended_only_for_its_host() {
        let tls = SessionTls::new("abc234").unwrap();
        let config = Arc::new(
            ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .unwrap()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(AnyCert))
                .with_no_client_auth(),
        );
        let handshake = |sni: &'static str| {
            let config = config.clone();
            async move {
                let (guest, ours) = tokio::io::duplex(64 * 1024);
                let client = tokio::spawn(async move {
                    let name = rustls::pki_types::ServerName::try_from(sni).unwrap();
                    tokio_rustls::TlsConnector::from(config)
                        .connect(name, guest)
                        .await
                });
                (ours, client)
            }
        };
        let (ours, _client) = handshake("other.example").await;
        let e = tls
            .accept(ours, "api.example", Alpn::Any)
            .await
            .err()
            .unwrap();
        assert!(
            e.contains("other.example does not match api.example"),
            "{e}"
        );

        let (ours, _client) = handshake("API.example").await;
        assert!(tls.accept(ours, "api.example", Alpn::Http1).await.is_ok());

        let (_guest, ours) = tokio::io::duplex(1024);
        let e = tls
            .accept(ours, "api.example", Alpn::Any)
            .await
            .err()
            .unwrap();
        assert!(e.contains("no handshake"), "{e}");
    }
}
