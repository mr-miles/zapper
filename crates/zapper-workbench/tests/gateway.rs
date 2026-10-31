//! The gateway against zapper's API, served on a socket of its own over a
//! `TestCore` (no VMs): who sees and does what, through tokens and the UI's
//! cookie.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::Extension;
use axum::routing::get;
use axum::{Json, Router};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use zapper_core::prelude::*;
use zapper_core::testing::TestCore;
use zapper_core::NewSession;
use zapper_workbench::facility::{Facilities, Facility, Panel};
use zapper_workbench::gateway::{self, Ctx};
use zapper_workbench::tokens::{Grant, Right, Tokens};
use zapper_workbench::zapper::Zapper;

const CONFIG: &str = "
policy:
  net:
    - { id: global-allow, action: allow, host: global.example }
profiles:
  web: {}
";

struct Bench {
    t: TestCore,
    addr: std::net::SocketAddr,
    tokens: Tokens,
    owner: String,
}

/// A facility for the tests: one route, saying who asked.
struct Hello;

impl Facility for Hello {
    fn name(&self) -> &'static str {
        "hello"
    }
    fn routes(&self) -> Router<Ctx> {
        Router::new().route(
            "/who",
            get(|Extension(g): Extension<Grant>| async move { Json(json!({ "token": g.name })) }),
        )
    }
    fn panel(&self) -> Option<Panel> {
        Some(Panel {
            title: "Hello",
            script: "document.querySelector('[data-facility=\"hello\"]').textContent = 'hi';",
        })
    }
}

async fn bench() -> Bench {
    let t = TestCore::new(CONFIG);
    let api = zapper_api::api(t.core.clone(), &zapper_api::Mount::default());
    bench_on(t, api).await
}

/// A zapper that boots nothing: a session asked for is answered as made,
/// with what was asked.
fn fake_zapper() -> Router {
    Router::new().route(
        "/v1/sessions",
        axum::routing::post(|Json(asked): Json<Value>| async move {
            Json(json!({ "id": "made1", "labels": asked["labels"], "workdir": asked["workdir"], "asked": asked }))
        }),
    )
}

async fn bench_on(t: TestCore, api: Router) -> Bench {
    let socket = t.dir.join("api.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    tokio::spawn(async move { axum::serve(listener, api).await });
    let tokens = Tokens::open(t.dir.join("wb")).unwrap();
    let work = t.dir.join("work");
    std::fs::create_dir_all(work.join("proj")).unwrap();
    let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = tcp.local_addr().unwrap();
    let ctx = Ctx {
        zapper: Zapper::new(socket),
        tokens: tokens.clone(),
        facilities: Arc::new(Facilities::new(vec![Arc::new(Hello)]).unwrap()),
        work,
        port: addr.port(),
        remote: false,
    };
    tokio::spawn(async move { axum::serve(tcp, gateway::app(ctx)).await });
    let owner = tokens.owner().unwrap();
    Bench {
        t,
        addr,
        tokens,
        owner,
    }
}

struct Got {
    status: StatusCode,
    headers: hyper::HeaderMap,
    body: Value,
}

async fn send(
    b: &Bench,
    method: Method,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<Value>,
) -> Got {
    let stream = tokio::net::TcpStream::connect(b.addr).await.unwrap();
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(conn);
    let mut req = Request::builder()
        .method(method)
        .uri(path)
        .header("host", format!("127.0.0.1:{}", b.addr.port()));
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let body = match body {
        Some(v) => {
            req = req.header("content-type", "application/json");
            Bytes::from(v.to_string())
        }
        None => Bytes::new(),
    };
    let resp = sender
        .send_request(req.body(Full::new(body)).unwrap())
        .await
        .unwrap();
    let (status, headers) = (resp.status(), resp.headers().clone());
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into()));
    Got {
        status,
        headers,
        body,
    }
}

/// `method path` with `token` as automation sends it.
async fn as_token(b: &Bench, token: &str, method: Method, path: &str, body: Option<Value>) -> Got {
    let auth = format!("Bearer {token}");
    send(b, method, path, &[("authorization", &auth)], body).await
}

fn session(b: &Bench, labels: &[(&str, &str)]) -> String {
    let mut new = NewSession::plain("web".into(), b.t.workdir(), Default::default());
    new.labels = labels
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    b.t.core.test_session(new, true).unwrap().id.clone()
}

fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn ids(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_token_sees_only_the_sessions_with_its_labels() {
    let b = bench().await;
    let x = session(&b, &[("project", "x")]);
    let y = session(&b, &[("project", "y")]);
    let token = b
        .tokens
        .add("laptop", labels(&[("project", "x")]), [Right::Read].into())
        .unwrap();

    let all = as_token(&b, &b.owner, Method::GET, "/w/v1/sessions", None).await;
    let mut both = ids(&all.body);
    both.sort();
    let mut want = vec![x.clone(), y.clone()];
    want.sort();
    assert_eq!(both, want);

    let mine = as_token(&b, &token, Method::GET, "/w/v1/sessions", None).await;
    assert_eq!(ids(&mine.body), std::slice::from_ref(&x));
    let got = as_token(
        &b,
        &token,
        Method::GET,
        &format!("/w/v1/sessions/{x}"),
        None,
    )
    .await;
    assert_eq!(got.status, StatusCode::OK);
    // Another's is answered as one that isn't there.
    let other = as_token(
        &b,
        &token,
        Method::GET,
        &format!("/w/v1/sessions/{y}"),
        None,
    )
    .await;
    assert_eq!(other.status, StatusCode::NOT_FOUND, "{:?}", other.body);
    let audit = as_token(
        &b,
        &token,
        Method::GET,
        &format!("/w/v1/sessions/{y}/audit"),
        None,
    )
    .await;
    assert_eq!(audit.status, StatusCode::NOT_FOUND);

    let status = as_token(&b, &token, Method::GET, "/w/v1/status", None).await;
    assert_eq!(status.body["token"]["name"], "laptop");
    assert_eq!(status.body["token"]["rights"], json!(["read"]));

    for bad in ["", "nope"] {
        let r = as_token(&b, bad, Method::GET, "/w/v1/sessions", None).await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED, "{bad:?}");
    }
    b.tokens.remove("laptop").unwrap();
    let gone = as_token(&b, &token, Method::GET, "/w/v1/sessions", None).await;
    assert_eq!(
        gone.status,
        StatusCode::UNAUTHORIZED,
        "removed, refused at once"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_it_creates_has_the_tokens_labels_and_one_of_its_directories() {
    let b = bench_on(TestCore::new(CONFIG), fake_zapper()).await;
    let token = b
        .tokens
        .add(
            "ci",
            labels(&[("project", "x")]),
            [Right::Read, Right::Create].into(),
        )
        .unwrap();
    let made = as_token(
        &b,
        &token,
        Method::POST,
        "/w/v1/sessions",
        Some(json!({ "profile": "web", "workdir": "proj" })),
    )
    .await;
    assert_eq!(made.status, StatusCode::OK, "{:?}", made.body);
    assert_eq!(made.body["labels"]["project"], "x");
    assert_eq!(made.body["labels"]["workbench"], "1");
    assert!(
        made.body["workdir"]
            .as_str()
            .unwrap()
            .ends_with("/work/proj"),
        "{:?}",
        made.body
    );

    let fresh = as_token(
        &b,
        &token,
        Method::POST,
        "/w/v1/sessions",
        Some(json!({ "profile": "web" })),
    )
    .await;
    assert!(
        fresh.body["workdir"]
            .as_str()
            .unwrap()
            .contains("/work/new-"),
        "{:?}",
        fresh.body
    );

    for (body, why) in [
        (
            json!({ "profile": "web", "labels": { "project": "y" } }),
            "another project's label",
        ),
        (
            json!({ "profile": "web", "workdir": "../.." }),
            "a path out of work/",
        ),
        (
            json!({ "profile": "web", "workdir": "/etc" }),
            "a host path",
        ),
        (
            json!({ "profile": "web", "system": true }),
            "a field it doesn't take",
        ),
    ] {
        let r = as_token(&b, &token, Method::POST, "/w/v1/sessions", Some(body)).await;
        assert!(
            r.status.is_client_error(),
            "{why}: {} {:?}",
            r.status,
            r.body
        );
    }
    let rules = as_token(
        &b,
        &token,
        Method::POST,
        "/w/v1/sessions",
        Some(json!({ "profile": "web", "rules": [{ "kind": "net", "action": "allow", "host": "x.example" }] })),
    )
    .await;
    assert_eq!(
        rules.status,
        StatusCode::FORBIDDEN,
        "rules need the rules right"
    );

    let reader = b
        .tokens
        .add("reader", BTreeMap::new(), [Right::Read].into())
        .unwrap();
    let r = as_token(
        &b,
        &reader,
        Method::POST,
        "/w/v1/sessions",
        Some(json!({ "profile": "web" })),
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
}

#[tokio::test(flavor = "multi_thread")]
async fn rules_go_to_the_sessions_own_layer_only() {
    let b = bench().await;
    let x = session(&b, &[("project", "x")]);
    let token = b
        .tokens
        .add(
            "dev",
            labels(&[("project", "x")]),
            [Right::Read, Right::Rules].into(),
        )
        .unwrap();
    let rule = json!({ "rule": { "kind": "net", "action": "allow", "host": "api.example" } });
    let added = as_token(
        &b,
        &token,
        Method::POST,
        &format!("/w/v1/sessions/{x}/rules"),
        Some(rule),
    )
    .await;
    assert_eq!(added.status, StatusCode::OK, "{:?}", added.body);
    let id = added.body["id"].as_str().unwrap().to_string();
    let listed = as_token(
        &b,
        &token,
        Method::GET,
        &format!("/w/v1/sessions/{x}/rules"),
        None,
    )
    .await;
    assert!(ids(&listed.body).contains(&id));

    // A global rule, named through the session's route, stays.
    let r = as_token(
        &b,
        &token,
        Method::DELETE,
        &format!("/w/v1/sessions/{x}/rules/global-allow"),
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND, "{:?}", r.body);
    assert!(b
        .t
        .core
        .config()
        .policy
        .rules()
        .iter()
        .any(|r| r.id() == Some("global-allow")));
    let r = as_token(
        &b,
        &token,
        Method::DELETE,
        &format!("/w/v1/sessions/{x}/rules/{id}"),
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_terminal_is_never_a_system_one_and_names_the_token() {
    let b = bench().await;
    let x = session(&b, &[]);
    let s = b.t.core.session(&x).unwrap();
    let mut vm = zapper_core::testing::shell_vm(&s);
    let token = b
        .tokens
        .add(
            "pad",
            BTreeMap::new(),
            [Right::Read, Right::Terminal].into(),
        )
        .unwrap();
    let auth = format!("Bearer {token}");

    let refused = send(
        &b,
        Method::GET,
        &format!("/w/v1/sessions/{x}/attach?system=true"),
        &[
            ("authorization", &auth),
            ("connection", "upgrade"),
            ("upgrade", "zapper-shell"),
        ],
        None,
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::BAD_REQUEST,
        "{:?}",
        refused.body
    );

    let opened = send(
        &b,
        Method::GET,
        &format!("/w/v1/sessions/{x}/attach?argv=%5B%22id%22%5D"),
        &[
            ("authorization", &auth),
            ("connection", "upgrade"),
            ("upgrade", "zapper-shell"),
        ],
        None,
    )
    .await;
    assert_eq!(
        opened.status,
        StatusCode::SWITCHING_PROTOCOLS,
        "{:?}",
        opened.body
    );
    assert!(opened.headers.contains_key("x-zapper-terminal"));
    let guest = tokio::time::timeout(Duration::from_secs(5), vm.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(guest.request.argv, ["id"]);
    assert!(!guest.request.system);
    // The terminal's event follows its opening by a moment.
    let mut shell = None;
    for _ in 0..200 {
        shell =
            b.t.core
                .recent_events(Some(&x))
                .into_iter()
                .map(|e| serde_json::to_value(e).unwrap())
                .find(|e| e["type"] == "shell");
        if shell.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let shell = shell.expect("a shell event");
    assert_eq!(shell["client"], "workbench:pad");

    let reader = b
        .tokens
        .add("reader", BTreeMap::new(), [Right::Read].into())
        .unwrap();
    let r = as_token(
        &b,
        &reader,
        Method::GET,
        &format!("/w/v1/sessions/{x}/attach"),
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
}

/// The events a stream of `path` sends within a moment, as JSON.
async fn events(b: &Bench, token: &str, path: &str) -> Vec<Value> {
    let stream = tokio::net::TcpStream::connect(b.addr).await.unwrap();
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(conn);
    let req = Request::builder()
        .uri(path)
        .header("host", format!("127.0.0.1:{}", b.addr.port()))
        .header("authorization", format!("Bearer {token}"))
        .body(Full::new(Bytes::new()))
        .unwrap();
    let resp = sender.send_request(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let mut body = resp.into_body();
    let mut text = String::new();
    let _ = tokio::time::timeout(Duration::from_millis(500), async {
        while let Some(Ok(f)) = body.frame().await {
            if let Ok(d) = f.into_data() {
                text.push_str(&String::from_utf8_lossy(&d));
            }
        }
    })
    .await;
    text.lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .filter_map(|d| serde_json::from_str(d.trim()).ok())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn events_are_those_of_the_tokens_sessions() {
    let b = bench().await;
    let x = session(&b, &[("project", "x")]);
    let y = session(&b, &[("project", "y")]);
    for id in [&x, &y] {
        b.t.core.session(id).unwrap().set_ssh(true).unwrap();
    }
    let token = b
        .tokens
        .add("laptop", labels(&[("project", "x")]), [Right::Read].into())
        .unwrap();
    let seen = events(&b, &token, "/w/v1/events?history=true").await;
    let sessions: std::collections::BTreeSet<_> =
        seen.iter().filter_map(|e| e["session"].as_str()).collect();
    assert_eq!(sessions, [x.as_str()].into(), "{seen:?}");
    assert!(
        seen.iter()
            .all(|e| e["session"].is_string() || e["type"] == "gap"),
        "no session's events: {seen:?}"
    );

    let all = events(&b, &b.owner, "/w/v1/events?history=true").await;
    let sessions: std::collections::BTreeSet<_> =
        all.iter().filter_map(|e| e["session"].as_str()).collect();
    assert_eq!(sessions, [x.as_str(), y.as_str()].into());

    let r = as_token(
        &b,
        &token,
        Method::GET,
        &format!("/w/v1/events?session={y}"),
        None,
    )
    .await;
    assert_eq!(
        r.status,
        StatusCode::NOT_FOUND,
        "another's stream is refused"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_approval_is_answered_only_for_the_tokens_sessions() {
    let b = bench().await;
    let token = b
        .tokens
        .add("ops", BTreeMap::new(), [Right::Read].into())
        .unwrap();
    let r = as_token(
        &b,
        &token,
        Method::POST,
        "/w/v1/approvals/a-1",
        Some(json!({ "allow": true })),
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "answering needs its right");
    let r = as_token(
        &b,
        &b.owner,
        Method::POST,
        "/w/v1/approvals/a-1",
        Some(json!({ "allow": true })),
    )
    .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND, "{:?}", r.body);
    let list = as_token(&b, &token, Method::GET, "/w/v1/approvals", None).await;
    assert_eq!(list.body, json!([]));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_ui_signs_in_at_its_login_page_and_writes_need_its_header_and_origin() {
    let b = bench().await;
    let origin = format!("http://127.0.0.1:{}", b.addr.port());
    let ui = [("x-workbench-ui", "1"), ("origin", origin.as_str())];

    // Not signed in: the page sends you to sign in; the API says so.
    let page = send(&b, Method::GET, "/", &[], None).await;
    assert_eq!(page.status, StatusCode::SEE_OTHER);
    assert_eq!(page.headers["location"], "/login");
    let none = send(&b, Method::GET, "/w/v1/sessions", &[], None).await;
    assert_eq!(none.status, StatusCode::UNAUTHORIZED);
    let login = send(&b, Method::GET, "/login", &[], None).await;
    assert_eq!(login.status, StatusCode::OK);
    assert!(login.body.as_str().unwrap().contains("type=\"password\""));

    // A token in the address is no way in, and sets nothing.
    let addressed = send(&b, Method::GET, &format!("/?token={}", b.owner), &[], None).await;
    assert_eq!(addressed.status, StatusCode::SEE_OTHER);
    assert_eq!(addressed.headers["location"], "/login");
    assert!(!addressed.headers.contains_key("set-cookie"));

    // Signing in is a write from the UI, the token in its body.
    let token = Some(json!({ "token": b.owner }));
    let bare = send(&b, Method::POST, "/login", &[], token.clone()).await;
    assert_eq!(
        bare.status,
        StatusCode::FORBIDDEN,
        "without the header and Origin"
    );
    let wrong = send(
        &b,
        Method::POST,
        "/login",
        &ui,
        Some(json!({ "token": "nope" })),
    )
    .await;
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
    assert!(!wrong.headers.contains_key("set-cookie"));
    let r = send(&b, Method::POST, "/login", &ui, token).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    assert_eq!(r.body["token"], "owner");
    let cookie = r.headers["set-cookie"].to_str().unwrap().to_string();
    assert!(
        cookie.contains("HttpOnly") && cookie.contains("SameSite=Strict"),
        "{cookie}"
    );
    let cookie = cookie.split(';').next().unwrap().to_string();

    let page = send(&b, Method::GET, "/", &[("cookie", &cookie)], None).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.headers.contains_key("content-security-policy"));
    assert_eq!(page.headers["cache-control"], "no-store");

    // Signing out clears it.
    let out = send(&b, Method::POST, "/logout", &ui, None).await;
    assert!(out.headers["set-cookie"]
        .to_str()
        .unwrap()
        .contains("Max-Age=0"));

    // DNS rebinding: another name for this address.
    let stream = tokio::net::TcpStream::connect(b.addr).await.unwrap();
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(conn);
    let req = Request::builder()
        .uri("/w/v1/sessions")
        .header("host", "evil.example")
        .header("cookie", &cookie)
        .body(Full::new(Bytes::new()))
        .unwrap();
    assert_eq!(
        sender.send_request(req).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );

    let origin = format!("http://127.0.0.1:{}", b.addr.port());
    let x = session(&b, &[]);
    let rules = format!("/w/v1/sessions/{x}/rules");
    let body = Some(json!({ "rule": { "kind": "net", "action": "allow", "host": "ui.example" } }));
    let bare = send(
        &b,
        Method::POST,
        &rules,
        &[("cookie", &cookie), ("origin", &origin)],
        body.clone(),
    )
    .await;
    assert_eq!(bare.status, StatusCode::FORBIDDEN, "without the header");
    let foreign = send(
        &b,
        Method::POST,
        &rules,
        &[
            ("cookie", &cookie),
            ("x-workbench-ui", "1"),
            ("origin", "http://evil.example"),
        ],
        body.clone(),
    )
    .await;
    assert_eq!(
        foreign.status,
        StatusCode::FORBIDDEN,
        "another site's Origin"
    );
    let ok = send(
        &b,
        Method::POST,
        &rules,
        &[
            ("cookie", &cookie),
            ("x-workbench-ui", "1"),
            ("origin", &origin),
        ],
        body,
    )
    .await;
    assert_eq!(ok.status, StatusCode::OK, "{:?}", ok.body);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_facility_has_its_routes_behind_the_gateway_and_its_panel() {
    let b = bench().await;
    let token = b
        .tokens
        .add("t", BTreeMap::new(), [Right::Read].into())
        .unwrap();
    let r = as_token(&b, &token, Method::GET, "/w/v1/hello/who", None).await;
    assert_eq!(r.body, json!({ "token": "t" }));
    let r = send(&b, Method::GET, "/w/v1/hello/who", &[], None).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    let r = as_token(&b, &token, Method::GET, "/w/v1/facilities", None).await;
    assert_eq!(r.body[0]["name"], "hello");
    assert_eq!(r.body[0]["title"], "Hello");
    assert_eq!(r.body[0]["panel"], "/panels/hello.js");
    let auth = format!("Bearer {token}");
    let js = send(
        &b,
        Method::GET,
        "/panels/hello.js",
        &[("authorization", &auth)],
        None,
    )
    .await;
    assert_eq!(js.status, StatusCode::OK);
    assert!(js.headers["content-type"]
        .to_str()
        .unwrap()
        .starts_with("text/javascript"));
    let none = send(
        &b,
        Method::GET,
        "/panels/nope.js",
        &[("authorization", &auth)],
        None,
    )
    .await;
    assert_eq!(none.status, StatusCode::NOT_FOUND);

    struct Named(&'static str);
    impl Facility for Named {
        fn name(&self) -> &'static str {
            self.0
        }
    }
    for bad in ["sessions", "Hello", "", "a/b"] {
        assert!(
            Facilities::new(vec![Arc::new(Named(bad))]).is_err(),
            "{bad}"
        );
    }
    assert!(Facilities::new(vec![Arc::new(Named("x")), Arc::new(Named("x"))]).is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn with_zapper_gone_a_client_is_still_signed_in() {
    let t = TestCore::new(CONFIG);
    // A socket nothing listens on: zapper stopped.
    let tokens = Tokens::open(t.dir.join("wb")).unwrap();
    let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = tcp.local_addr().unwrap();
    let ctx = Ctx {
        zapper: Zapper::new(t.dir.join("gone.sock")),
        tokens: tokens.clone(),
        facilities: Arc::new(Facilities::default()),
        work: t.dir.join("work"),
        port: addr.port(),
        remote: false,
    };
    tokio::spawn(async move { axum::serve(tcp, gateway::app(ctx)).await });
    let owner = tokens.owner().unwrap();
    let b = Bench {
        t,
        addr,
        tokens,
        owner,
    };
    let s = as_token(&b, &b.owner, Method::GET, "/w/v1/status", None).await;
    assert_eq!(s.status, StatusCode::OK, "{:?}", s.body);
    assert_eq!(s.body["token"]["name"], "owner");
    assert!(s.body["zapper"]["error"].is_string(), "{:?}", s.body);
    let l = as_token(&b, &b.owner, Method::GET, "/w/v1/sessions", None).await;
    assert_eq!(
        l.status,
        StatusCode::BAD_GATEWAY,
        "zapper unreachable, not signed out"
    );
}
