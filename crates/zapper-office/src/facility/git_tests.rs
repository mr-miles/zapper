//! The git server over HTTP, as a worker's git makes its requests: its
//! checks (`authorize`, tested on its own in `run_tests`) refuse, and what
//! they let through reaches git.

use std::convert::Infallible;
use std::sync::Arc;

use base64::Engine as _;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request};
use hyper_util::rt::TokioIo;

use super::super::access;
use super::super::{test_run, FirmRun};

const FIRM: &str = "
profiles:
  w: { agent: { kind: claude-code } }
firms:
  acme:
    roles:
      ceo: { profile: w, agent: ceo }
      eng: { profile: w, agent: software-engineer }
    workers:
      - { name: ceo, role: ceo }
      - { name: pat, role: eng, reports_to: ceo }
    git: { repos: [api, web] }
";

/// The git server's answer to `member` asking for `path`, with `auth` as
/// their Basic credentials: its status and text.
async fn git(run: &Arc<FirmRun>, member: &str, auth: Option<&str>, path: &str) -> (u16, String) {
    let (client, server) = tokio::io::duplex(1 << 16);
    let (run, who) = (run.clone(), member.to_string());
    tokio::spawn(async move {
        let svc = hyper::service::service_fn(move |req| {
            let (run, who) = (run.clone(), who.clone());
            async move { Ok::<_, Infallible>(super::git_request(&run, &who, req).await) }
        });
        let _ = hyper::server::conn::http1::Builder::new()
            .serve_connection(TokioIo::new(server), svc)
            .await;
    });
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(client))
        .await
        .unwrap();
    tokio::spawn(conn);
    let method = if path.ends_with("-pack") {
        Method::POST
    } else {
        Method::GET
    };
    let mut req = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "git.acme");
    if let Some(a) = auth {
        let v = base64::engine::general_purpose::STANDARD.encode(a);
        req = req.header(hyper::header::AUTHORIZATION, format!("Basic {v}"));
    }
    let r = sender
        .send_request(req.body(Full::new(Bytes::new())).unwrap())
        .await
        .unwrap();
    let status = r.status().as_u16();
    let body = r.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&body).into_owned())
}

/// Past the checks: whatever git itself answers (an empty push is a bad
/// request to it), not one of their refusals.
fn allowed((status, text): &(u16, String)) -> bool {
    ![401, 403, 404].contains(status) && !text.contains("bad path")
}

#[tokio::test]
async fn the_git_server_checks_who_asks_and_their_grant_on_the_repository() {
    let t = crate::test_core(FIRM);
    let run = test_run(&t.core, "acme");
    // The repositories as the firm's git server keeps them.
    for r in ["api", "web"] {
        let bare = run.dir.join("git").join(format!("{r}.git"));
        std::fs::create_dir_all(&bare).unwrap();
        let ok = std::process::Command::new("git")
            .args(["init", "-q", "--bare"])
            .arg(&bare)
            .status()
            .unwrap();
        assert!(ok.success());
    }
    run.facility
        .git_passwords
        .lock()
        .unwrap()
        .insert("pat".into(), "pw".into());
    let refs = "/api.git/info/refs?service=git-upload-pack";
    let push = "/api.git/git-receive-pack";

    // Who: the password, for the worker the connection came from.
    assert_eq!(git(&run, "pat", None, refs).await.0, 401);
    assert_eq!(git(&run, "pat", Some("pat:wrong"), refs).await.0, 401);
    assert_eq!(git(&run, "pat", Some("ceo:pw"), refs).await.0, 401);

    // What: a grant on this repository.
    let (status, text) = git(&run, "pat", Some("pat:pw"), refs).await;
    assert_eq!(status, 403);
    assert!(text.contains("no access to api"), "{text}");

    access::board_grant(&t.core, &run, "pat", "git:api", "read", None, "test").unwrap();
    let r = git(&run, "pat", Some("pat:pw"), refs).await;
    assert!(allowed(&r), "{r:?}");
    let (status, text) = git(&run, "pat", Some("pat:pw"), push).await;
    assert_eq!(status, 403);
    assert!(text.contains("you can read api"), "{text}");
    assert_eq!(
        git(
            &run,
            "pat",
            Some("pat:pw"),
            "/api.git/info/refs?service=git-receive-pack"
        )
        .await
        .0,
        403,
        "asking to push is a push"
    );

    access::board_grant(&t.core, &run, "pat", "git:api", "write", None, "test").unwrap();
    let r = git(&run, "pat", Some("pat:pw"), push).await;
    assert!(allowed(&r), "{r:?}");

    // Other repositories: their own grant; unknown ones aren't there.
    assert_eq!(
        git(&run, "pat", Some("pat:pw"), "/web.git/info/refs")
            .await
            .0,
        403
    );
    let (status, text) = git(&run, "pat", Some("pat:pw"), "/other.git/info/refs").await;
    assert_eq!(status, 404);
    assert!(
        text.contains("api.git") && text.contains("web.git"),
        "{text}"
    );
    assert_eq!(
        git(&run, "pat", Some("pat:pw"), "/api.git/../web.git/HEAD")
            .await
            .0,
        400
    );
}
