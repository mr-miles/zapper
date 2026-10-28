//! Fail-closed properties of the egress proxy, over a core without VMs:
//! a passthrough tunnel is pinned to the TLS server name, and an inspected
//! request whose secret cannot be resolved is refused before it is sent
//! upstream (the request never leaves without the credential it needed).

use crate::prelude::*;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::Empty;
use hyper::{Method, Request, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::io::AsyncWriteExt;

use crate::testing::{dial, guest_tls, net_event, TestCore};
use crate::{NewSession, Session};

fn session(t: &TestCore) -> Arc<Session> {
    t.core
        .test_session(
            NewSession::plain("web".into(), t.workdir(), Default::default()),
            true,
        )
        .unwrap()
}

/// A service handler, registered only so `target()` resolves and the tunnel /
/// MITM runs; these tests never reach the upstream.
struct NeverServed;
impl crate::ServiceHandler for NeverServed {
    fn serve(&self, _s: Arc<Session>, _io: Box<dyn crate::ServiceIo>) {}
}

fn service(t: &TestCore, name: &str, tls: bool) {
    t.core
        .add_service(crate::ServiceSpec {
            name: name.into(),
            port: 443,
            sessions: Default::default(),
            target: crate::ServiceTarget::Handler {
                tls,
                handler: Arc::new(NeverServed),
            },
            owner: "test".into(),
        })
        .unwrap();
}

/// A TLS ClientHello naming `name`.
fn client_hello(name: &str) -> Vec<u8> {
    let mut c = rustls::ClientConnection::new(
        crate::daemon::tls::client_config(),
        name.to_string().try_into().unwrap(),
    )
    .unwrap();
    let mut out = Vec::new();
    c.write_tls(&mut out).unwrap();
    out
}

/// A passthrough tunnel is bound to the TLS server name: a ClientHello that
/// names another host than the one allowed/dialled is refused before any
/// upstream connection, so an allowed host cannot front for another.
#[tokio::test]
async fn a_passthrough_tls_hello_that_names_another_host_is_refused() {
    let t = TestCore::new(
        "profiles:\n  web:\n    policy:\n      net:\n        - { action: allow, host: a.example }\n",
    );
    let s = session(&t);
    service(&t, "a.example", false);
    let mut io = dial(&t, &s, "a.example", 443);
    io.write_all(&client_hello("b.example")).await.unwrap();
    let ev = net_event(&t, &s, 0).await;
    assert_eq!(ev["action"], "deny", "{ev}");
    assert!(
        ev["error"].as_str().unwrap().contains("does not match"),
        "{ev}"
    );
}

/// A ClientHello naming the dialled host is accepted into the tunnel (the
/// control case for the one above).
#[tokio::test]
async fn a_passthrough_tls_hello_that_names_the_dialled_host_is_allowed() {
    let t = TestCore::new(
        "profiles:\n  web:\n    policy:\n      net:\n        - { action: allow, host: a.example }\n",
    );
    let s = session(&t);
    service(&t, "a.example", false);
    let mut io = dial(&t, &s, "a.example", 443);
    io.write_all(&client_hello("a.example")).await.unwrap();
    let ev = net_event(&t, &s, 0).await;
    assert_eq!(ev["action"], "allow", "{ev}");
    assert!(ev["error"].is_null(), "{ev}");
}

/// An inspected request whose secret cannot be resolved is refused (403) and
/// never sent upstream — it fails closed rather than going out without the
/// credential the rule required.
#[tokio::test]
async fn an_unresolvable_secret_refuses_the_request() {
    let t = TestCore::new(
        "secrets:\n  tok: { file: /nonexistent/zapper-secret-test, hosts: [api.example] }\nprofiles:\n  web:\n    policy:\n      net:\n        - { action: allow, host: api.example, methods: [GET], secrets: [tok] }\n",
    );
    let s = session(&t);
    // A service so target() resolves and the MITM runs; inject fails before
    // open_tls, so the upstream is never reached.
    service(&t, "api.example", true);
    let io = dial(&t, &s, "api.example", 443);
    let name = rustls::pki_types::ServerName::try_from("api.example").unwrap();
    let tls = guest_tls(&s).connect(name, io).await.unwrap();
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
        .await
        .unwrap();
    tokio::spawn(conn);
    // The guest holds only the placeholder; the real secret can't be read.
    let placeholder = zapper_policy::placeholder("tok");
    let req = Request::builder()
        .method(Method::GET)
        .uri("https://api.example/x")
        .header("authorization", format!("Bearer {placeholder}"))
        .body(Empty::<Bytes>::new())
        .unwrap();
    let resp = sender.send_request(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let ev = net_event(&t, &s, 0).await;
    assert_eq!(ev["action"], "deny", "{ev}");
    assert!(ev["error"].as_str().unwrap().contains("tok"), "{ev}");
}

/// Over plain HTTP, a secret that is not marked `plaintext: true` is refused
/// (not sent in the clear); the request fails closed with 403.
#[tokio::test]
async fn a_non_plaintext_secret_is_refused_over_plain_http() {
    let t = TestCore::new(
        "secrets:\n  s: { value: the-value, hosts: [api.example] }\nprofiles:\n  web:\n    policy:\n      net:\n        - { action: allow, host: api.example, ports: [80], methods: [GET], secrets: [s] }\n",
    );
    let s = session(&t);
    // Plain-HTTP service (no TLS); target() resolves, inject fails first.
    t.core
        .add_service(crate::ServiceSpec {
            name: "api.example".into(),
            port: 80,
            sessions: Default::default(),
            target: crate::ServiceTarget::Handler {
                tls: false,
                handler: Arc::new(NeverServed),
            },
            owner: "test".into(),
        })
        .unwrap();
    let io = dial(&t, &s, "api.example", 80);
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(io))
        .await
        .unwrap();
    tokio::spawn(conn);
    let placeholder = zapper_policy::placeholder("s");
    let req = Request::builder()
        .method(Method::GET)
        .uri("http://api.example/x")
        .header("authorization", format!("Bearer {placeholder}"))
        .body(Empty::<Bytes>::new())
        .unwrap();
    let resp = sender.send_request(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let ev = net_event(&t, &s, 0).await;
    assert_eq!(ev["action"], "deny", "{ev}");
    assert!(ev["error"].as_str().unwrap().contains("plain HTTP"), "{ev}");
}

/// An inspected HTTP/1 request through the proxy to `api.example`, carrying
/// the placeholder of `name`; the response status.
async fn send_with_placeholder(t: &TestCore, s: &Arc<Session>, name: &str) -> StatusCode {
    let io = dial(t, s, "api.example", 443);
    let server = rustls::pki_types::ServerName::try_from("api.example").unwrap();
    let tls = guest_tls(s).connect(server, io).await.unwrap();
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
        .await
        .unwrap();
    tokio::spawn(conn);
    let req = Request::builder()
        .method(Method::GET)
        .uri("https://api.example/x")
        .header(
            "authorization",
            format!("Bearer {}", zapper_policy::placeholder(name)),
        )
        .body(Empty::<Bytes>::new())
        .unwrap();
    sender.send_request(req).await.unwrap().status()
}

/// A secret minted by a host command is run on first use and reused for the
/// requests after it, until it needs refreshing: the command is not run per
/// request. (The upstream is a service, so the request ends in a 502 after the
/// secret was resolved and injected.)
#[tokio::test]
async fn a_command_secret_is_minted_once_and_injected() {
    let runs = std::env::temp_dir().join(format!("zapper-mint-runs-{}", std::process::id()));
    let _ = std::fs::remove_file(&runs);
    let script = format!(
        "echo run >> {}; printf '{{\"value\":\"ghs_minted\",\"expires_at\":\"2099-01-01T00:00:00Z\"}}'",
        runs.display()
    );
    let t = TestCore::new(&format!(
        "secrets:\n  tok: {{ command: [sh, -c, {script:?}], hosts: [api.example] }}\nprofiles:\n  web:\n    policy:\n      net:\n        - {{ action: allow, host: api.example, methods: [GET], secrets: [tok] }}\n"
    ));
    let s = session(&t);
    service(&t, "api.example", true);
    for i in 0..3 {
        assert_eq!(
            send_with_placeholder(&t, &s, "tok").await,
            StatusCode::BAD_GATEWAY
        );
        let ev = net_event(&t, &s, i).await;
        // Past injection, the service target is what stops it: the secret
        // was resolved and used, not refused.
        assert!(
            ev["error"].as_str().unwrap().contains("is a service"),
            "{ev}"
        );
        assert!(
            ev["secrets"].as_array().unwrap().iter().any(|x| x == "tok"),
            "injected: {ev}"
        );
    }
    assert_eq!(
        std::fs::read_to_string(&runs).unwrap().lines().count(),
        1,
        "minted once for three requests"
    );
    let _ = std::fs::remove_file(&runs);
}

/// When the command fails, the request is refused and never sent: the error
/// names the secret and the command's own message, and nothing it printed on
/// stdout.
#[tokio::test]
async fn a_failing_command_secret_refuses_the_request() {
    let t = TestCore::new(
        "secrets:\n  tok: { command: [sh, -c, \"echo LEAKED_STDOUT; echo 'HTTP 401: Bad credentials' >&2; exit 1\"], hosts: [api.example] }\nprofiles:\n  web:\n    policy:\n      net:\n        - { action: allow, host: api.example, methods: [GET], secrets: [tok] }\n",
    );
    let s = session(&t);
    service(&t, "api.example", true);
    assert_eq!(
        send_with_placeholder(&t, &s, "tok").await,
        StatusCode::FORBIDDEN
    );
    let ev = net_event(&t, &s, 0).await;
    assert_eq!(ev["action"], "deny", "{ev}");
    let err = ev["error"].as_str().unwrap();
    assert!(
        err.contains("tok") && err.contains("Bad credentials"),
        "{ev}"
    );
    assert!(!ev.to_string().contains("LEAKED_STDOUT"), "{ev}");
}

fn gh_secret(script: &str) -> zapper_policy::Secret {
    zapper_policy::Secret {
        source: zapper_policy::SecretSource::Command {
            command: vec!["sh".into(), "-c".into(), script.into()],
            timeout_secs: None,
            ttl_secs: Some(600),
        },
        mode: zapper_policy::SecretMode::Proxy,
        hosts: vec!["api.example".into()],
        plaintext: false,
        user: None,
        database: None,
        mechanism: None,
    }
}

/// A session's own secret is used by that session's rules, injected like any
/// other, and is nowhere in the config.
#[tokio::test]
async fn a_session_secret_is_injected_for_its_own_session_only() {
    let t = TestCore::new("profiles:\n  web:\n    policy:\n      net: []\n");
    let (a, b) = (session(&t), session(&t));
    service(&t, "api.example", true);
    let rule = || {
        zapper_policy::AnyRule::Net(zapper_policy::NetRule {
            id: Some("gh".into()),
            action: zapper_policy::Action::Allow,
            host: "api.example".into(),
            ports: vec![443],
            methods: vec![],
            path: None,
            secrets: vec![],
            set_headers: [(
                "Authorization".to_string(),
                "Bearer {{secret:gh-a}}".to_string(),
            )]
            .into(),
            final_: false,
            comment: None,
            protocol: Default::default(),
        })
    };
    a.put_secret("gh-a", gh_secret("echo own_token")).unwrap();
    a.put_rule(rule());
    b.put_rule(rule());

    // The session that has the secret uses it (the service target then stops it).
    send_with_placeholder(&t, &a, "gh-a").await;
    let ev = net_event(&t, &a, 0).await;
    assert!(
        ev["error"].as_str().unwrap().contains("is a service"),
        "{ev}"
    );
    assert!(
        ev["secrets"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x == "gh-a"),
        "{ev}"
    );

    // Another session whose rule names it has no such secret: refused.
    assert_eq!(
        send_with_placeholder(&t, &b, "gh-a").await,
        StatusCode::FORBIDDEN
    );
    let ev = net_event(&t, &b, 0).await;
    assert!(
        ev["error"]
            .as_str()
            .unwrap()
            .contains("unknown secret gh-a"),
        "{ev}"
    );

    assert!(
        !t.core.config().to_yaml().contains("gh-a"),
        "the config never holds it"
    );
    assert_eq!(a.secret_names(), ["gh-a"]);
    assert!(a.remove_secret("gh-a"));
    assert!(!a.remove_secret("gh-a"));
}

#[tokio::test]
async fn a_session_secret_cannot_take_a_config_secrets_name_or_be_a_guest_secret() {
    let t = TestCore::new("secrets:\n  claude: { value: x, hosts: [api.example] }\nprofiles:\n  web:\n    policy:\n      net: []\n");
    let s = session(&t);
    let e = s
        .put_secret("claude", gh_secret("echo x"))
        .unwrap_err()
        .to_string();
    assert!(e.contains("in the config"), "{e}");
    let mut guest = gh_secret("echo x");
    guest.mode = zapper_policy::SecretMode::Guest;
    assert!(s.put_secret("gh-x", guest).is_err());
    let mut no_hosts = gh_secret("echo x");
    no_hosts.hosts.clear();
    assert!(s.put_secret("gh-x", no_hosts).is_err());
    assert!(s.put_secret("Bad_Name", gh_secret("echo x")).is_err());
}

/// An HTTP upstream that answers with the path it was asked for, the
/// override headers it saw, and every request header, in its body and in an
/// `x-seen-authorization` response header: what an echo API leaks.
struct HttpEcho;
impl crate::ServiceHandler for HttpEcho {
    fn serve(&self, _s: Arc<Session>, io: Box<dyn crate::ServiceIo>) {
        tokio::spawn(async move {
            let svc =
                hyper::service::service_fn(|req: Request<hyper::body::Incoming>| async move {
                    let mut body = format!(
                        "path={}\n",
                        req.uri().path_and_query().map_or("", |p| p.as_str())
                    );
                    for (k, v) in req.headers() {
                        body.push_str(&format!("{k}: {}\n", String::from_utf8_lossy(v.as_bytes())));
                    }
                    let auth = req
                        .headers()
                        .get("authorization")
                        .cloned()
                        .unwrap_or(hyper::header::HeaderValue::from_static(""));
                    Ok::<_, std::convert::Infallible>(
                        hyper::Response::builder()
                            .header("x-seen-authorization", auth)
                            .body(http_body_util::Full::new(Bytes::from(body)))
                            .unwrap(),
                    )
                });
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(io), svc)
                .await;
        });
    }
}

/// A plain-HTTP echo at `api.example:80`, and a session on `rules`.
fn echo_core(rules: &str) -> (TestCore, Arc<Session>) {
    let t = TestCore::new(&format!(
        "secrets:\n  s: {{ value: the-secret-value-123, hosts: [api.example], plaintext: true }}\nprofiles:\n  web:\n    policy:\n      net:\n{rules}"
    ));
    let s = session(&t);
    t.core
        .add_service(crate::ServiceSpec {
            name: "api.example".into(),
            port: 80,
            sessions: Default::default(),
            target: crate::ServiceTarget::Handler {
                tls: false,
                handler: Arc::new(HttpEcho),
            },
            owner: "test".into(),
        })
        .unwrap();
    (t, s)
}

/// One request through the proxy: its status, the headers and the body the
/// guest got.
async fn request(
    t: &TestCore,
    s: &Arc<Session>,
    method: Method,
    path: &str,
    headers: &[(&str, String)],
) -> (StatusCode, hyper::HeaderMap, String) {
    use http_body_util::BodyExt;
    let io = dial(t, s, "api.example", 80);
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(io))
        .await
        .unwrap();
    tokio::spawn(conn);
    let mut req = Request::builder()
        .method(method)
        .uri(format!("http://api.example{path}"));
    for (k, v) in headers {
        req = req.header(*k, v);
    }
    let resp = sender
        .send_request(req.body(Empty::<Bytes>::new()).unwrap())
        .await
        .unwrap();
    let (status, headers) = (resp.status(), resp.headers().clone());
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    (status, headers, String::from_utf8_lossy(&body).into_owned())
}

/// A path rule judges the path the upstream will act on: dot segments and
/// percent-encoded spellings can't step past a deny, and what is forwarded
/// is the path that was checked.
#[tokio::test]
async fn a_path_rule_cant_be_stepped_around() {
    let (t, s) = echo_core(
        "        - { action: deny, host: api.example, ports: [80], path: \"/admin/*\" }\n        - { action: allow, host: api.example, ports: [80], methods: [GET] }\n",
    );
    let get = |p: &'static str| request(&t, &s, Method::GET, p, &[]);
    assert_eq!(get("/admin/users").await.0, StatusCode::FORBIDDEN);
    for sneaky in [
        "/x/../admin/users",
        "/admin/./users",
        "/%61dmin/users",
        "/x/%2e%2e/admin/users",
        "/x/%2E%2E/admin/users",
        "/./admin/users",
    ] {
        let (status, _, body) = get(sneaky).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{sneaky}: {body}");
    }
    // An encoded slash that would make a dot segment once decoded is
    // refused outright: upstreams differ on decoding it.
    for ambiguous in [
        "/x/..%2fadmin/users",
        "/x/%2e%2e%2Fadmin/users",
        "/x\\..\\admin/users",
    ] {
        let (status, _, body) = get(ambiguous).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{ambiguous}: {body}");
    }
    // What is allowed is forwarded as it was judged.
    let (status, _, body) = get("/a/b/../c/%7euser?q=%2e%2e").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.starts_with("path=/a/c/~user?q=%2e%2e\n"), "{body}");
    // An encoded slash inside a segment stays one segment, as sent.
    let (status, _, body) = get("/projects/group%2Frepo").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.starts_with("path=/projects/group%2Frepo\n"), "{body}");
}

/// A request that asks the upstream to run another method (an override
/// header or query parameter) is refused: rules judge the request line.
#[tokio::test]
async fn a_method_override_is_refused() {
    let (t, s) =
        echo_core("        - { action: allow, host: api.example, ports: [80], methods: [GET] }\n");
    let h = |k: &'static str, v: &'static str| vec![(k, v.to_string())];
    for (path, headers) in [
        ("/x", h("x-http-method-override", "DELETE")),
        ("/x", h("X-HTTP-Method", "put")),
        ("/x", h("x-method-override", "GET, POST")),
        ("/x", h("x-goog-http-method-override", "PATCH")),
        ("/x?_method=DELETE", vec![]),
        ("/x?a=1&_HttpMethod=DELETE", vec![]),
        ("/x?%5Fmethod=post", vec![]),
    ] {
        let (status, _, body) = request(&t, &s, Method::GET, path, &headers).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path} {headers:?}: {body}");
        assert!(body.contains("method"), "{body}");
    }
    // Naming the request's own method is harmless.
    let (status, _, _) = request(
        &t,
        &s,
        Method::GET,
        "/x?_method=get",
        &h("x-http-method-override", "GET"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

/// A secret the proxy added never comes back to the guest in the response:
/// an echo of the request's headers is masked, in headers and in the body.
#[tokio::test]
async fn an_injected_secret_is_masked_in_the_response() {
    let (t, s) = echo_core("        - { action: allow, host: api.example, ports: [80], methods: [GET], secrets: [s] }\n");
    let auth = format!("Bearer {}", zapper_policy::placeholder("s"));
    let (status, headers, body) =
        request(&t, &s, Method::GET, "/x", &[("authorization", auth)]).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body.contains("the-secret-value-123"), "{body}");
    assert!(
        body.contains("authorization: Bearer ********************"),
        "{body}"
    );
    let seen = headers["x-seen-authorization"].to_str().unwrap();
    assert!(!seen.contains("the-secret-value-123"), "{seen}");
    // The upstream did get the value.
    assert_eq!(seen.len(), "Bearer the-secret-value-123".len());
}
