//! HTTP/2 on the inspected (MITM) path: the guest negotiates h2 by ALPN, the
//! proxy terminates it with the session CA, decides and injects per request,
//! and answers over h2. The real upstream half (an h2 server) is exercised by
//! the e2e scenario; here the host is a service, so `open_tls` refuses it with
//! a 502 — but only *after* the request has been served, decided and injected
//! over h2, which is exactly the new guest-facing path under test.

use crate::prelude::*;
use std::sync::Arc;

use base64::Engine as _;
use bytes::Bytes;
use http_body_util::Empty;
use hyper::{Method, Request, StatusCode, Version};
use hyper_util::rt::{TokioExecutor, TokioIo};

use crate::testing::{dial, net_event, TestCore};
use crate::{NewSession, Session};

/// A client that trusts the session CA and offers only h2 over ALPN.
fn guest_h2(s: &Session) -> tokio_rustls::TlsConnector {
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
    let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    config.alpn_protocols = vec![b"h2".to_vec()];
    tokio_rustls::TlsConnector::from(Arc::new(config))
}

/// Registered only so `target()` resolves and the MITM runs; `open_tls`
/// refuses a service target before this is ever reached.
struct NeverServed;
impl crate::ServiceHandler for NeverServed {
    fn serve(&self, _s: Arc<Session>, _io: Box<dyn crate::ServiceIo>) {}
}

#[tokio::test]
async fn http2_is_served_decided_and_injected() {
    let t = TestCore::new(
        "secrets:\n  tok: { value: s3cret, hosts: [api.example] }\nprofiles:\n  web:\n    policy:\n      net:\n        - { action: allow, host: api.example, methods: [GET], secrets: [tok] }\n",
    );
    let s = t
        .core
        .test_session(
            NewSession::plain("web".into(), t.workdir(), Default::default()),
            true,
        )
        .unwrap();
    // A rule with `methods`/`secrets` makes the host inspected; registering it
    // as a service makes `target()` resolve so the inspecting MITM runs.
    t.core
        .add_service(crate::ServiceSpec {
            name: "api.example".into(),
            port: 443,
            sessions: Default::default(),
            target: crate::ServiceTarget::Handler {
                tls: true,
                handler: Arc::new(NeverServed),
            },
            owner: "test".into(),
        })
        .unwrap();

    let io = dial(&t, &s, "api.example", 443);
    let name = rustls::pki_types::ServerName::try_from("api.example").unwrap();
    let tls = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        guest_h2(&s).connect(name, io),
    )
    .await
    .expect("TLS handshake timed out")
    .expect("guest TLS + ALPN h2");
    // The proxy negotiated h2 with the guest.
    assert_eq!(
        tls.get_ref().1.alpn_protocol(),
        Some(&b"h2"[..]),
        "server must negotiate h2"
    );

    let (mut sender, conn) =
        hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(tls))
            .await
            .unwrap();
    tokio::spawn(conn);

    let placeholder = zapper_policy::placeholder("tok");
    let req = Request::builder()
        .method(Method::GET)
        .uri("https://api.example/thing")
        .header("authorization", format!("Bearer {placeholder}"))
        .body(Empty::<Bytes>::new())
        .unwrap();
    let resp = tokio::time::timeout(std::time::Duration::from_secs(5), sender.send_request(req))
        .await
        .expect("request timed out")
        .unwrap();

    // Answered over h2. It is a 502 because the "upstream" is a service, which
    // `open_tls` refuses — but we have already proven the guest-facing h2 path.
    assert_eq!(resp.version(), Version::HTTP_2, "answered over h2");
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);

    // The request was inspected and the secret injected, over h2.
    let ev = net_event(&t, &s, 0).await;
    assert_eq!(ev["inspected"], true, "{ev}");
    assert_eq!(ev["method"], "GET", "{ev}");
    assert!(
        ev["secrets"].as_array().unwrap().iter().any(|x| x == "tok"),
        "secret injected over h2: {ev}"
    );
}
