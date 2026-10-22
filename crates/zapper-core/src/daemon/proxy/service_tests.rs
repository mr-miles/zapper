//! Connections to a firm's names through the proxy, as a guest's NIC
//! connections arrive (a synthetic address the session's DNS gave out):
//! the git server over TLS, the cluster's API over vsock, names nothing
//! serves, and sessions of no firm.

use std::sync::Arc;

use base64::Engine as _;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

use crate::testing::TestCore;
use crate::Session;

/// A connection from `s` to `name:port`, through the proxy.
pub(crate) fn dial(t: &TestCore, s: &Arc<Session>, name: &str, port: u16) -> UnixStream {
    let ip = {
        let mut dns = s.dns.lock().unwrap();
        dns.remember(name, true);
        dns.address(name).unwrap()
    };
    let (guest, host) = UnixStream::pair().unwrap();
    tokio::spawn(super::transparent(
        t.core.daemon().clone(),
        s.clone(),
        host,
        ip,
        port,
    ));
    guest
}

/// The session's net events, newest last.
pub(crate) fn net_events(t: &TestCore, s: &Session) -> Vec<Value> {
    t.core
        .recent_events(Some(&s.id))
        .iter()
        .map(|e| serde_json::to_value(e).unwrap())
        .filter(|e| e["type"] == "net")
        .collect()
}

/// Waits for the session's `n`th net event.
pub(crate) async fn net_event(t: &TestCore, s: &Session, n: usize) -> Value {
    for _ in 0..200 {
        if let Some(e) = net_events(t, s).get(n) {
            return e.clone();
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("no net event {n}: {:?}", net_events(t, s));
}

/// A TLS client that trusts the session's CA, as the guest does.
pub(crate) fn guest_tls(s: &Session) -> tokio_rustls::TlsConnector {
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
pub(crate) fn echoing_vm(s: &Session) {
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

#[cfg(feature = "office")]
mod firm {
    use super::*;
    use crate::daemon::office::{facility, labels, test_run, FirmRun};
    use crate::NewSession;

    const FIRM: &str = "
profiles:
  w: { agent: { kind: claude-code } }
firms:
  acme:
    roles:
      ceo: { profile: w, skills: { work: ceo } }
      eng: { profile: w, skills: { work: software-engineer } }
    workers:
      - { name: ceo, role: ceo }
      - { name: pat, role: eng, reports_to: ceo }
    git: { repos: [api] }
";

    fn worker(run: &FirmRun, name: &str) -> Arc<Session> {
        run.rt(name)
            .unwrap()
            .session
            .lock()
            .unwrap()
            .clone()
            .unwrap()
    }

    fn allow(t: &TestCore, s: &Arc<Session>, host: &str) {
        let rule = serde_yaml::from_str(&format!("{{ kind: net, action: allow, host: '{host}' }}"))
            .unwrap();
        t.core.add_session_rule(s, rule, None);
    }

    #[tokio::test]
    async fn the_git_server_answers_over_tls_with_the_sessions_ca() {
        let t = TestCore::new(FIRM);
        let run = test_run(&t.core, "acme");
        // The CEO holds every repository: their git rule allows the name.
        let ceo = worker(&run, "ceo");
        let host = format!("git.{}", run.domain);
        let io = dial(&t, &ceo, &host, 443);
        let name = rustls::pki_types::ServerName::try_from(host.clone()).unwrap();
        let mut tls = guest_tls(&ceo).connect(name, io).await.unwrap();
        tls.write_all(
            format!("GET /api.git/info/refs HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .unwrap();
        let mut reply = String::new();
        let _ = tls.read_to_string(&mut reply).await;
        assert!(reply.starts_with("HTTP/1.1 401"), "{reply}");
        assert!(reply.contains("credentials required"), "{reply}");
    }

    #[tokio::test]
    async fn the_clusters_api_is_its_vms_port() {
        let t = TestCore::new(FIRM);
        let run = test_run(&t.core, "acme");
        let mut new = NewSession::plain("w".into(), t.workdir(), Default::default());
        new.labels = labels(&run.firm, &run.id, facility::CLUSTER, false);
        let cluster = t.core.test_session(new, true).unwrap();
        echoing_vm(&cluster);
        facility::test_cluster(&t.core, &run, &cluster);

        let pat = worker(&run, "pat");
        let host = format!("cluster.{}", run.domain);
        allow(&t, &pat, &host);
        let mut io = dial(&t, &pat, &host, 6443);
        io.write_all(b"ping").await.unwrap();
        let mut buf = [0u8; 4];
        io.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ping");
        let e = net_event(&t, &pat, 0).await;
        assert_eq!(e["action"], "allow", "{e}");
    }

    #[tokio::test]
    async fn a_name_the_firm_doesnt_serve_goes_nowhere() {
        let t = TestCore::new(FIRM);
        let run = test_run(&t.core, "acme");
        let pat = worker(&run, "pat");
        let d = run.domain.clone();
        allow(&t, &pat, &format!("*.{d}"));
        // One net event each, refused after the rules allowed it.
        for (i, (name, port)) in [(format!("x.{d}"), 443), (format!("git.{d}"), 80)]
            .into_iter()
            .enumerate()
        {
            let _io = dial(&t, &pat, &name, port);
            let e = net_event(&t, &pat, i).await;
            let error = e["error"].as_str().unwrap_or_default();
            assert!(
                error.contains(&format!("nothing serves {name}:{port}")),
                "{e}"
            );
        }

        // A session of no firm: the name is the host's to resolve.
        let plain = t
            .core
            .test_session(
                NewSession::plain("w".into(), t.workdir(), Default::default()),
                true,
            )
            .unwrap();
        allow(&t, &plain, &format!("*.{d}"));
        let _io = dial(&t, &plain, &format!("git.{d}"), 443);
        let e = net_event(&t, &plain, 0).await;
        let error = e["error"].as_str().unwrap_or_default();
        assert!(error.starts_with("resolving git."), "{e}");
    }
}
