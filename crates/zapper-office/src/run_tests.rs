//! A firm's run without VMs (`test_run`): finding a session's run, grants
//! and their network rules per repository, the git server's checks, the
//! firm's services and zone, and the office port's requests.

use std::sync::Arc;

use crate::firm::BOARD;
use base64::Engine as _;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use zapper_policy::{AnyRule, Policy};

use super::access::{self, Level};
use super::facility;
use super::{labels, run_of, test_run, FirmRun};
use crate::OfficeOf;
use zapper_core::testing::TestCore;
use zapper_core::{NewSession, Session};

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
    let t = crate::test_core(FIRM);
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
    let found = run_of(&t.core.office(), &session_of(&run, "pat")).unwrap();
    assert_eq!(found.id, run.id);

    // A session from an earlier run of the firm: not this run's.
    let mut old = NewSession::plain("w".into(), t.workdir(), Policy::default());
    old.labels = labels("acme", "an-earlier-run", "pat", true);
    let old = t.core.test_session(old, true).unwrap();
    assert!(run_of(&t.core.office(), &old).is_none());
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
    use zapper_core::ServiceTarget as Target;
    match zapper_core::testing::route(&t.core, s, host, port) {
        Some(Some(Target::Handler { tls: true, .. })) => "git server".into(),
        Some(Some(Target::Session { id, port })) => format!("session {id}:{port}"),
        Some(Some(_)) => "other".into(),
        Some(None) => "unserved".into(),
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

    assert_eq!(
        zapper_core::testing::resolves(&t.core, &pat, &format!("git.{d}")),
        Some(true)
    );
    assert_eq!(
        zapper_core::testing::resolves(&t.core, &pat, &format!("nope.{d}")),
        Some(false)
    );
    assert_eq!(
        zapper_core::testing::resolves(&t.core, &pat, "example.com"),
        None
    );

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

/// One request to the office's tools, as `s`'s VM makes it through the
/// gateway: `{"op", "args"}` as a call of that tool (anything else as the
/// line itself). The answer as `{"ok": ...}` or `{"error": "..."}`.
async fn office(t: &TestCore, s: &Arc<Session>, request: &str) -> Value {
    let (host, mut guest) = UnixStream::pair().unwrap();
    let (d, vm) = (t.core.clone(), s.clone());
    tokio::spawn(async move { zapper_core::testing::mcp(&d, &vm, host).await });
    let line = match serde_json::from_str::<Value>(request) {
        Ok(v) if v.get("op").is_some() => json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": v["op"], "arguments": v.get("args").cloned().unwrap_or(json!({})) },
        })
        .to_string(),
        _ => request.to_string(),
    };
    let (r, mut w) = guest.split();
    let send = async move {
        // A line too long to be read is answered before it is all sent.
        let _ = w.write_all(format!("office\n{line}\n").as_bytes()).await;
    };
    let mut reply = String::new();
    let mut r = BufReader::new(r);
    let read = r.read_line(&mut reply);
    let (_, n) = tokio::join!(send, read);
    n.unwrap();
    let v: Value = serde_json::from_str(&reply).unwrap();
    if let Some(e) = v.get("error") {
        return json!({ "error": e["message"] });
    }
    let text = v["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    if v["result"]["isError"] == json!(true) {
        return json!({ "error": text });
    }
    json!({ "ok": serde_json::from_str::<Value>(text).unwrap() })
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

/// Every tool any worker could have: what each worker may call is exactly
/// what their list shows, and an officer's tool says it is one.
#[tokio::test]
async fn a_worker_may_call_exactly_the_tools_they_are_shown() {
    use crate::ops::{tools, Powers};
    let (t, run) = run();
    let every = Powers {
        officer: true,
        budget: true,
        root: true,
        hires: true,
        manages: true,
        contractor: true,
        owner: true,
    };
    let officers: Vec<Value> = tools(Powers {
        officer: true,
        ..Default::default()
    });
    let mut all: Vec<String> = tools(every)
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    all.sort();
    all.dedup();
    for who in ["ceo", "pat"] {
        let s = session_of(&run, who);
        let mine = tool_names(&t, &s).await;
        let officer = run.worker(who).unwrap().officer();
        for name in &all {
            let r = office(&t, &s, &json!({ "op": name, "args": {} }).to_string()).await;
            let e = r["error"].as_str().unwrap_or("");
            let gated =
                e.ends_with("isn't one of your tools") || e.ends_with("is for security officers");
            assert_eq!(gated, !mine.contains(name), "{who} {name}: {r}");
            if gated && !officer && officers.iter().any(|t| t["name"] == name.as_str()) {
                assert_eq!(e, format!("{name} is for security officers"));
            }
        }
    }
}

/// The tool definitions every combination of powers is shown, names,
/// descriptions and schemas, as a digest: what the agents see doesn't
/// change unless this does.
#[test]
fn the_tool_lists_are_as_they_were() {
    use crate::ops::{tools, Powers};
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
    for bits in 0u8..128 {
        let b = |i: u8| bits & (1 << i) != 0;
        let p = Powers {
            officer: b(0),
            budget: b(1),
            root: b(2),
            hires: b(3),
            manages: b(4),
            contractor: b(5),
            owner: b(6),
        };
        ctx.update(serde_json::to_string(&tools(p)).unwrap().as_bytes());
    }
    let digest: String = ctx
        .finish()
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(
        digest,
        "16a67e8926d72a8467d2342b75026ea2c0fe964680cebb3354c91d5d63f7ef20"
    );
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
    let body = "x".repeat(zapper_policy::proto::MAX_MCP_LINE + 10);
    let request = format!(r#"{{"op":"mail_send","args":{{"body":"{body}"}}}}"#);
    let r = office(&t, &session_of(&run, "pat"), &request).await;
    let e = r["error"].as_str().unwrap_or_default();
    assert!(e.starts_with("request longer than"), "{e}");
}

#[tokio::test]
async fn once_the_firm_stops_its_tools_are_gone() {
    let (t, run) = run();
    super::ops::unregister(&t.core, &run);
    let r = office(&t, &session_of(&run, "pat"), r#"{"op":"desk"}"#).await;
    assert_eq!(r["error"], "no MCP server office for this session", "{r}");
}

#[tokio::test]
async fn the_office_server_introduces_itself() {
    let (t, run) = run();
    let (host, guest) = UnixStream::pair().unwrap();
    let (d, s) = (t.core.clone(), session_of(&run, "pat"));
    tokio::spawn(async move { zapper_core::testing::mcp(&d, &s, host).await });
    let (r, mut w) = tokio::io::split(guest);
    w.write_all(
        b"office\n{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n",
    )
    .await
    .unwrap();
    let mut line = String::new();
    BufReader::new(r).read_line(&mut line).await.unwrap();
    let v: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(v["result"]["serverInfo"]["name"], "office");
    assert!(
        v["result"]["instructions"]
            .as_str()
            .unwrap()
            .contains("office-mail"),
        "{v}"
    );
}

// Where the office meets the core: a restart, the operator's rule changes,
// and the daemon stopping.

/// A daemon started again on `t`'s config and state, as after a restart.
fn restart(t: &TestCore) -> zapper_core::Core {
    let d = zapper_core::Core::start(t.startup()).unwrap();
    crate::start(&d);
    d
}

/// The run's journal, one JSON value a line.
fn journal(run: &FirmRun) -> Vec<Value> {
    std::fs::read_to_string(run.dir.join("journal.jsonl"))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[test]
fn a_restart_ends_the_runs_the_daemon_left_running() {
    let (t, run) = run();
    assert_ne!(journal(&run).last().unwrap()["state"], "ended");
    let _again = restart(&t);
    let last = journal(&run).last().unwrap().clone();
    assert_eq!(last["type"], "run", "{last}");
    assert_eq!(last["state"], "ended", "{last}");
    assert_eq!(last["reason"], "the daemon stopped", "{last}");
}

/// The `config` events of `worker`'s session: what changed in its rules.
fn rule_changes(t: &TestCore, run: &FirmRun, worker: &str) -> Vec<String> {
    let s = session_of(run, worker);
    t.core
        .recent_events(Some(&s.id))
        .into_iter()
        .map(|e| serde_json::to_value(e).unwrap())
        .filter(|e| e["type"] == "config")
        .map(|e| e["change"].as_str().unwrap().to_string())
        .collect()
}

/// What the office puts in a worker's own rules (an officer's approval for
/// them, the network a credential grant needs) is in their session's audit
/// log, added and removed, like any session rule.
#[test]
fn a_workers_own_rules_from_the_office_are_in_their_audit_log() {
    use super::access::Level;
    use super::security::{self, DecideArgs};
    let (t, run) = run();
    let session = format!("session:{}", session_of(&run, "pat").id);
    let req = zapper_policy::NetRequest {
        host: "api.example".into(),
        port: 443,
        method: None,
        path: None,
    };
    let id = security::ask(&run, "pat", &req, None, None, "for the test").unwrap();
    let decide = DecideArgs {
        allow: true,
        scope: "worker".into(),
        reason: "pat's work".into(),
        methods: None,
        path: None,
        expires_in: None,
    };
    security::decide(&run, BOARD, &id, decide).unwrap();
    let rule = security::standing(&run)[0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    security::revoke(&run, BOARD, &rule, "done").unwrap();

    facility::network_follows_grant(&run, "pat", "git:api", Level::Write);
    facility::network_follows_grant(&run, "pat", "git:api", Level::None);

    assert_eq!(
        rule_changes(&t, &run, "pat"),
        [
            format!("rule {rule} added to {session}"),
            format!("rule {rule} removed from {session}"),
            format!("rule credentials-git-api added to {session}"),
            format!("rule credentials-git-api removed from {session}"),
        ]
    );
}

#[test]
fn an_officers_approval_for_a_role_is_a_rule_the_operator_can_revoke() {
    use super::security::{self, DecideArgs};
    let (t, run) = run();
    let req = zapper_policy::NetRequest {
        host: "api.example".into(),
        port: 443,
        method: None,
        path: None,
    };
    let id = security::ask(&run, "pat", &req, None, None, "for the test").unwrap();
    security::decide(
        &run,
        BOARD,
        &id,
        DecideArgs {
            allow: true,
            scope: "role".into(),
            reason: "everyone in eng".into(),
            methods: None,
            path: None,
            expires_in: None,
        },
    )
    .unwrap();
    let rule = security::standing(&run)[0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let role = zapper_policy::Scope::Role("acme/eng".into());
    let in_layers = |t: &TestCore| {
        t.core
            .rule_layers()
            .into_iter()
            .any(|(s, p)| s == role && p.rules().iter().any(|r| r.id() == Some(rule.as_str())))
    };
    assert!(in_layers(&t), "listed with the role's layer");

    assert!(matches!(
        t.core.remove_rule(&rule).unwrap(),
        zapper_core::RuleRemoval::Revoked
    ));
    assert!(security::standing(&run).is_empty());
    assert!(!in_layers(&t));
    let revoked = journal(&run)
        .into_iter()
        .any(|e| e["type"] == "access" && e["event"] == "revoked" && e["rule"] == rule.as_str());
    assert!(revoked, "{:?}", journal(&run));
}

#[tokio::test]
async fn the_daemon_stopping_ends_its_firms() {
    let (t, run) = run();
    t.core.shutdown().await;
    assert!(run.ended());
    let ended = journal(&run)
        .into_iter()
        .any(|e| e["type"] == "run" && e["state"] == "ended");
    assert!(ended, "{:?}", journal(&run));
}

// The firms' config, checked with the rest.

#[test]
fn a_firm_that_doesnt_add_up_is_refused_with_the_whole_change() {
    let (t, _run) = run();
    let e = t
        .core
        .change("a firm", |c| {
            c.profiles.insert("extra".into(), Default::default());
            let mut bad = crate::firm::get(c, "acme").unwrap();
            bad.workers[1].role = "nosuch".into();
            c.firms
                .insert("broken".into(), serde_yaml::to_value(bad).unwrap());
            Ok(())
        })
        .unwrap_err();
    let e = format!("{e:#}");
    assert!(e.contains("firm broken:"), "{e}");
    let c = t.core.config();
    assert!(!c.firms.contains_key("broken") && !c.profiles.contains_key("extra"));
}

#[test]
fn firms_need_the_driver() {
    let (t, _run) = run();
    let e = t
        .core
        .change("driver off", |c| {
            c.daemon.driver = false;
            Ok(())
        })
        .unwrap_err();
    assert!(format!("{e:#}").contains("firms need the driver"), "{e:#}");
}

#[test]
fn a_startup_config_with_a_broken_firm_doesnt_load() {
    let t = crate::test_core("profiles:\n  w: {}\n");
    std::fs::write(
        t.startup(),
        std::fs::read_to_string(t.startup()).unwrap()
            + "firms:\n  acme:\n    roles: { ceo: { profile: nosuch, skills: { work: ceo } } }\n    workers: [{ name: ceo, role: ceo }]\n",
    )
    .unwrap();
    let e = match zapper_core::Core::start(t.startup()) {
        Ok(d) => {
            crate::start(&d);
            d.check_config().unwrap_err()
        }
        Err(e) => e,
    };
    assert!(format!("{e:#}").contains("firm acme:"), "{e:#}");
}
