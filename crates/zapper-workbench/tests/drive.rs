//! The drive, end to end over a `TestCore` (no VMs): a session's request to
//! drive.zapper:443 through zapper's proxy (TLS with the session's CA, net
//! rules with paths), the caller line, the drive's socket and its files;
//! and the user's side through the gateway.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::Value;
use zapper_core::testing::{dial, guest_tls, TestCore};
use zapper_core::{NewSession, Session};
use zapper_workbench::drive::Drive;
use zapper_workbench::facility::{self, Facilities};
use zapper_workbench::gateway::{self, Ctx};
use zapper_workbench::tokens::{Right, Tokens};
use zapper_workbench::zapper::Zapper;

const CONFIG: &str = "profiles:\n  web: {}\n";

struct Bench {
    t: TestCore,
    addr: std::net::SocketAddr,
    tokens: Tokens,
    owner: String,
    root: std::path::PathBuf,
}

async fn bench() -> Bench {
    let t = TestCore::new(CONFIG);
    let socket = t.dir.join("api.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let api = zapper_api::api(t.core.clone(), &zapper_api::Mount::default());
    tokio::spawn(async move { axum::serve(listener, api).await });
    let tokens = Tokens::open(t.dir.join("wb")).unwrap();
    let root = t.dir.join("drive");
    let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = tcp.local_addr().unwrap();
    let ctx = Ctx {
        zapper: Zapper::new(socket),
        tokens: tokens.clone(),
        facilities: Arc::new(
            Facilities::new(vec![Arc::new(Drive::new(root.clone()).unwrap())]).unwrap(),
        ),
        work: t.dir.join("work"),
        port: addr.port(),
        remote: false,
        live: Default::default(),
    };
    for f in ctx.facilities.iter() {
        f.start(&ctx);
    }
    facility::register(&ctx).await.unwrap();
    // Its socket, once zapper said where.
    let sock = t.core.socket_dir().join(zapper_workbench::drive::SOCKET);
    for _ in 0..200 {
        if sock.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::spawn(async move { axum::serve(tcp, gateway::app(ctx)).await });
    let owner = tokens.owner().unwrap();
    Bench {
        t,
        addr,
        tokens,
        owner,
        root,
    }
}

/// `method path` as a session asks it of drive.zapper: its status and body.
async fn from_session(
    b: &Bench,
    s: &Arc<Session>,
    method: &str,
    path: &str,
    body: &str,
) -> (u16, String) {
    let io = dial(&b.t, s, zapper_workbench::drive::NAME, 443);
    let name = rustls::pki_types::ServerName::try_from(zapper_workbench::drive::NAME).unwrap();
    let tls = guest_tls(s).connect(name, io).await.unwrap();
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
        .await
        .unwrap();
    tokio::spawn(conn);
    let req = Request::builder()
        .method(method)
        .uri(path)
        .header("host", zapper_workbench::drive::NAME)
        .body(Full::new(Bytes::from(body.to_string())))
        .unwrap();
    let r = sender.send_request(req).await.unwrap();
    let status = r.status().as_u16();
    let ct = r
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default();
    let text =
        String::from_utf8_lossy(&r.into_body().collect().await.unwrap().to_bytes()).into_owned();
    (status, format!("{ct}\n{text}"))
}

/// `method path` through the gateway, as `token`.
async fn as_user(
    b: &Bench,
    token: &str,
    method: Method,
    path: &str,
    body: &str,
) -> (StatusCode, hyper::HeaderMap, Bytes) {
    let stream = tokio::net::TcpStream::connect(b.addr).await.unwrap();
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(conn);
    let req = Request::builder()
        .method(method)
        .uri(path)
        .header("host", format!("127.0.0.1:{}", b.addr.port()))
        .header("authorization", format!("Bearer {token}"))
        .body(Full::new(Bytes::from(body.to_string())))
        .unwrap();
    let r = sender.send_request(req).await.unwrap();
    let (status, headers) = (r.status(), r.headers().clone());
    (
        status,
        headers,
        r.into_body().collect().await.unwrap().to_bytes(),
    )
}

fn workbench_session(b: &Bench) -> Arc<Session> {
    let mut new = NewSession::plain("web".into(), b.t.workdir(), Default::default());
    new.labels = [("workbench".to_string(), "1".to_string())].into();
    b.t.core.test_session(new, true).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_reads_the_inbox_and_writes_its_own_outbox_only() {
    let b = bench().await;
    let s = workbench_session(&b);
    let (st, _, body) = as_user(&b, &b.owner, Method::POST, "/w/v1/drive/rules", "").await;
    assert_eq!(st, StatusCode::OK, "{}", String::from_utf8_lossy(&body));

    // The user puts a brief in the inbox.
    let (st, _, _) = as_user(
        &b,
        &b.owner,
        Method::PUT,
        "/w/v1/drive/files/inbox/brief.md",
        "# do this",
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    let (st, text) = from_session(&b, &s, "GET", "/inbox/brief.md", "").await;
    assert_eq!(st, 200, "{text}");
    assert_eq!(text, "text/plain; charset=utf-8\n# do this");
    let (st, text) = from_session(&b, &s, "GET", "/inbox/", "").await;
    assert_eq!(st, 200, "{text}");
    assert!(text.contains("\"brief.md\""), "{text}");

    // Its own outbox: written, read back, on the host.
    let mine = format!("/outbox/{}/report.html", s.id);
    let (st, text) = from_session(&b, &s, "PUT", &mine, "<script>x</script>").await;
    assert_eq!(st, 201, "{text}");
    assert_eq!(
        std::fs::read_to_string(b.root.join(format!("outbox/{}/report.html", s.id))).unwrap(),
        "<script>x</script>"
    );
    let (st, text) = from_session(&b, &s, "GET", &mine, "").await;
    assert_eq!(
        (st, text.lines().next().unwrap()),
        (200, "text/plain; charset=utf-8"),
        "HTML as text"
    );

    // What the rules don't allow, zapper refuses before the drive sees it.
    for (m, p) in [
        ("PUT", "/inbox/x".to_string()),
        ("PUT", "/outbox/someone-else/x".to_string()),
        ("GET", "/elsewhere".to_string()),
    ] {
        let (st, text) = from_session(&b, &s, m, &p, "no").await;
        assert_eq!(st, 403, "{m} {p}: {text}");
    }
    assert!(!b.root.join("inbox/x").exists());
    assert!(!b.root.join("outbox/someone-else").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_user_reaches_the_drive_and_a_download_never_runs_in_the_page() {
    let b = bench().await;
    std::fs::create_dir_all(b.root.join("outbox/s1")).unwrap();
    std::fs::write(
        b.root.join("outbox/s1/page.html"),
        "<script>steal()</script>",
    )
    .unwrap();
    let (st, h, body) = as_user(
        &b,
        &b.owner,
        Method::GET,
        "/w/v1/drive/files/outbox/s1/page.html",
        "",
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(h["content-type"], "application/octet-stream");
    assert_eq!(h["content-disposition"], "attachment");
    assert_eq!(h["content-security-policy"], "sandbox");
    assert_eq!(&body[..], b"<script>steal()</script>");

    let (st, _, body) = as_user(&b, &b.owner, Method::GET, "/w/v1/drive/files", "").await;
    assert_eq!(st, StatusCode::OK);
    let listing: Value = serde_json::from_slice(&body).unwrap();
    let names: Vec<&str> = listing["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["inbox", "outbox"]);

    let (st, _, _) = as_user(
        &b,
        &b.owner,
        Method::GET,
        "/w/v1/drive/files/..%2F..%2Fetc%2Fpasswd",
        "",
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let (st, _, _) = as_user(
        &b,
        &b.owner,
        Method::DELETE,
        "/w/v1/drive/files/outbox/s1/page.html",
        "",
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert!(!b.root.join("outbox/s1/page.html").exists());

    let other = b
        .tokens
        .add("ops", Default::default(), Right::ALL.into())
        .unwrap();
    let (st, _, _) = as_user(&b, &other, Method::GET, "/w/v1/drive/files", "").await;
    assert_eq!(st, StatusCode::FORBIDDEN, "the owner's");
}
