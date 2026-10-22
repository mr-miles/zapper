//! A firm's run without VMs (`test_run`): finding a session's run, grants
//! and their network rules per repository, the git server's checks, the
//! firm's services and zone, and the office port's requests.

use std::sync::Arc;

use base64::Engine as _;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use zapper_policy::{AnyRule, Policy, BOARD};

use super::access::{self, Level};
use super::facility;
use super::{labels, run_of, test_run, FirmRun};
use crate::testing::TestCore;
use crate::{NewSession, Session};

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
    git: { repos: [api, web] }
";

fn run() -> (TestCore, Arc<FirmRun>) {
    let t = TestCore::new(FIRM);
    let run = test_run(&t.core, "acme");
    (t, run)
}

fn session_of(run: &FirmRun, worker: &str) -> Arc<Session> {
    run.rt(worker)
        .unwrap()
        .session
        .lock()
        .unwrap()
        .clone()
        .unwrap()
}

/// The ids of a worker's credential rules, and where the first allows.
fn credential_rules(run: &FirmRun, worker: &str) -> Vec<(String, String, Vec<u16>)> {
    let mut v: Vec<(String, String, Vec<u16>)> = session_of(run, worker)
        .policy
        .read()
        .unwrap()
        .rules()
        .into_iter()
        .filter_map(|r| match r {
            AnyRule::Net(n)
                if n.id
                    .as_deref()
                    .is_some_and(|i| i.starts_with("credentials-")) =>
            {
                Some((n.id.unwrap(), n.host, n.ports))
            }
            _ => None,
        })
        .collect();
    v.sort();
    v
}

fn ids(rules: &[(String, String, Vec<u16>)]) -> Vec<&str> {
    rules.iter().map(|(i, ..)| i.as_str()).collect()
}

#[test]
fn a_session_finds_its_run_by_its_labels() {
    let (t, run) = run();
    let found = run_of(t.core.office(), &session_of(&run, "pat")).unwrap();
    assert_eq!(found.id, run.id);

    // A session from an earlier run of the firm: not this run's.
    let mut old = NewSession::plain("w".into(), t.workdir(), Policy::default());
    old.labels = labels("acme", "an-earlier-run", "pat", true);
    let old = t.core.test_session(old, true).unwrap();
    assert!(run_of(t.core.office(), &old).is_none());
}

#[test]
fn the_ceo_holds_every_repository_and_others_nothing() {
    let (_t, run) = run();
    let host = format!("git.{}", run.domain);
    let ceo = credential_rules(&run, "ceo");
    assert_eq!(ids(&ceo), ["credentials-git-api", "credentials-git-web"]);
    assert!(ceo.iter().all(|(_, h, p)| *h == host && *p == [443]));
    assert!(credential_rules(&run, "pat").is_empty());
}

#[test]
fn a_git_grant_is_per_repository_with_its_own_network_rule() {
    let (t, run) = run();
    access::board_grant(&t.core, &run, "pat", "git:api", Level::Write, None, "test").unwrap();
    assert_eq!(ids(&credential_rules(&run, "pat")), ["credentials-git-api"]);
    assert_eq!(access::level(&run, "pat", "git:api"), Level::Write);
    assert_eq!(access::level(&run, "pat", "git:web"), Level::None);

    access::board_grant(&t.core, &run, "pat", "git:web", Level::Read, None, "test").unwrap();
    assert_eq!(
        ids(&credential_rules(&run, "pat")),
        ["credentials-git-api", "credentials-git-web"]
    );

    // Revoking one repository leaves the other's rule, and so its host.
    access::revoke(&t.core, &run, BOARD, "pat", "git:api", "done").unwrap();
    assert_eq!(ids(&credential_rules(&run, "pat")), ["credentials-git-web"]);
    assert_eq!(access::level(&run, "pat", "git:api"), Level::None);

    // With two repositories, plain `git` names none of them.
    let e = access::board_grant(&t.core, &run, "pat", "git", Level::Read, None, "test")
        .unwrap_err()
        .to_string();
    assert!(e.contains("name the repository"), "{e}");
}

/// The git server's answer to `member` asking for `path`, with `auth` as
/// their Basic credentials: their level, or the refusal's status and text.
fn git(
    run: &FirmRun,
    member: &str,
    auth: Option<&str>,
    path: &str,
) -> Result<Level, (u16, String)> {
    let mut headers = hyper::HeaderMap::new();
    if let Some(a) = auth {
        let v = base64::engine::general_purpose::STANDARD.encode(a);
        headers.insert(
            hyper::header::AUTHORIZATION,
            format!("Basic {v}").parse().unwrap(),
        );
    }
    let (path, query) = path.split_once('?').unwrap_or((path, ""));
    facility::authorize(run, member, &headers, path, query).map_err(|r| {
        let r = *r;
        let status = r.status().as_u16();
        let body =
            futures_util::FutureExt::now_or_never(http_body_util::BodyExt::collect(r.into_body()))
                .unwrap()
                .unwrap()
                .to_bytes();
        (status, String::from_utf8_lossy(&body).into_owned())
    })
}

#[test]
fn the_git_server_checks_who_asks_and_their_grant_on_the_repository() {
    let (t, run) = run();
    run.facility
        .git_passwords
        .lock()
        .unwrap()
        .insert("pat".into(), "pw".into());
    let refs = "/api.git/info/refs?service=git-upload-pack";
    let push = "/api.git/git-receive-pack";

    // Who: the password, for the worker the connection came from.
    assert_eq!(git(&run, "pat", None, refs).unwrap_err().0, 401);
    assert_eq!(
        git(&run, "pat", Some("pat:wrong"), refs).unwrap_err().0,
        401
    );
    assert_eq!(git(&run, "pat", Some("ceo:pw"), refs).unwrap_err().0, 401);

    // What: a grant on this repository.
    let (status, text) = git(&run, "pat", Some("pat:pw"), refs).unwrap_err();
    assert_eq!(status, 403);
    assert!(text.contains("no access to api"), "{text}");

    access::board_grant(&t.core, &run, "pat", "git:api", Level::Read, None, "test").unwrap();
    assert_eq!(git(&run, "pat", Some("pat:pw"), refs), Ok(Level::Read));
    let (status, text) = git(&run, "pat", Some("pat:pw"), push).unwrap_err();
    assert_eq!(status, 403);
    assert!(text.contains("you can read api"), "{text}");
    assert_eq!(
        git(
            &run,
            "pat",
            Some("pat:pw"),
            "/api.git/info/refs?service=git-receive-pack"
        )
        .unwrap_err()
        .0,
        403,
        "asking to push is a push"
    );

    access::board_grant(&t.core, &run, "pat", "git:api", Level::Write, None, "test").unwrap();
    assert_eq!(git(&run, "pat", Some("pat:pw"), push), Ok(Level::Write));

    // Other repositories: their own grant; unknown ones aren't there.
    assert_eq!(
        git(&run, "pat", Some("pat:pw"), "/web.git/info/refs")
            .unwrap_err()
            .0,
        403
    );
    let (status, text) = git(&run, "pat", Some("pat:pw"), "/other.git/info/refs").unwrap_err();
    assert_eq!(status, 404);
    assert!(
        text.contains("api.git") && text.contains("web.git"),
        "{text}"
    );
    assert_eq!(
        git(&run, "pat", Some("pat:pw"), "/api.git/../web.git/HEAD")
            .unwrap_err()
            .0,
        400
    );
}

/// Where `host:port` goes for `s`, in words.
fn route(t: &TestCore, s: &Session, host: &str, port: u16) -> String {
    use crate::daemon::services::{Route, Target};
    match t.core.daemon().services.route(s, host, port) {
        Some(Route::Service(Target::Handler { tls: true, .. })) => "git server".into(),
        Some(Route::Service(Target::Session { id, port })) => format!("session {id}:{port}"),
        Some(Route::Service(_)) => "other".into(),
        Some(Route::Unserved) => "unserved".into(),
        None => "host".into(),
    }
}

#[test]
fn the_firms_names_are_its_own() {
    let (t, run) = run();
    let d = run.domain.clone();
    let pat = session_of(&run, "pat");
    assert_eq!(route(&t, &pat, &format!("git.{d}"), 443), "git server");
    assert_eq!(route(&t, &pat, &format!("git.{d}"), 80), "unserved");
    // No cluster yet: its names are in the domain, unserved.
    assert_eq!(route(&t, &pat, &format!("cluster.{d}"), 6443), "unserved");
    assert_eq!(route(&t, &pat, &format!("x.{d}"), 443), "unserved");
    assert_eq!(route(&t, &pat, "example.com", 443), "host");

    let services = &t.core.daemon().services;
    assert_eq!(services.resolves(&pat, &format!("git.{d}")), Some(true));
    assert_eq!(services.resolves(&pat, &format!("nope.{d}")), Some(false));
    assert_eq!(services.resolves(&pat, "example.com"), None);

    // Not a session of the run: the names are nothing to it.
    let plain = t
        .core
        .test_session(
            NewSession::plain("w".into(), t.workdir(), Policy::default()),
            true,
        )
        .unwrap();
    assert_eq!(route(&t, &plain, &format!("git.{d}"), 443), "host");
}

#[tokio::test]
async fn the_cluster_serves_its_api_and_apps_until_the_run_stops() {
    let (t, run) = run();
    let d = run.domain.clone();
    let mut new = NewSession::plain("w".into(), t.workdir(), Policy::default());
    new.labels = labels(&run.firm, &run.id, facility::CLUSTER, false);
    let cluster = t.core.test_session(new, true).unwrap();
    facility::test_cluster(&t.core, &run, &cluster);
    let pat = session_of(&run, "pat");
    let c = &cluster.id;
    assert_eq!(
        route(&t, &pat, &format!("cluster.{d}"), 6443),
        format!("session {c}:6443")
    );
    assert_eq!(
        route(&t, &pat, &format!("apps.{d}"), 80),
        format!("session {c}:80")
    );
    assert_eq!(
        route(&t, &pat, &format!("shop.apps.{d}"), 443),
        format!("session {c}:443")
    );
    assert_eq!(route(&t, &pat, &format!("cluster.{d}"), 443), "unserved");

    facility::stop(&t.core, &run).await;
    assert!(t.core.services().is_empty(), "{:?}", t.core.services());
    assert_eq!(route(&t, &pat, &format!("git.{d}"), 443), "host");
}

/// One request over the office port, as `s`'s VM would make it.
async fn office(t: &TestCore, s: &Arc<Session>, request: &str) -> Value {
    let (host, mut guest) = UnixStream::pair().unwrap();
    let serve = tokio::spawn(super::ops::handle(t.core.clone(), s.clone(), host));
    guest
        .write_all(format!("{request}\n").as_bytes())
        .await
        .unwrap();
    let mut line = String::new();
    BufReader::new(&mut guest)
        .read_line(&mut line)
        .await
        .unwrap();
    serve.await.unwrap().unwrap();
    serde_json::from_str(&line).unwrap()
}

#[tokio::test]
async fn the_office_port_answers_for_the_vm_it_serves() {
    let (t, run) = run();
    let pat = session_of(&run, "pat");
    let dir = office(&t, &pat, r#"{"op":"directory"}"#).await;
    let workers = dir["ok"]["workers"].as_array().unwrap();
    assert!(workers.iter().any(|w| w["name"] == "pat"), "{dir}");

    // Who asks is the VM's member label, not anything in the request.
    let desk = office(&t, &pat, r#"{"op":"desk"}"#).await;
    assert!(desk["ok"].is_object(), "{desk}");

    let unknown = office(&t, &pat, r#"{"op":"no_such_op"}"#).await;
    assert!(unknown["error"].is_string(), "{unknown}");
    let bad = office(&t, &pat, "not json").await;
    assert!(
        bad["error"].as_str().unwrap().starts_with("bad request"),
        "{bad}"
    );

    // A VM of an earlier run: its firm isn't running for it.
    let mut old = NewSession::plain("w".into(), t.workdir(), Policy::default());
    old.labels = labels("acme", "an-earlier-run", "pat", true);
    let old = t.core.test_session(old, true).unwrap();
    let r = office(&t, &old, r#"{"op":"directory"}"#).await;
    assert_eq!(r["error"], "the firm isn't running");
}

/// The names of the tools the office port lists for `s`.
async fn tool_names(t: &TestCore, s: &Arc<Session>) -> Vec<String> {
    let v = office(t, s, r#"{"op":"tools"}"#).await;
    v["ok"]
        .as_array()
        .unwrap_or_else(|| panic!("{v}"))
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn each_workers_tools_are_their_roles() {
    let (t, run) = run();
    let ceo = tool_names(&t, &session_of(&run, "ceo")).await;
    let pat = tool_names(&t, &session_of(&run, "pat")).await;
    for both in ["desk", "mail_send", "directory"] {
        assert!(
            ceo.iter().any(|n| n == both) && pat.iter().any(|n| n == both),
            "{both}"
        );
    }
    // The CEO owns the firm's policy; an engineer doesn't.
    assert!(ceo.iter().any(|n| n == "credentials_list"), "{ceo:?}");
    assert!(!pat.iter().any(|n| n == "credentials_list"), "{pat:?}");
}

#[tokio::test]
async fn mail_is_from_the_vm_it_came_from() {
    let (t, run) = run();
    let pat = session_of(&run, "pat");
    let sent = office(
        &t,
        &pat,
        r#"{"op":"mail_send","args":{"to":["ceo"],"subject":"Hi","body":"hello","kind":"fyi","from":"ceo"}}"#,
    )
    .await;
    assert!(sent["ok"].is_object(), "{sent}");
    let inbox = office(&t, &session_of(&run, "ceo"), r#"{"op":"mail_inbox"}"#).await;
    let mail = &inbox["ok"]["messages"][0];
    assert_eq!(mail["from"], "pat", "{inbox}");
    assert_eq!(mail["subject"], "Hi", "{inbox}");
}

#[tokio::test]
async fn the_stop_hook_asks_whether_mail_came() {
    let (t, run) = run();
    let r = office(&t, &session_of(&run, "pat"), r#"{"op":"stop_check"}"#).await;
    assert_eq!(r["ok"]["block"], false, "{r}");
}

#[tokio::test]
async fn an_overlong_request_is_refused() {
    let (t, run) = run();
    let body = "x".repeat(zapper_policy::proto::MAX_OFFICE_LINE + 10);
    let request = format!(r#"{{"op":"mail_send","args":{{"body":"{body}"}}}}"#);
    let r = office(&t, &session_of(&run, "pat"), &request).await;
    let e = r["error"].as_str().unwrap_or_default();
    assert!(e.starts_with("request longer than"), "{e}");
}
