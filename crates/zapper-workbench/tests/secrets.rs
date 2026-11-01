//! The secrets facility against zapper's API, served on a socket of its
//! own over a `TestCore` (no VMs): values kept on disk, entries in zapper's
//! running config, and who may do what.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt as _;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use zapper_core::prelude::*;
use zapper_core::testing::TestCore;
use zapper_core::NewSession;
use zapper_workbench::facility::Facilities;
use zapper_workbench::gateway::{self, Ctx};
use zapper_workbench::secrets::Secrets;
use zapper_workbench::tokens::{Right, Tokens};
use zapper_workbench::zapper::Zapper;

/// A config with a secret of its own, written by hand.
const CONFIG: &str = "
secrets:
  handmade: { file: /etc/handmade.token, hosts: [hand.example] }
profiles:
  web: {}
";

struct Bench {
    t: TestCore,
    addr: std::net::SocketAddr,
    tokens: Tokens,
    owner: String,
    dir: std::path::PathBuf,
}

async fn bench() -> Bench {
    let t = TestCore::new(CONFIG);
    let socket = t.dir.join("api.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let api = zapper_api::api(t.core.clone(), &zapper_api::Mount::default());
    tokio::spawn(async move { axum::serve(listener, api).await });
    let tokens = Tokens::open(t.dir.join("wb")).unwrap();
    let dir = t.dir.join("wb").join("secrets");
    let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = tcp.local_addr().unwrap();
    let ctx = Ctx {
        zapper: Zapper::new(socket),
        tokens: tokens.clone(),
        facilities: Arc::new(
            Facilities::new(vec![Arc::new(Secrets::new(dir.clone()).unwrap())]).unwrap(),
        ),
        work: t.dir.join("work"),
        port: addr.port(),
        remote: false,
        live: Default::default(),
    };
    tokio::spawn(async move { axum::serve(tcp, gateway::app(ctx)).await });
    let owner = tokens.owner().unwrap();
    Bench {
        t,
        addr,
        tokens,
        owner,
        dir,
    }
}

async fn call(
    b: &Bench,
    token: &str,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let stream = tokio::net::TcpStream::connect(b.addr).await.unwrap();
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(conn);
    let mut req = Request::builder()
        .method(method)
        .uri(path)
        .header("host", format!("127.0.0.1:{}", b.addr.port()))
        .header("authorization", format!("Bearer {token}"));
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
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

const CLAUDE: &str = "sk-ant-oat01-not-a-real-token";

#[tokio::test(flavor = "multi_thread")]
async fn a_secret_is_a_private_file_and_an_entry_that_names_it() {
    let b = bench().await;
    let (s, v) = call(
        &b,
        &b.owner,
        Method::PUT,
        "/w/v1/secrets/claude",
        Some(json!({ "preset": "claude", "value": format!("{CLAUDE}\n") })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let file = b.dir.join("claude");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), CLAUDE, "trimmed");
    assert_eq!(
        std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(&b.dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let entry = &b.t.core.config().secrets["claude"];
    assert_eq!(entry.hosts, ["api.anthropic.com"]);
    assert_eq!(
        serde_json::to_value(entry).unwrap()["file"],
        file.display().to_string()
    );

    let (_, listed) = call(&b, &b.owner, Method::GET, "/w/v1/secrets", None).await;
    assert!(
        !listed.to_string().contains(CLAUDE),
        "never shown back: {listed}"
    );
    let mine = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "claude")
        .unwrap();
    assert_eq!(
        (mine["workbench"].as_bool(), mine["has_value"].as_bool()),
        (Some(true), Some(true))
    );
    let theirs = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "handmade")
        .unwrap();
    assert_eq!(theirs["workbench"], false);

    // The value replaced alone: the entry as it was.
    let (s, _) = call(
        &b,
        &b.owner,
        Method::PUT,
        "/w/v1/secrets/claude",
        Some(json!({ "value": "sk-ant-oat01-newer" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "sk-ant-oat01-newer"
    );
    assert_eq!(
        b.t.core.config().secrets["claude"].hosts,
        ["api.anthropic.com"]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_secret_leaves_nothing_behind() {
    let b = bench().await;
    // Not what the preset's credential looks like.
    let (s, v) = call(
        &b,
        &b.owner,
        Method::PUT,
        "/w/v1/secrets/claude",
        Some(json!({ "preset": "claude", "value": "ghp_wrong" })),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    // zapper refuses it: a proxy secret with no hosts.
    let (s, v) = call(
        &b,
        &b.owner,
        Method::PUT,
        "/w/v1/secrets/bare",
        Some(json!({ "value": "x" })),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert!(v["error"].as_str().unwrap().contains("needs hosts"), "{v}");
    for n in ["claude", "bare"] {
        assert!(!b.t.core.config().secrets.contains_key(n), "{n}");
        assert!(!b.dir.join(n).exists(), "{n}");
    }
    assert_eq!(
        std::fs::read_dir(&b.dir).unwrap().count(),
        0,
        "no staged file left"
    );
    let (s, _) = call(
        &b,
        &b.owner,
        Method::PUT,
        "/w/v1/secrets/new",
        Some(json!({ "hosts": ["x.example"] })),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "a new secret needs a value");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_secret_the_config_holds_stays_the_operators() {
    let b = bench().await;
    let (s, _) = call(
        &b,
        &b.owner,
        Method::PUT,
        "/w/v1/secrets/handmade",
        Some(json!({ "value": "x", "hosts": ["evil.example"] })),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
    let (s, _) = call(&b, &b.owner, Method::DELETE, "/w/v1/secrets/handmade", None).await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(
        b.t.core.config().secrets["handmade"].hosts,
        ["hand.example"]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_secret_in_use_stays_and_one_unused_goes_with_its_value() {
    let b = bench().await;
    call(
        &b,
        &b.owner,
        Method::PUT,
        "/w/v1/secrets/gh",
        Some(json!({ "preset": "github", "value": "github_pat_x" })),
    )
    .await;
    let id = {
        let new = NewSession::plain("web".into(), b.t.workdir(), Default::default());
        b.t.core.test_session(new, true).unwrap().id.clone()
    };
    let rule = json!({ "rule": { "kind": "net", "action": "allow", "host": "api.github.com", "secrets": ["gh"] } });
    let (s, v) = call(
        &b,
        &b.owner,
        Method::POST,
        &format!("/w/v1/sessions/{id}/rules"),
        Some(rule),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let rule_id = v["id"].as_str().unwrap().to_string();
    let (_, listed) = call(&b, &b.owner, Method::GET, "/w/v1/secrets", None).await;
    let used = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "gh")
        .unwrap()["used_by"]
        .clone();
    assert!(used.to_string().contains(&rule_id), "{used}");

    let (s, v) = call(&b, &b.owner, Method::DELETE, "/w/v1/secrets/gh", None).await;
    assert_eq!(s, StatusCode::CONFLICT, "in use by a session's rule: {v}");
    assert!(b.dir.join("gh").exists(), "its value stays with it");
    call(
        &b,
        &b.owner,
        Method::DELETE,
        &format!("/w/v1/sessions/{id}/rules/{rule_id}"),
        None,
    )
    .await;
    let (s, v) = call(&b, &b.owner, Method::DELETE, "/w/v1/secrets/gh", None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert!(!b.dir.join("gh").exists());
    assert!(!b.t.core.config().secrets.contains_key("gh"));
}

#[tokio::test(flavor = "multi_thread")]
async fn secrets_and_rules_that_use_them_need_the_secrets_right() {
    let b = bench().await;
    call(
        &b,
        &b.owner,
        Method::PUT,
        "/w/v1/secrets/gh",
        Some(json!({ "preset": "github", "value": "github_pat_x" })),
    )
    .await;
    let id = {
        let new = NewSession::plain("web".into(), b.t.workdir(), Default::default());
        b.t.core.test_session(new, true).unwrap().id.clone()
    };
    let dev = b
        .tokens
        .add("dev", BTreeMap::new(), [Right::Read, Right::Rules].into())
        .unwrap();
    let (s, _) = call(&b, &dev, Method::GET, "/w/v1/secrets", None).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = call(
        &b,
        &dev,
        Method::PUT,
        "/w/v1/secrets/x",
        Some(json!({ "preset": "github", "value": "github_pat_y" })),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    // A rule that would have the session use the owner's token.
    let using = json!({ "rule": { "kind": "net", "action": "allow", "host": "api.github.com", "secrets": ["gh"] } });
    let (s, _) = call(
        &b,
        &dev,
        Method::POST,
        &format!("/w/v1/sessions/{id}/rules"),
        Some(using),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let plain = json!({ "rule": { "kind": "net", "action": "allow", "host": "api.github.com" } });
    let (s, _) = call(
        &b,
        &dev,
        Method::POST,
        &format!("/w/v1/sessions/{id}/rules"),
        Some(plain),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "a rule without one is the rules right's");

    // Saving zapper's whole config is the owner's alone.
    let (s, _) = call(&b, &dev, Method::POST, "/w/v1/config/save", None).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, d) = call(&b, &b.owner, Method::GET, "/w/v1/config/diff", None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(d["diff"].to_string().contains("gh"), "{d}");
    let (s, v) = call(&b, &b.owner, Method::POST, "/w/v1/config/save", None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let saved = std::fs::read_to_string(b.t.startup()).unwrap();
    assert!(
        saved.contains("api.github.com") && !saved.contains("github_pat_x"),
        "{saved}"
    );
}
