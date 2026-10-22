//! Services and zones over a core without VMs: which sessions a name is for,
//! where `host:port` goes, what resolves without the host, and connecting.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::daemon::proxy::service_tests::{dial, echoing_vm, guest_tls, net_event};

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
    let mut io = super::connect(t.core.daemon(), &s, "echo.internal", &target)
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
    let e = super::connect(t.core.daemon(), &s, "db.internal", &local)
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(e.starts_with("127.0.0.1:5432"), "{e}");
    let gone = to_session("abcdef", 6443);
    let e = super::connect(t.core.daemon(), &s, "cluster.internal", &gone)
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

#[tokio::test]
async fn a_handler_behind_tls_gets_the_plain_stream() {
    let t = TestCore::new(CONFIG);
    let s = session(&t, &[]);
    let target = Target::Handler {
        tls: true,
        handler: Arc::new(Echo::default()),
    };
    let io = super::connect(t.core.daemon(), &s, "echo.internal", &target)
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

    // Over TLS the proxy would have to open TLS to the service itself.
    let io = dial(&t, &s, "web.internal", 443);
    let name = rustls::pki_types::ServerName::try_from("web.internal").unwrap();
    let mut tls = guest_tls(&s).connect(name, io).await.unwrap();
    tls.write_all(request).await.unwrap();
    let mut reply = String::new();
    let _ = tls.read_to_string(&mut reply).await;
    assert!(
        reply.contains(
            "web.internal is a service; rules with methods, paths or secrets don't apply to it"
        ),
        "{reply}"
    );
}
