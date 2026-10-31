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
/// MITM runs; these tests never reach the upstream. Registered without TLS:
/// HTTPS to it is then refused after a request is decided and its secrets
/// added, where a handler whose TLS the daemon terminates would be sent it.
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
    // open_upstream, so the upstream is never reached.
    service(&t, "api.example", false);
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
/// request. (The upstream is a service whose TLS isn't the daemon's, so the
/// request ends in a 502 after the secret was resolved and injected.)
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
    service(&t, "api.example", false);
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
    service(&t, "api.example", false);
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
    service(&t, "api.example", false);
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

/// `body` in the coding a path names (`/gzip`, `/deflate`, `/br`, `/zstd`),
/// else as it is.
fn compressed(path: &str, body: Vec<u8>) -> (&'static str, Vec<u8>) {
    use std::io::Write;
    let coding = ["gzip", "deflate", "br", "zstd"]
        .into_iter()
        .find(|c| path.starts_with(&format!("/{c}")));
    match coding {
        Some("gzip") => {
            let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            e.write_all(&body).unwrap();
            ("gzip", e.finish().unwrap())
        }
        Some("deflate") => {
            let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
            e.write_all(&body).unwrap();
            ("deflate", e.finish().unwrap())
        }
        Some("br") => {
            let mut out = Vec::new();
            {
                let mut e = brotli::CompressorWriter::new(&mut out, 4096, 5, 22);
                e.write_all(&body).unwrap();
            }
            ("br", out)
        }
        Some(_) => ("zstd", body),
        None => ("identity", body),
    }
}
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
                    // An upstream that compresses whatever it was asked for
                    // (`/zstd` only says so: a coding the proxy can't decode).
                    let (encoding, body) = compressed(req.uri().path(), body.into_bytes());
                    Ok::<_, std::convert::Infallible>(
                        hyper::Response::builder()
                            .header("x-seen-authorization", auth)
                            .header("content-encoding", encoding)
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

/// A response compressed though the proxy asked for it uncompressed, to a
/// request it added a secret to, is decoded, masked and sent on
/// uncompressed; one in a coding it can't decode is refused. Without a
/// secret added, it goes through as it came.
#[tokio::test]
async fn a_compressed_response_to_a_request_with_a_secret_is_decoded_and_masked() {
    let (t, s) = echo_core("        - { action: allow, host: api.example, ports: [80], methods: [GET], secrets: [s] }\n");
    let auth = format!("Bearer {}", zapper_policy::placeholder("s"));
    for coding in ["gzip", "deflate", "br"] {
        let path = format!("/{coding}");
        let (status, headers, body) = request(
            &t,
            &s,
            Method::GET,
            &path,
            &[("authorization", auth.clone())],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{coding}: {body}");
        assert!(
            !headers.contains_key("content-encoding"),
            "{coding}: {headers:?}"
        );
        assert!(
            body.contains(&format!("path={path}")),
            "{coding}: decoded: {body}"
        );
        assert!(!body.contains("the-secret-value-123"), "{coding}: {body}");
        assert!(
            body.contains("authorization: Bearer ********************"),
            "{coding}: {body}"
        );
    }
    let (status, _, body) = request(&t, &s, Method::GET, "/zstd", &[("authorization", auth)]).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert!(!body.contains("the-secret-value-123"), "{body}");
    assert!(body.contains("zstd-encoded"), "{body}");

    let (status, headers, _) = request(&t, &s, Method::GET, "/gzip", &[]).await;
    assert_eq!(status, StatusCode::OK, "no secret added, nothing to mask");
    assert_eq!(headers["content-encoding"], "gzip", "passed on as it came");
}

/// A body that decodes to far more than it is comes out in pieces, each
/// bounded, and whole.
#[tokio::test]
async fn a_decoded_body_comes_out_in_bounded_pieces() {
    use http_body_util::BodyExt;
    use std::io::Write;
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    e.write_all(&vec![b'a'; 8 << 20]).unwrap();
    let gz = e.finish().unwrap();
    assert!(gz.len() < 64 << 10, "{}", gz.len());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (io, _) = listener.accept().await.unwrap();
        let svc = hyper::service::service_fn(move |_req: Request<hyper::body::Incoming>| {
            let gz = gz.clone();
            async move {
                Ok::<_, std::convert::Infallible>(
                    hyper::Response::builder()
                        .header("content-encoding", "gzip")
                        .body(http_body_util::Full::new(Bytes::from(gz)))
                        .unwrap(),
                )
            }
        });
        let _ = hyper::server::conn::http1::Builder::new()
            .serve_connection(TokioIo::new(io), svc)
            .await;
    });
    let io = tokio::net::TcpStream::connect(addr).await.unwrap();
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(io))
        .await
        .unwrap();
    tokio::spawn(conn);
    let resp = sender
        .send_request(
            Request::builder()
                .uri("/")
                .body(Empty::<Bytes>::new())
                .unwrap(),
        )
        .await
        .unwrap();
    let mut body = super::sanitize::decode_response(resp, "gzip")
        .unwrap()
        .into_body();
    let (mut total, mut largest) = (0usize, 0usize);
    while let Some(frame) = body.frame().await {
        if let Ok(data) = frame.unwrap().into_data() {
            assert!(data.iter().all(|b| *b == b'a'));
            total += data.len();
            largest = largest.max(data.len());
        }
    }
    assert_eq!(total, 8 << 20, "whole");
    assert!(largest <= 2 << 20, "a piece of {largest} bytes");
}

/// A plain-HTTP echo at `api.example:80` and an `aws` secret, `aws`, whose
/// keys (with a session token) are the documentation's examples.
fn aws_core(rules: &str) -> (TestCore, Arc<Session>) {
    let creds = r#"{"AccessKeyId":"AKIAIOSFODNN7EXAMPLE","SecretAccessKey":"wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY","SessionToken":"the-session-token-1"}"#;
    let t = TestCore::new(&format!(
        "secrets:\n  aws: {{ value: '{creds}', mode: aws, hosts: [api.example], plaintext: true }}\nprofiles:\n  web:\n    policy:\n      net:\n{rules}"
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

/// What an SDK holding the fake key sends.
fn fake_signed() -> Vec<(&'static str, String)> {
    let fake = zapper_policy::aws_key_id("aws");
    vec![
        ("x-amz-date", "20991231T000000Z".into()),
        ("x-amz-content-sha256", "UNSIGNED-PAYLOAD".into()),
        (
            "authorization",
            format!("AWS4-HMAC-SHA256 Credential={fake}/20991231/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature={}", "0".repeat(64)),
        ),
    ]
}

/// A request the guest signed with the fake key reaches AWS signed with the
/// real one, the token added; the guest sees neither.
#[tokio::test]
async fn a_request_signed_with_the_fake_aws_key_is_signed_again() {
    let (t, s) = aws_core("        - { action: allow, host: api.example, ports: [80], methods: [GET], secrets: [aws] }\n");
    let (status, headers, body) = request(&t, &s, Method::GET, "/bucket/key", &fake_signed()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("Credential=AKIAIOSFODNN7EXAMPLE/"), "{body}");
    assert!(
        body.contains("SignedHeaders=host;x-amz-content-sha256;x-amz-date;x-amz-security-token"),
        "{body}"
    );
    assert!(!body.contains(&"0".repeat(64)), "{body}");
    assert!(!body.contains("ZAPPER"), "{body}");
    // The token was sent, and masked on the way back.
    assert!(
        body.contains("x-amz-security-token: *******************"),
        "{body}"
    );
    assert!(!headers["x-seen-authorization"]
        .to_str()
        .unwrap()
        .contains("ZAPPER"));
    let ev = net_event(&t, &s, 0).await;
    assert!(ev["secrets"].to_string().contains("aws"), "{ev}");
}

/// The fake key signs nothing a rule doesn't list it for, and nowhere its
/// secret may not go.
#[tokio::test]
async fn the_fake_aws_key_signs_only_where_a_rule_lists_its_secret() {
    let (t, s) =
        aws_core("        - { action: allow, host: api.example, ports: [80], methods: [GET] }\n");
    let (status, _, body) = request(&t, &s, Method::GET, "/bucket/key", &fake_signed()).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(
        body.contains("no rule allowing this request lists"),
        "{body}"
    );
    // A request signed with keys of the guest's own is left as it is.
    let (t, s) = aws_core("        - { action: allow, host: api.example, ports: [80], methods: [GET], secrets: [aws] }\n");
    let own = vec![(
        "authorization",
        "AWS4-HMAC-SHA256 Credential=AKIAOWNKEY000000000/20991231/us-east-1/s3/aws4_request, SignedHeaders=host, Signature=abc".to_string(),
    )];
    let (status, _, body) = request(&t, &s, Method::GET, "/x", &own).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Credential=AKIAOWNKEY000000000/"), "{body}");
}

/// A rule broader than the secret's hosts doesn't widen where it signs: a
/// sibling host the rule allows is refused the real keys.
#[tokio::test]
async fn the_fake_aws_key_signs_only_for_the_secrets_hosts() {
    let (t, s) = aws_core("        - { action: allow, host: \"*.example\", ports: [80], methods: [GET], secrets: [aws] }\n");
    t.core
        .add_service(crate::ServiceSpec {
            name: "sibling.example".into(),
            port: 80,
            sessions: Default::default(),
            target: crate::ServiceTarget::Handler {
                tls: false,
                handler: Arc::new(HttpEcho),
            },
            owner: "test".into(),
        })
        .unwrap();
    let io = dial(&t, &s, "sibling.example", 80);
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(io))
        .await
        .unwrap();
    tokio::spawn(conn);
    let mut req = Request::builder()
        .method(Method::GET)
        .uri("http://sibling.example/x");
    for (k, v) in fake_signed() {
        req = req.header(k, v);
    }
    let resp = sender
        .send_request(req.body(Empty::<Bytes>::new()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let ev = net_event(&t, &s, 0).await;
    assert!(
        ev["error"]
            .as_str()
            .unwrap()
            .contains("may not sign for sibling.example"),
        "{ev}"
    );
}

/// A database rule wider than its `dbauth` secret's hosts doesn't widen where
/// the credential goes: to a host the rule allows but the secret isn't for,
/// it is refused before the secret is even read; to one of its hosts, it
/// gets past that (and here fails only to connect).
#[tokio::test]
async fn a_dbauth_secret_goes_only_to_its_hosts() {
    let t = TestCore::with_daemon(
        "private_networks: [10.0.0.0/8]",
        "secrets:\n  prod: { value: pw, mode: dbauth, hosts: [10.0.0.9], user: app }\nprofiles:\n  web:\n    policy:\n      net:\n        - { action: allow, host: \"10.*\", ports: [6379], protocol: redis, secrets: [prod] }\n",
    );
    let s = session(&t);
    let mut io = crate::testing::dial_addr(&t, &s, "10.1.2.3".parse().unwrap(), 6379);
    let _ = io.write_all(b"*1\r\n$4\r\nPING\r\n").await;
    let refused = net_event(&t, &s, 0).await["error"].to_string();
    assert!(
        refused.contains("secret prod isn't for 10.1.2.3"),
        "{refused}"
    );
    // The request log says so too: refused, not allowed.
    let logged = s.requests.lock().unwrap().list();
    assert_eq!(logged[0]["action"], "deny", "{logged:?}");

    // Within the secret's hosts: not refused for that. The relay goes on to
    // dial 10.0.0.9, which here may fail at once or hang, so what follows
    // isn't waited for: a refusal would be logged before any dial.
    let mut io = crate::testing::dial_addr(&t, &s, "10.0.0.9".parse().unwrap(), 6379);
    let _ = io.write_all(b"*1\r\n$4\r\nPING\r\n").await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let events = crate::testing::net_events(&t, &s);
    assert!(
        events[1..]
            .iter()
            .all(|e| !e["error"].to_string().contains("isn't for")),
        "{events:?}"
    );
}

/// A CONNECT to a name no rule could allow is refused before the name is
/// looked up on the host, even when a rule with a path makes every
/// connection inspected (so decided per request): else any name the guest
/// made up, data encoded in it, would reach the host's resolver, which the
/// DNS resolver refuses. A name a rule allows gets past it.
#[tokio::test]
async fn an_inspected_connect_to_a_name_nothing_allows_is_not_looked_up() {
    let t = TestCore::new(
        "profiles:\n  web:\n    policy:\n      net:\n        - { action: allow, host: ok.example }\n        - { action: deny, host: \"*\", path: \"/admin/**\" }\n      defaults: { net: deny }\n",
    );
    let s = session(&t);
    let connect = |host: &'static str| {
        let (d, s) = (t.core.daemon().clone(), s.clone());
        async move {
            let (mut guest, host_side) = tokio::net::UnixStream::pair().unwrap();
            tokio::spawn(super::serve(d, s, host_side));
            guest
                .write_all(
                    format!("CONNECT {host}:443 HTTP/1.1\r\nHost: {host}:443\r\n\r\n").as_bytes(),
                )
                .await
                .unwrap();
            let mut reply = vec![0u8; 512];
            let n = tokio::io::AsyncReadExt::read(&mut guest, &mut reply)
                .await
                .unwrap();
            String::from_utf8_lossy(&reply[..n]).into_owned()
        }
    };
    let refused = connect("c2VjcmV0.attacker.example").await;
    assert!(refused.starts_with("HTTP/1.1 403"), "{refused}");
    assert!(refused.contains("no rule allows this name"), "{refused}");
    // Allowed: past the check, to the host's resolver (which this test
    // doesn't have).
    let allowed = connect("ok.example").await;
    assert!(!allowed.contains("no rule allows"), "{allowed}");
}

/// A CONNECT tunnel holds a connection permit of its own for as long as it
/// runs: with none left, it is refused, not opened past the cap.
#[tokio::test]
async fn a_tunnel_needs_a_connection_permit() {
    let (t, s) = echo_core("        - { action: allow, host: api.example, ports: [80] }\n");
    let connect = || {
        let (d, s) = (t.core.daemon().clone(), s.clone());
        async move {
            let (mut guest, host_side) = tokio::net::UnixStream::pair().unwrap();
            tokio::spawn(super::serve(d, s, host_side));
            guest
                .write_all(b"CONNECT api.example:80 HTTP/1.1\r\nHost: api.example:80\r\n\r\n")
                .await
                .unwrap();
            let mut reply = vec![0u8; 512];
            let n = tokio::io::AsyncReadExt::read(&mut guest, &mut reply)
                .await
                .unwrap();
            (guest, String::from_utf8_lossy(&reply[..n]).into_owned())
        }
    };
    let conns = s.runtime.conns();
    let all = conns.available_permits() as u32;
    let held = conns.clone().try_acquire_many_owned(all).unwrap();
    let (_, refused) = connect().await;
    assert!(refused.starts_with("HTTP/1.1 503"), "{refused}");
    drop(held);
    let (_tunnel, opened) = connect().await;
    assert!(opened.starts_with("HTTP/1.1 200"), "{opened}");
    assert_eq!(conns.available_permits() as u32, all - 1, "the tunnel's");
}

/// A database rule relays only to a real address: a name the daemon
/// serves itself is refused, its credential never sent to a service.
#[tokio::test]
async fn a_database_rule_doesnt_relay_to_a_service() {
    let t = TestCore::new(
        "secrets:\n  db: { value: pw, mode: dbauth, hosts: [db.internal], user: app }\nprofiles:\n  web:\n    policy:\n      net:\n        - { action: allow, host: db.internal, ports: [6379], protocol: redis, secrets: [db] }\n",
    );
    let s = session(&t);
    t.core
        .add_service(crate::ServiceSpec {
            name: "db.internal".into(),
            port: 6379,
            sessions: Default::default(),
            target: crate::ServiceTarget::Handler {
                tls: false,
                handler: Arc::new(HttpEcho),
            },
            owner: "test".into(),
        })
        .unwrap();
    let mut io = dial(&t, &s, "db.internal", 6379);
    let _ = io.write_all(b"*1\r\n$4\r\nPING\r\n").await;
    let refused = net_event(&t, &s, 0).await["error"].to_string();
    assert!(refused.contains("is a service"), "{refused}");
}
