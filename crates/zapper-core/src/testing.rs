//! A `Core` on a directory of its own, for tests: placeholder agent and
//! Firecracker files, no keeper, no VM. Sessions come from
//! `Core::test_session`. Below it, ways in for tests of what sits on the
//! core: a session's connections through the proxy, its DNS queries, where
//! a name goes for it, and its MCP connections; a VM's side of vsock and
//! a guest's TLS.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use base64::Engine as _;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

use crate::{Core, ServiceTarget, Session};

pub struct TestCore {
    pub core: Core,
    pub dir: PathBuf,
}

impl TestCore {
    /// A core whose config is `yaml` (profiles, policy, firms...).
    pub fn new(yaml: &str) -> TestCore {
        TestCore::with_daemon("", yaml)
    }

    /// As `new`, with `daemon` added to the config's `daemon` section
    /// (one `key: value` per line, e.g. `driver: false`).
    pub fn with_daemon(daemon: &str, yaml: &str) -> TestCore {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "zapper-core-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for f in ["agent", "firecracker"] {
            std::fs::write(dir.join(f), f).unwrap();
        }
        let mut extra = String::new();
        for l in daemon.lines().filter(|l| !l.trim().is_empty()) {
            extra.push_str(&format!("  {}\n", l.trim()));
        }
        let d = dir.display();
        let config = format!(
            "daemon:\n  state: {d}/state\n  agent: {d}/agent\n  firecracker: {d}/firecracker\n  images: {d}/images\n  kernels: {d}/kernels\n  ui: \"\"\n{extra}{yaml}"
        );
        let path = dir.join("zapper.yaml");
        std::fs::write(&path, config).unwrap();
        let core = Core::start(path).unwrap();
        TestCore { core, dir }
    }

    /// The startup file.
    pub fn startup(&self) -> PathBuf {
        self.dir.join("zapper.yaml")
    }

    /// A working directory for sessions.
    pub fn workdir(&self) -> PathBuf {
        self.dir.clone()
    }
}

impl Drop for TestCore {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A connection from `s` to `name:port`, through the proxy.
pub fn dial(t: &TestCore, s: &Arc<Session>, name: &str, port: u16) -> UnixStream {
    let ip = {
        let mut dns = s.dns.lock().unwrap();
        dns.remember(name, true);
        dns.address(name).unwrap()
    };
    let (guest, host) = UnixStream::pair().unwrap();
    tokio::spawn(crate::daemon::proxy::transparent(
        t.core.daemon().clone(),
        s.clone(),
        host,
        ip,
        port,
    ));
    guest
}

/// The session's net events, newest last.
pub fn net_events(t: &TestCore, s: &Session) -> Vec<Value> {
    t.core
        .recent_events(Some(&s.id))
        .iter()
        .map(|e| serde_json::to_value(e).unwrap())
        .filter(|e| e["type"] == "net")
        .collect()
}

/// Waits for the session's `n`th net event.
pub async fn net_event(t: &TestCore, s: &Session, n: usize) -> Value {
    for _ in 0..200 {
        if let Some(e) = net_events(t, s).get(n) {
            return e.clone();
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("no net event {n}: {:?}", net_events(t, s));
}

/// A TLS client that trusts the session's CA, as the guest does.
pub fn guest_tls(s: &Session) -> tokio_rustls::TlsConnector {
    let b64: String =
        s.ca.pem
            .lines()
            .filter(|l| !l.starts_with("-----"))
            .collect();
    let der = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .unwrap();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(der.into()).unwrap();
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    tokio_rustls::TlsConnector::from(Arc::new(config))
}

/// Plays a VM's side of host-initiated vsock connections on `s`'s socket:
/// each is accepted and echoes what it is sent.
pub fn echoing_vm(s: &Session) {
    let path = s.vsock_path();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let listener = UnixListener::bind(&path).unwrap();
    tokio::spawn(async move {
        while let Ok((mut c, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut line = Vec::new();
                let mut b = [0u8; 1];
                while c.read_exact(&mut b).await.is_ok() && b[0] != b'\n' {
                    line.push(b[0]);
                }
                assert!(line.starts_with(b"CONNECT "), "{line:?}");
                c.write_all(b"OK 1\n").await.unwrap();
                let mut buf = [0u8; 256];
                while let Ok(n) = c.read(&mut buf).await {
                    if n == 0 || c.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
}

/// The DNS server's answer to `query` from `s`.
pub async fn query_dns(core: &Core, s: &Arc<Session>, query: &[u8]) -> Option<Vec<u8>> {
    crate::daemon::nic::query_dns(core.daemon(), s, query).await
}

/// Where `host:port` goes for `s`: a service's target, `Some(None)` for a
/// name served but not on that port, `None` for the host to resolve.
pub fn route(core: &Core, s: &Session, host: &str, port: u16) -> Option<Option<ServiceTarget>> {
    use crate::daemon::services::Route;
    match core.daemon().services.route(s, host, port)? {
        Route::Service(target) => Some(Some(target)),
        Route::Unserved => Some(None),
    }
}

/// Whether `name` exists for `s` without asking the host: `None` when the
/// host should say.
pub fn resolves(core: &Core, s: &Session, name: &str) -> Option<bool> {
    core.daemon().services.resolves(s, name)
}

/// Serves one MCP connection from `s`'s VM through the gateway.
pub async fn mcp<S>(core: &Core, s: &Arc<Session>, stream: S) -> anyhow::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    crate::daemon::gateway::connection(core.daemon(), s, stream).await
}
