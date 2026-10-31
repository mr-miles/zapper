//! A `Core` on a directory of its own, for tests: placeholder agent and
//! Firecracker files, no keeper, no VM. Sessions come from
//! `Core::test_session`. Below it, ways in for tests of what sits on the
//! core: a session's connections through the proxy, its DNS queries, where
//! a name goes for it, and its MCP connections; a VM's side of vsock and
//! a guest's TLS.

use crate::prelude::*;
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
        let mut dns = s.dns.names();
        dns.remember(name, true);
        std::net::IpAddr::V4(dns.address(name).unwrap())
    };
    dial_ip(t, s, ip, port)
}

/// Like `dial`, but the guest reached the name's synthetic IPv6 address.
pub fn dial6(t: &TestCore, s: &Arc<Session>, name: &str, port: u16) -> UnixStream {
    let ip = {
        let mut dns = s.dns.names();
        dns.remember(name, true);
        std::net::IpAddr::V6(dns.address6(name).unwrap())
    };
    dial_ip(t, s, ip, port)
}

/// Like `dial`, to an address the guest reached without a name: decided on
/// the address itself.
pub fn dial_addr(t: &TestCore, s: &Arc<Session>, ip: std::net::IpAddr, port: u16) -> UnixStream {
    dial_ip(t, s, ip, port)
}

fn dial_ip(t: &TestCore, s: &Arc<Session>, ip: std::net::IpAddr, port: u16) -> UnixStream {
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
    let b64: String = s
        .tls
        .pem()
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

/// A command the host started in a VM (`shell_vm`): its request, its stdin
/// as lines, and its stdout.
pub struct Shell {
    pub request: zapper_policy::proto::ShellRequest,
    read: tokio::net::unix::OwnedReadHalf,
    write: tokio::net::unix::OwnedWriteHalf,
    buf: Vec<u8>,
}

impl Shell {
    /// The next line of its stdin, without the newline; none once the host
    /// closed it.
    pub async fn line(&mut self) -> Option<String> {
        use zapper_policy::proto::Frame;
        loop {
            if let Some(i) = self.buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = self.buf.drain(..=i).collect();
                return Some(String::from_utf8_lossy(&line[..i]).into_owned());
            }
            match crate::util::read_frame(&mut self.read).await {
                Ok(Some(Frame::Data(d))) => self.buf.extend(d),
                Ok(Some(Frame::Resize(..))) => {}
                _ => return None,
            }
        }
    }

    /// Writes `line` and a newline to its stdout.
    pub async fn say(&mut self, line: &str) {
        self.frame(zapper_policy::proto::Frame::Data(
            format!("{line}\n").into_bytes(),
        ))
        .await;
    }

    /// All of its stdin, up to the host's end of it.
    pub async fn input(&mut self) -> Vec<u8> {
        use zapper_policy::proto::Frame;
        let mut all = std::mem::take(&mut self.buf);
        while let Ok(Some(Frame::Data(d))) = crate::util::read_frame(&mut self.read).await {
            all.extend(d);
        }
        all
    }

    /// Writes `bytes` to its stdout as they are.
    pub async fn output(&mut self, bytes: &[u8]) {
        for chunk in bytes.chunks(60 * 1024) {
            self.frame(zapper_policy::proto::Frame::Data(chunk.to_vec()))
                .await;
        }
    }

    /// The command exits with `code`.
    pub async fn exit(mut self, code: i32) {
        self.frame(zapper_policy::proto::Frame::Exit(code)).await;
    }

    async fn frame(&mut self, f: zapper_policy::proto::Frame) {
        let mut b = Vec::new();
        f.write_to(&mut b).unwrap();
        let _ = self.write.write_all(&b).await;
    }
}

/// Plays a VM's side of the commands the host starts in `s`
/// (`Session::open_shell`: an agent the foreman drives, `zapper exec`):
/// each, as it is started.
pub fn shell_vm(s: &Session) -> tokio::sync::mpsc::UnboundedReceiver<Shell> {
    let path = s.vsock_path();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let listener = UnixListener::bind(&path).unwrap();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok((mut c, _)) = listener.accept().await {
            let mut line = Vec::new();
            let mut b = [0u8; 1];
            while c.read_exact(&mut b).await.is_ok() && b[0] != b'\n' {
                line.push(b[0]);
            }
            assert!(line.starts_with(b"CONNECT "), "{line:?}");
            c.write_all(b"OK 1\n").await.unwrap();
            line.clear();
            while c.read_exact(&mut b).await.is_ok() && b[0] != b'\n' {
                line.push(b[0]);
            }
            let request = serde_json::from_slice(&line).unwrap();
            let (read, write) = c.into_split();
            let shell = Shell {
                request,
                read,
                write,
                buf: Vec::new(),
            };
            if tx.send(shell).is_err() {
                break;
            }
        }
    });
    rx
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
