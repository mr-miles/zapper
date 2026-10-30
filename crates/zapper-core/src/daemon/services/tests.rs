//! Services and zones over a core without VMs: which sessions a name is for,
//! where `host:port` goes, what resolves without the host, and connecting.

use crate::prelude::*;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::testing::{dial, echoing_vm, guest_tls, net_event};

use super::{check_name, Handler, Io, Route, Spec, Target};
use crate::testing::TestCore;
use crate::{NewSession, Session};

fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn session(t: &TestCore, pairs: &[(&str, &str)]) -> Arc<Session> {
    let mut new = NewSession::plain("web".into(), t.workdir(), Default::default());
    new.labels = labels(pairs);
    t.core.test_session(new, true).unwrap()
}

/// Echoes what it is sent, and remembers who connected.
#[derive(Default)]
struct Echo {
    seen: Mutex<Vec<String>>,
}

impl Handler for Echo {
    fn serve(&self, s: Arc<Session>, mut io: Box<dyn Io>) {
        self.seen.lock().unwrap().push(s.id.clone());
        tokio::spawn(async move {
            let mut buf = [0u8; 64];
            if let Ok(n) = io.read(&mut buf).await {
                let _ = io.write_all(&buf[..n]).await;
            }
        });
    }
}

fn spec(name: &str, port: u16, sessions: &[(&str, &str)], target: Target) -> Spec {
    Spec {
        name: name.into(),
        port,
        sessions: labels(sessions),
        target,
        owner: "test".into(),
    }
}

fn to_session(id: &str, port: u32) -> Target {
    Target::Session {
        id: id.into(),
        port,
    }
}

/// Where `host:port` goes for `s`, in words.
fn route(t: &TestCore, s: &Session, host: &str, port: u16) -> String {
    match t.core.daemon().services.route(s, host, port) {
        Some(Route::Service(Target::Session { id, port })) => format!("{id}:{port}"),
        Some(Route::Service(Target::Addr(a))) => a.to_string(),
        Some(Route::Service(Target::Handler { .. })) => "handler".into(),
        Some(Route::Service(Target::Socket { name, .. })) => format!("socket {name}"),
        Some(Route::Unserved) => "unserved".into(),
        None => "host".into(),
    }
}

const CONFIG: &str = "profiles:\n  web: {}\n";

#[test]
fn names_are_host_names_or_everything_under_one() {
    assert_eq!(check_name("Git.Example.").unwrap(), "git.example");
    assert_eq!(check_name("*.apps.example").unwrap(), "*.apps.example");
    for bad in ["", "*", "*.", "a..b", "a b", "**.x", "x.*"] {
        assert!(check_name(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn a_service_is_for_the_sessions_its_labels_pick() {
    let t = TestCore::new(CONFIG);
    let acme = session(&t, &[("firm", "acme"), ("member", "pat")]);
    let other = session(&t, &[("firm", "other")]);
    let plain = session(&t, &[]);
    t.core
        .add_service(spec(
            "db.internal",
            5432,
            &[("firm", "acme")],
            to_session("abc", 5432),
        ))
        .unwrap();
    assert_eq!(route(&t, &acme, "db.internal", 5432), "abc:5432");
    assert_eq!(route(&t, &other, "db.internal", 5432), "host");
    assert_eq!(route(&t, &plain, "db.internal", 5432), "host");

    // No labels: every session's.
    t.core
        .add_service(spec("cache.internal", 6379, &[], to_session("def", 6379)))
        .unwrap();
    assert_eq!(route(&t, &plain, "cache.internal", 6379), "def:6379");
    assert_eq!(route(&t, &other, "cache.internal", 6379), "def:6379");
}

#[test]
fn a_services_other_ports_go_nowhere_and_exact_names_win() {
    let t = TestCore::new(CONFIG);
    let s = session(&t, &[]);
    let add = |name: &str, port: u16, to: &str| {
        t.core
            .add_service(spec(name, port, &[], to_session(to, port as u32)))
            .unwrap()
    };
    add("*.apps.x", 443, "wild");
    add("shop.apps.x", 443, "exact");
    assert_eq!(route(&t, &s, "shop.apps.x", 443), "exact:443");
    assert_eq!(route(&t, &s, "blog.apps.x", 443), "wild:443");
    assert_eq!(route(&t, &s, "a.b.apps.x", 443), "wild:443");
    assert_eq!(
        route(&t, &s, "apps.x", 443),
        "host",
        "`*.` is names under it"
    );
    // A service's name is never looked up, on any port.
    assert_eq!(route(&t, &s, "shop.apps.x", 80), "unserved");
    assert_eq!(route(&t, &s, "blog.apps.x", 22), "unserved");

    // The earliest wins among equals; the same name, port and sessions
    // twice is refused.
    let e = t
        .core
        .add_service(spec("shop.apps.x", 443, &[], to_session("again", 443)))
        .unwrap_err()
        .to_string();
    assert!(e.contains("already a service"), "{e}");
    assert!(t
        .core
        .add_service(spec("x", 0, &[], to_session("a", 1)))
        .is_err());
    assert!(t
        .core
        .add_service(spec("bad name", 1, &[], to_session("a", 1)))
        .is_err());
}

#[test]
fn a_zones_names_are_its_services_or_nothing() {
    let t = TestCore::new(CONFIG);
    let inside = session(&t, &[("firm", "acme")]);
    let outside = session(&t, &[]);
    t.core
        .add_zone("acme.internal", labels(&[("firm", "acme")]), "test")
        .unwrap();
    t.core
        .add_service(spec(
            "git.acme.internal",
            443,
            &[("firm", "acme")],
            to_session("g", 443),
        ))
        .unwrap();
    let services = &t.core.daemon().services;
    assert_eq!(services.resolves(&inside, "git.acme.internal"), Some(true));
    assert_eq!(services.resolves(&inside, "x.acme.internal"), Some(false));
    assert_eq!(services.resolves(&inside, "acme.internal"), Some(false));
    assert_eq!(services.resolves(&inside, "example.com"), None);
    assert_eq!(services.resolves(&outside, "git.acme.internal"), None);
    assert_eq!(route(&t, &inside, "x.acme.internal", 443), "unserved");
    assert_eq!(route(&t, &inside, "notacme.internal", 443), "host");
    assert_eq!(route(&t, &outside, "x.acme.internal", 443), "host");
}

#[test]
fn services_are_listed_removed_and_events() {
    let t = TestCore::new(CONFIG);
    let mut events = t.core.subscribe();
    let id = t
        .core
        .add_service(spec(
            "db.internal",
            5432,
            &[("team", "a")],
            to_session("abc", 5432),
        ))
        .unwrap();
    let zone = t
        .core
        .add_zone("a.internal", BTreeMap::new(), "test")
        .unwrap();
    let list = serde_json::to_value(t.core.services()).unwrap();
    assert_eq!(list[0]["name"], "db.internal");
    assert_eq!(list[0]["port"], 5432);
    assert_eq!(list[0]["to"], "session abc port 5432");
    assert_eq!(list[0]["sessions"]["team"], "a");
    assert_eq!(list[1]["kind"], "zone");
    let e = serde_json::to_value(events.try_recv().unwrap()).unwrap();
    assert_eq!(e["type"], "service");
    assert_eq!(e["change"], "added");
    assert_eq!(e["name"], "db.internal");

    assert_eq!(t.core.remove_service(id).unwrap().name, "db.internal");
    assert!(t.core.remove_service(id).is_none());
    t.core.remove_service(zone).unwrap();
    assert!(t.core.services().is_empty());
}

#[tokio::test]
async fn connecting_to_a_handler_hands_it_the_session() {
    let t = TestCore::new(CONFIG);
    let s = session(&t, &[]);
    let echo = Arc::new(Echo::default());
    let target = Target::Handler {
        tls: false,
        handler: echo.clone(),
    };
    let mut io = super::connect(t.core.daemon(), &s, "echo.internal", 1, &target)
        .await
        .unwrap();
    io.write_all(b"hello").await.unwrap();
    let mut buf = [0u8; 5];
    io.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"hello");
    assert_eq!(
        echo.seen.lock().unwrap().as_slice(),
        std::slice::from_ref(&s.id)
    );
}

#[tokio::test]
async fn addresses_keep_to_the_address_policy_and_gone_sessions_say_so() {
    let t = TestCore::new(CONFIG);
    let s = session(&t, &[]);
    let local = Target::Addr("127.0.0.1:5432".parse().unwrap());
    let e = super::connect(t.core.daemon(), &s, "db.internal", 1, &local)
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(e.starts_with("127.0.0.1:5432"), "{e}");
    let gone = to_session("abcdef", 6443);
    let e = super::connect(t.core.daemon(), &s, "cluster.internal", 1, &gone)
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(e.contains("which serves cluster.internal, is gone"), "{e}");
}

#[test]
fn a_zone_is_a_domain() {
    let t = TestCore::new(CONFIG);
    assert!(t
        .core
        .add_zone("not a domain", BTreeMap::new(), "test")
        .is_err());
}

/// A handler is handed a byte stream and speaks HTTP/1.1 on it (the firm's
/// git server does): a client that would rather have HTTP/2, as git's curl
/// does, is given HTTP/1.1, not a protocol nobody on the other side speaks.
#[tokio::test]
async fn a_handler_behind_tls_negotiates_http1() {
    let t = TestCore::new(CONFIG);
    let s = session(&t, &[]);
    let target = Target::Handler {
        tls: true,
        handler: Arc::new(Echo::default()),
    };
    let io = super::connect(t.core.daemon(), &s, "echo.internal", 1, &target)
        .await
        .unwrap();
    let pem: String = s
        .tls
        .pem()
        .lines()
        .filter(|l| !l.starts_with("-----"))
        .collect();
    let der = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, pem).unwrap();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(der.into()).unwrap();
    let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    let name = rustls::pki_types::ServerName::try_from("echo.internal").unwrap();
    let tls = tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(name, io)
        .await
        .unwrap();
    assert_eq!(tls.get_ref().1.alpn_protocol(), Some(&b"http/1.1"[..]));
}

#[tokio::test]
async fn a_handler_behind_tls_gets_the_plain_stream() {
    let t = TestCore::new(CONFIG);
    let s = session(&t, &[]);
    let target = Target::Handler {
        tls: true,
        handler: Arc::new(Echo::default()),
    };
    let io = super::connect(t.core.daemon(), &s, "echo.internal", 1, &target)
        .await
        .unwrap();
    let name = rustls::pki_types::ServerName::try_from("echo.internal").unwrap();
    let mut tls = guest_tls(&s).connect(name, io).await.unwrap();
    tls.write_all(b"hello").await.unwrap();
    let mut buf = [0u8; 5];
    tls.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"hello");
}

#[tokio::test]
async fn a_sessions_port_is_reached_over_vsock() {
    let t = TestCore::new(CONFIG);
    let s = session(&t, &[]);
    let cluster = session(&t, &[]);
    echoing_vm(&cluster);
    let mut io = super::connect(
        t.core.daemon(),
        &s,
        "k3s.internal",
        6443,
        &to_session(&cluster.id, 6443),
    )
    .await
    .unwrap();
    io.write_all(b"ping").await.unwrap();
    let mut buf = [0u8; 4];
    io.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"ping");
}

// Through the proxy, as the guest's connections arrive.

fn allow(t: &TestCore, s: &Arc<Session>, rule: &str) {
    t.core
        .add_session_rule(s, serde_yaml::from_str(rule).unwrap(), None);
}

#[tokio::test]
async fn an_operators_service_is_for_the_sessions_it_picks() {
    let t = TestCore::new(CONFIG);
    let a = session(&t, &[("team", "a")]);
    let b = session(&t, &[("team", "b")]);
    let echo = Arc::new(Echo::default());
    t.core
        .add_service(spec(
            "echo.internal",
            7,
            &[("team", "a")],
            Target::Handler {
                tls: false,
                handler: echo.clone(),
            },
        ))
        .unwrap();
    for s in [&a, &b] {
        allow(&t, s, "{ kind: net, action: allow, host: echo.internal }");
    }
    let mut io = dial(&t, &a, "echo.internal", 7);
    io.write_all(b"hi").await.unwrap();
    let mut buf = [0u8; 2];
    io.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"hi");
    assert_eq!(
        echo.seen.lock().unwrap().as_slice(),
        std::slice::from_ref(&a.id)
    );

    let _io = dial(&t, &b, "echo.internal", 7);
    let e = net_event(&t, &b, 0).await;
    assert!(
        e["error"]
            .as_str()
            .unwrap_or_default()
            .starts_with("resolving echo.internal"),
        "{e}"
    );
}

/// Answers one HTTP request with `200 served`.
struct Http;

impl Handler for Http {
    fn serve(&self, _s: Arc<Session>, mut io: Box<dyn Io>) {
        tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            let _ = io.read(&mut buf).await;
            let _ = io
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-length: 6\r\nconnection: close\r\n\r\nserved",
                )
                .await;
        });
    }
}

#[tokio::test]
async fn requests_to_a_service_are_inspected_over_http_not_tls() {
    let t = TestCore::new(CONFIG);
    let s = session(&t, &[]);
    let http = || Target::Handler {
        tls: false,
        handler: Arc::new(Http),
    };
    t.core
        .add_service(spec("web.internal", 80, &[], http()))
        .unwrap();
    t.core
        .add_service(spec("web.internal", 443, &[], http()))
        .unwrap();
    // A rule by method: each request is decided, so connections are
    // inspected.
    allow(
        &t,
        &s,
        "{ kind: net, action: allow, host: web.internal, methods: [GET] }",
    );
    let request = b"GET / HTTP/1.1\r\nHost: web.internal\r\nConnection: close\r\n\r\n";

    let mut io = dial(&t, &s, "web.internal", 80);
    io.write_all(request).await.unwrap();
    let mut reply = String::new();
    let _ = io.read_to_string(&mut reply).await;
    assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
    assert!(reply.ends_with("served"), "{reply}");

    // Over TLS to a handler that doesn't terminate TLS, the proxy would have
    // to open TLS to the service itself.
    let io = dial(&t, &s, "web.internal", 443);
    let name = rustls::pki_types::ServerName::try_from("web.internal").unwrap();
    let mut tls = guest_tls(&s).connect(name, io).await.unwrap();
    tls.write_all(request).await.unwrap();
    let mut reply = String::new();
    let _ = tls.read_to_string(&mut reply).await;
    assert!(
        reply.contains(
            "web.internal is a service whose TLS isn't the daemon's; rules with methods, paths or secrets don't apply to it"
        ),
        "{reply}"
    );
}

/// Sends `request` over TLS to `host:443` from `s`, through the proxy; the
/// reply as text.
async fn https(t: &TestCore, s: &Arc<Session>, host: &str, request: &str) -> String {
    let io = dial(t, s, host, 443);
    let name = rustls::pki_types::ServerName::try_from(host.to_string()).unwrap();
    let mut tls = guest_tls(s).connect(name, io).await.unwrap();
    tls.write_all(request.as_bytes()).await.unwrap();
    let mut reply = String::new();
    let _ = tls.read_to_string(&mut reply).await;
    reply
}

fn get(host: &str, path: &str) -> String {
    format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n")
}

#[tokio::test]
async fn https_to_a_service_behind_the_daemons_tls_is_inspected() {
    let t = TestCore::new(CONFIG);
    let s = session(&t, &[]);
    t.core
        .add_service(spec(
            "web.internal",
            443,
            &[],
            Target::Handler {
                tls: true,
                handler: Arc::new(Http),
            },
        ))
        .unwrap();
    allow(
        &t,
        &s,
        "{ kind: net, action: allow, host: web.internal, methods: [GET] }",
    );
    allow(&t, &s, "{ kind: net, action: deny, host: web.internal }");
    let reply = https(&t, &s, "web.internal", &get("web.internal", "/")).await;
    assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
    assert!(reply.ends_with("served"), "{reply}");
    let post =
        "POST / HTTP/1.1\r\nHost: web.internal\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    let reply = https(&t, &s, "web.internal", post).await;
    assert!(reply.starts_with("HTTP/1.1 403"), "{reply}");
}

/// Serves HTTP on socket `name`: reads the caller line, then answers every
/// request on the connection with `200` and the caller's session as body.
fn http_socket_server(t: &TestCore, name: &str) {
    let path = t
        .core
        .daemon()
        .paths
        .state
        .join(super::SOCKETS_DIR)
        .join(name);
    let listener = tokio::net::UnixListener::bind(path).unwrap();
    tokio::spawn(async move {
        while let Ok((sock, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut reader = tokio::io::BufReader::new(sock);
                let mut line = String::new();
                tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut line)
                    .await
                    .unwrap();
                let caller: super::Caller = serde_json::from_str(&line).unwrap();
                loop {
                    let mut head = String::new();
                    loop {
                        let mut l = String::new();
                        let n = tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut l)
                            .await
                            .unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        if l == "\r\n" {
                            break;
                        }
                        head.push_str(&l);
                    }
                    let body = &caller.session;
                    let reply = format!(
                        "HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n{body}",
                        body.len()
                    );
                    if reader.get_mut().write_all(reply.as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
}

#[tokio::test]
async fn a_socket_services_paths_are_decided_for_the_calling_session() {
    let t = TestCore::new(CONFIG);
    let a = session(&t, &[("team", "a")]);
    let b = session(&t, &[("team", "b")]);
    http_socket_server(&t, "drive.sock");
    t.core
        .add_service(spec("drive.internal", 443, &[], socket("drive.sock", true)))
        .unwrap();
    for s in [&a, &b] {
        allow(
            &t,
            s,
            "{ kind: net, action: allow, host: drive.internal, path: \"/outbox/{session}/**\" }",
        );
        allow(
            &t,
            s,
            "{ kind: net, action: allow, host: drive.internal, methods: [GET], path: \"/teams/{label:team}/**\" }",
        );
        allow(&t, s, "{ kind: net, action: deny, host: drive.internal }");
    }
    let own = format!("/outbox/{}/report.csv", a.id);
    let reply = https(&t, &a, "drive.internal", &get("drive.internal", &own)).await;
    assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
    assert!(
        reply.ends_with(&a.id),
        "the socket is told the caller: {reply}"
    );
    // b can't reach a's folder, and a can't reach b's team.
    let reply = https(&t, &b, "drive.internal", &get("drive.internal", &own)).await;
    assert!(reply.starts_with("HTTP/1.1 403"), "{reply}");
    let reply = https(
        &t,
        &a,
        "drive.internal",
        &get("drive.internal", "/teams/a/x"),
    )
    .await;
    assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
    let reply = https(
        &t,
        &a,
        "drive.internal",
        &get("drive.internal", "/teams/b/x"),
    )
    .await;
    assert!(reply.starts_with("HTTP/1.1 403"), "{reply}");
}

fn socket(name: &str, tls: bool) -> Target {
    Target::Socket {
        name: name.into(),
        tls,
    }
}

/// Listens on `name` in the daemon's sockets directory: for one connection,
/// sends back the caller line it was given, then echoes.
fn socket_server(t: &TestCore, name: &str) -> tokio::task::JoinHandle<super::Caller> {
    let path = t
        .core
        .daemon()
        .paths
        .state
        .join(super::SOCKETS_DIR)
        .join(name);
    let listener = tokio::net::UnixListener::bind(path).unwrap();
    tokio::spawn(async move {
        let (sock, _) = listener.accept().await.unwrap();
        let mut reader = tokio::io::BufReader::new(sock);
        let mut line = String::new();
        tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut line)
            .await
            .unwrap();
        let caller: super::Caller = serde_json::from_str(&line).unwrap();
        // Read on through the buffer: the guest's bytes may have come in
        // with the caller line.
        let mut buf = [0u8; 64];
        let n = reader.read(&mut buf).await.unwrap();
        reader.get_mut().write_all(&buf[..n]).await.unwrap();
        caller
    })
}

#[test]
fn socket_names_stay_in_the_sockets_directory() {
    let t = TestCore::new(CONFIG);
    for bad in [
        "",
        "../api.sock",
        "a/b",
        ".hidden",
        "/run/docker.sock",
        "x y",
    ] {
        let e = t
            .core
            .add_service(spec("s.internal", 1, &[], socket(bad, false)))
            .unwrap_err();
        assert!(format!("{e}").contains("not a socket name"), "{bad}: {e}");
    }
    let id = t
        .core
        .add_service(spec("s.internal", 1, &[], socket("drive.sock", true)))
        .unwrap();
    let info = t.core.services().into_iter().find(|i| i.id == id).unwrap();
    assert_eq!(info.to, "socket drive.sock (TLS terminated)");
}

#[tokio::test]
async fn a_socket_service_is_told_the_caller_before_the_guests_bytes() {
    let t = TestCore::new(CONFIG);
    let s = session(&t, &[("team", "a")]);
    let server = socket_server(&t, "echo.sock");
    let mut io = super::connect(
        t.core.daemon(),
        &s,
        "echo.internal",
        7,
        &socket("echo.sock", false),
    )
    .await
    .unwrap();
    // The guest's first line looks like a caller line: it is only data.
    io.write_all(b"{\"session\":\"other\"}\n").await.unwrap();
    let mut buf = [0u8; 20];
    io.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"{\"session\":\"other\"}\n");
    let caller = server.await.unwrap();
    assert_eq!(
        caller,
        super::Caller {
            session: s.id.clone(),
            labels: labels(&[("team", "a")]),
            name: "echo.internal".into(),
            port: 7,
        }
    );
}

#[tokio::test]
async fn a_socket_service_behind_tls_gets_the_plain_stream() {
    let t = TestCore::new(CONFIG);
    let s = session(&t, &[]);
    let server = socket_server(&t, "web.sock");
    let io = super::connect(
        t.core.daemon(),
        &s,
        "web.internal",
        443,
        &socket("web.sock", true),
    )
    .await
    .unwrap();
    let name = rustls::pki_types::ServerName::try_from("web.internal").unwrap();
    let mut tls = guest_tls(&s).connect(name, io).await.unwrap();
    assert_eq!(tls.get_ref().1.alpn_protocol(), None);
    tls.write_all(b"hello").await.unwrap();
    let mut buf = [0u8; 5];
    tls.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"hello");
    assert_eq!(server.await.unwrap().port, 443);
}

#[tokio::test]
async fn a_socket_nobody_serves_says_so() {
    let t = TestCore::new(CONFIG);
    let s = session(&t, &[]);
    let e = match super::connect(
        t.core.daemon(),
        &s,
        "gone.internal",
        80,
        &socket("gone.sock", false),
    )
    .await
    {
        Err(e) => format!("{e:#}"),
        Ok(_) => panic!("connected to a socket nobody serves"),
    };
    assert!(
        e.contains("connecting to socket gone.sock, which serves gone.internal"),
        "{e}"
    );
}
