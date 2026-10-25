//! The office's tools as workers call them, through the gateway as their
//! VMs do: mail read by whoever reads it, access asked for and decided by
//! an officer, credentials granted by a policy owner, and a budgeted firm's
//! money: the board's budget, a hire, funding, and a contract paid for.

use std::sync::Arc;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use zapper_core::testing::TestCore;
use zapper_core::{NewSession, Session};
use zapper_policy::{NetRequest, Policy};

use crate::{labels, security, test_run, FirmRun};

const FIRM: &str = "
profiles:
  w:
    agent: { kind: claude-code }
    policy:
      net:
        - { action: ask, host: '*.example' }
        - { action: allow, host: registry.npmjs.org, ports: [443] }
firms:
  acme:
    budget: { seed: 1000000, security_share: 0.2, quote_fee: 10000 }
    security: { officers: [sec] }
    roles:
      ceo: { profile: w, agent: ceo, hires: [eng] }
      eng: { profile: w, agent: software-engineer }
      sec: { profile: w, agent: security-officer, officer: true }
    workers:
      - { name: ceo, role: ceo }
      - { name: sec, role: sec, reports_to: ceo }
      - { name: pat, role: eng, reports_to: ceo }
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

/// `who` calls `name` with `args`, from their VM: the tool's answer, or
/// `{"error": ...}`.
async fn call(t: &TestCore, run: &FirmRun, who: &str, name: &str, args: Value) -> Value {
    let s = session_of(run, who);
    let (host, mut guest) = UnixStream::pair().unwrap();
    let (d, vm) = (t.core.clone(), s.clone());
    tokio::spawn(async move { zapper_core::testing::mcp(&d, &vm, host).await });
    let line = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": name, "arguments": args } });
    guest
        .write_all(format!("office\n{line}\n").as_bytes())
        .await
        .unwrap();
    let mut reply = String::new();
    BufReader::new(guest).read_line(&mut reply).await.unwrap();
    let v: Value = serde_json::from_str(&reply).unwrap();
    let text = v["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    if v["result"]["isError"] == json!(true) {
        return json!({ "error": text });
    }
    serde_json::from_str(text).unwrap()
}

/// As `call`, which must succeed.
async fn ok(t: &TestCore, run: &FirmRun, who: &str, name: &str, args: Value) -> Value {
    let v = call(t, run, who, name, args).await;
    assert!(v.get("error").is_none(), "{who} {name}: {v}");
    v
}

/// As `call`, which must fail: its error.
async fn refused(t: &TestCore, run: &FirmRun, who: &str, name: &str, args: Value) -> String {
    let v = call(t, run, who, name, args).await;
    v["error"]
        .as_str()
        .unwrap_or_else(|| panic!("{who} {name} went through: {v}"))
        .to_string()
}

/// A worker who joined during the run gets a VM: a session of theirs.
fn give_vm(t: &TestCore, run: &FirmRun, worker: &str) {
    let mut new = NewSession::plain("w".into(), run.dir.join("empty"), Policy::default());
    new.labels = labels(&run.firm, &run.id, worker, true);
    *run.rt(worker).unwrap().session.lock().unwrap() =
        Some(t.core.test_session(new, true).unwrap());
}

fn journal(run: &FirmRun) -> Vec<Value> {
    std::fs::read_to_string(run.dir.join("journal.jsonl"))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[tokio::test]
async fn mail_is_read_by_whoever_reads_its_thread_or_message() {
    let (t, run) = run();
    let sent = ok(
        &t,
        &run,
        "ceo",
        "mail_send",
        json!({ "to": "pat, sec", "subject": "Kickoff", "body": "Read this.", "kind": "fyi" }),
    )
    .await;
    let unread = |who: &'static str| {
        let (t, run) = (&t, &run);
        async move { ok(t, run, who, "mail_inbox", json!({ "unread": true })).await["unread"].clone() }
    };
    assert_eq!(
        (unread("pat").await, unread("sec").await),
        (json!(1), json!(1))
    );

    let thread = ok(
        &t,
        &run,
        "pat",
        "mail_thread",
        json!({ "thread": sent["thread"] }),
    )
    .await;
    assert_eq!(thread["messages"][0]["body"], "Read this.", "{thread}");
    let message = ok(
        &t,
        &run,
        "sec",
        "mail_read",
        json!({ "message": sent["id"] }),
    )
    .await;
    assert_eq!(message["subject"], "Kickoff", "{message}");
    assert_eq!(
        (unread("pat").await, unread("sec").await),
        (json!(0), json!(0))
    );
    let inbox = ok(&t, &run, "pat", "mail_inbox", json!({})).await;
    assert!(inbox["note"].is_string(), "nothing new says so: {inbox}");

    assert!(refused(&t, &run, "pat", "mail_read", json!({}))
        .await
        .contains("message is required"));
    assert!(
        refused(&t, &run, "pat", "mail_thread", json!({ "thread": "t-99" }))
            .await
            .contains("t-99")
    );
}

/// A message's body lives only in the journal; `mail_read` reads it back
/// from there. If the journal write fails (a full disk), the body is gone, so
/// `mail_send` must fail rather than deliver a message that reads back empty.
#[tokio::test]
async fn mail_send_fails_when_the_body_cannot_be_journaled() {
    let (t, run) = run();
    run.journal.fail_writes();
    let err = refused(
        &t,
        &run,
        "ceo",
        "mail_send",
        json!({ "to": "pat", "subject": "Kickoff", "body": "the real body", "kind": "fyi" }),
    )
    .await;
    assert!(
        err.contains("record"),
        "says the record couldn't be written: {err}"
    );
    // Nothing was delivered: pat has no mail to read back empty.
    let inbox = ok(&t, &run, "pat", "mail_inbox", json!({})).await;
    assert!(inbox["note"].is_string(), "pat got no message: {inbox}");
}

#[tokio::test]
async fn access_is_asked_for_and_an_officer_decides_and_revokes_it() {
    let (t, run) = run();
    let asked = ok(
        &t,
        &run,
        "pat",
        "request_access",
        json!({ "host": "API.Example", "methods": "get, post", "reason": "the docs" }),
    )
    .await;
    let id = asked["request"].as_str().unwrap().to_string();
    assert!(refused(
        &t,
        &run,
        "pat",
        "request_access",
        json!({ "host": "a b", "reason": "x" })
    )
    .await
    .starts_with("host:"));

    let pending = ok(&t, &run, "sec", "access_requests", json!({})).await;
    let r = pending
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == id.as_str())
        .unwrap()
        .clone();
    assert_eq!(r["host"], "api.example", "{r}");
    assert_eq!(r["reason"], "the docs", "{r}");
    assert!(refused(&t, &run, "pat", "access_requests", json!({}))
        .await
        .contains("security officers"));
    assert_eq!(
        ok(
            &t,
            &run,
            "sec",
            "traffic",
            json!({ "since": "1h", "limit": 5 })
        )
        .await,
        json!([])
    );
    assert!(ok(&t, &run, "sec", "traffic_digest", json!({}))
        .await
        .is_string());

    assert!(refused(
        &t,
        &run,
        "sec",
        "access_decide",
        json!({ "request": id, "scope": "worker", "reason": "x" })
    )
    .await
    .contains("allow is required"));
    ok(
        &t,
        &run,
        "sec",
        "access_decide",
        json!({ "request": id, "allow": true, "scope": "worker", "reason": "for the docs", "methods": ["GET"], "expires_in": "1h" }),
    )
    .await;
    let rules = ok(&t, &run, "sec", "access_rules", json!({})).await;
    let rule = rules[0]["id"].as_str().unwrap().to_string();
    assert_eq!(rules[0]["scope"], "worker:pat", "{rules}");
    ok(
        &t,
        &run,
        "sec",
        "access_revoke",
        json!({ "rule": rule, "reason": "done" }),
    )
    .await;
    assert_eq!(
        ok(&t, &run, "sec", "access_rules", json!({})).await,
        json!([])
    );

    // One more, handed to the board.
    let again = ok(
        &t,
        &run,
        "pat",
        "request_access",
        json!({ "host": "secrets.example", "reason": "keys" }),
    )
    .await;
    ok(
        &t,
        &run,
        "sec",
        "escalate",
        json!({ "request": again["request"], "recommendation": "the board's call" }),
    )
    .await;
    let escalated = journal(&run)
        .into_iter()
        .any(|e| e["type"] == "access" && e["event"] == "escalated");
    assert!(escalated, "{:?}", journal(&run));
}

/// Asking again for what a waiting request covers joins it. Asking for
/// more replaces it with one request for both, closing the old one as
/// merged: one request waits at a time, and a request never grows after a
/// decider may have seen it.
#[tokio::test]
async fn asking_again_joins_or_replaces_the_request_waiting() {
    let (t, run) = run();
    let ask = |methods: Value| {
        let (t, run) = (&t, &run);
        async move {
            let r = ok(
                t,
                run,
                "pat",
                "request_access",
                json!({ "host": "api.example", "methods": methods, "reason": "the docs" }),
            )
            .await;
            let pending = ok(t, run, "sec", "access_requests", json!({})).await;
            assert_eq!(pending.as_array().unwrap().len(), 1, "one waits: {pending}");
            (
                r["request"].as_str().unwrap().to_string(),
                pending[0].clone(),
            )
        }
    };
    let (first, _) = ask(json!(["GET"])).await;
    let (again, _) = ask(json!(["get"])).await;
    assert_eq!(again, first, "the same again joins it");
    let (both, waiting) = ask(json!(["POST", "GET"])).await;
    assert_ne!(both, first, "more replaces it");
    assert_eq!(waiting["methods"], json!(["GET", "POST"]), "{waiting}");
    assert_eq!(waiting["attempts"], 3, "{waiting}");
    let (whole, waiting) = ask(json!(null)).await;
    assert_ne!(whole, both, "the whole connection is more again");
    assert!(waiting.get("methods").is_none(), "{waiting}");
    assert_eq!(
        ask(json!(["DELETE"])).await.0,
        whole,
        "the whole connection covers it"
    );
    let count = |event: &str| {
        journal(&run)
            .into_iter()
            .filter(|e| e["type"] == "access" && e["event"] == event)
            .count()
    };
    assert_eq!((count("requested"), count("merged")), (3, 2));
    // Another port is another request.
    let other = ok(
        &t,
        &run,
        "pat",
        "request_access",
        json!({ "host": "api.example", "port": 8443, "reason": "the admin API" }),
    )
    .await;
    assert_ne!(other["request"], json!(whole));
}

/// A decider who saw the GET decides it just as the worker asks for POST
/// too: nothing is decided, and they are told what waits now.
#[tokio::test]
async fn deciding_a_request_that_was_replaced_decides_nothing() {
    let (t, run) = run();
    let asked = |methods: Value| {
        let (t, run) = (&t, &run);
        async move {
            ok(
                t,
                run,
                "pat",
                "request_access",
                json!({ "host": "api.example", "methods": methods, "reason": "the docs" }),
            )
            .await["request"]
                .as_str()
                .unwrap()
                .to_string()
        }
    };
    let seen = asked(json!(["GET"])).await;
    let now = asked(json!(["POST"])).await;
    let e = refused(
        &t,
        &run,
        "sec",
        "access_decide",
        json!({ "request": seen, "allow": true, "scope": "worker", "reason": "the docs" }),
    )
    .await;
    assert!(
        e.contains(&format!("now {now}: GET, POST api.example:443")),
        "{e}"
    );
    assert!(security::standing(&run).is_empty(), "nothing allowed");
    let pending = ok(&t, &run, "sec", "access_requests", json!({})).await;
    assert_eq!(pending[0]["id"], json!(now), "{pending}");
}

/// What the worker's rules already allow isn't asked about: the answer
/// says so, and nobody gets a request to decide.
#[tokio::test]
async fn access_already_allowed_is_not_asked_for() {
    let (t, run) = run();
    let r = ok(
        &t,
        &run,
        "pat",
        "request_access",
        json!({ "host": "registry.npmjs.org", "methods": ["get"], "reason": "npm install" }),
    )
    .await;
    assert_eq!(r["allowed"], true, "{r}");
    assert!(r["request"].is_null(), "{r}");
    assert!(
        r["note"].as_str().unwrap().contains("already allowed"),
        "{r}"
    );
    assert_eq!(
        ok(&t, &run, "sec", "access_requests", json!({})).await,
        json!([])
    );
    let asked = journal(&run)
        .into_iter()
        .any(|e| e["type"] == "access" && e["event"] == "requested");
    assert!(!asked, "{:?}", journal(&run));
    // Another port isn't allowed, so it is asked about.
    let r = ok(
        &t,
        &run,
        "pat",
        "request_access",
        json!({ "host": "registry.npmjs.org", "port": 80, "reason": "npm over http" }),
    )
    .await;
    assert!(r["request"].is_string(), "{r}");
}

/// Nobody decides their own access request: an officer's own network or
/// credential request goes to someone else, and deciding it themselves is
/// refused.
#[tokio::test]
async fn nobody_decides_their_own_access_request() {
    let (t, run) = run();
    let asked = ok(
        &t,
        &run,
        "sec",
        "request_access",
        json!({ "host": "notes.example", "reason": "research" }),
    )
    .await;
    let id = asked["request"].as_str().unwrap().to_string();
    let r = crate::security::requests(&run, true)
        .into_iter()
        .find(|r| r["id"] == id.as_str())
        .unwrap();
    assert_ne!(r["officer"], "sec", "not given to the requester: {r}");
    let e = refused(
        &t,
        &run,
        "sec",
        "access_decide",
        json!({ "request": id, "allow": true, "scope": "once", "reason": "mine" }),
    )
    .await;
    assert!(e.contains("your own"), "{e}");
    assert!(crate::security::standing(&run).is_empty());

    // Credentials: policy owners hold admin on everything while they hold
    // the role, so their own requests can't arise; anyone else's own is
    // refused before who may decide is even looked at.
    let e = refused(
        &t,
        &run,
        "sec",
        "credentials_request",
        json!({ "resource": "drive", "level": "write", "spaces": "general", "reason": "to publish" }),
    )
    .await;
    assert!(e.contains("already have"), "{e}");
    let asked = ok(
        &t,
        &run,
        "pat",
        "credentials_request",
        json!({ "resource": "drive", "level": "write", "spaces": "general", "reason": "to publish" }),
    )
    .await;
    let id = asked["requests"][0].as_str().unwrap().to_string();
    let e = crate::access::decide(&t.core, &run, "pat", &id, true, None, None, "mine")
        .unwrap_err()
        .to_string();
    assert!(e.contains("your own"), "{e}");
}

/// An officer's approval never gets past a `final` rule: the standing
/// rule is made, at the worker's layer, and the request is still denied,
/// while the same approval for another host lets it through.
#[tokio::test]
async fn an_approval_never_gets_past_a_final_rule() {
    let t = crate::test_core(&format!(
        "policy:\n  net:\n    - {{ action: deny, host: vault.example, final: true }}\n{FIRM}"
    ));
    let run = test_run(&t.core, "acme");
    let decide = |host: &'static str| {
        let (t, run) = (&t, &run);
        async move {
            let asked = ok(
                t,
                run,
                "pat",
                "request_access",
                json!({ "host": host, "reason": "x" }),
            )
            .await;
            ok(
                t,
                run,
                "sec",
                "access_decide",
                json!({ "request": asked["request"], "allow": true, "scope": "worker", "reason": "fine" }),
            )
            .await;
        }
    };
    decide("vault.example").await;
    decide("docs.example").await;
    assert_eq!(crate::security::standing(&run).len(), 2);
    let s = session_of(&run, "pat");
    let layers = t.core.layers_for(Some(&s.id), None).unwrap();
    let layers: Vec<_> = layers.iter().map(|(sc, p)| (sc.clone(), p)).collect();
    let action = |host: &str| {
        let req = NetRequest {
            host: host.into(),
            port: 443,
            method: None,
            path: None,
        };
        zapper_policy::evaluate_net(&layers, &req).0.action
    };
    assert_eq!(action("vault.example"), zapper_policy::Action::Deny);
    assert_eq!(action("docs.example"), zapper_policy::Action::Allow);
}

/// A hired officer decides nothing before their background check clears
/// them: no requests, no checks, no decisions; once cleared, they do.
#[tokio::test]
async fn a_hired_officer_decides_nothing_before_clearance() {
    let t = crate::test_core(&FIRM.replace("hires: [eng]", "hires: [eng, sec]"));
    let run = test_run(&t.core, "acme");
    approved(&t, &run).await;
    let hired = ok(
        &t,
        &run,
        "ceo",
        "hire",
        json!({ "role": "sec", "budget": "200k", "subject": "Security", "brief": "Watch the traffic.", "reason": "A second officer.", "name": "kim" }),
    )
    .await;
    assert_eq!(hired["worker"], "kim", "{hired}");
    give_vm(&t, &run, "kim");
    let asked = ok(
        &t,
        &run,
        "pat",
        "request_access",
        json!({ "host": "docs.example", "reason": "the docs" }),
    )
    .await;
    let request = asked["request"].as_str().unwrap().to_string();
    for (tool, args) in [
        ("access_requests", json!({})),
        ("background_checks", json!({})),
        (
            "access_decide",
            json!({ "request": request, "allow": true, "scope": "once", "reason": "x" }),
        ),
    ] {
        let v = call(&t, &run, "kim", tool, args).await;
        assert!(v.get("error").is_some(), "kim {tool} before clearance: {v}");
    }
    let check = ok(&t, &run, "sec", "background_checks", json!({})).await;
    let check = check
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["worker"] == "kim")
        .unwrap()["screening"]
        .as_str()
        .unwrap()
        .to_string();
    ok(
        &t,
        &run,
        "sec",
        "background_check",
        json!({ "screening": check, "clear": true, "note": "fine" }),
    )
    .await;
    ok(&t, &run, "kim", "access_requests", json!({})).await;
}

#[tokio::test]
async fn credentials_are_asked_for_and_a_policy_owner_grants_them() {
    let (t, run) = run();
    assert!(refused(
        &t,
        &run,
        "pat",
        "credentials_request",
        json!({ "resource": "drive", "level": "write", "reason": "x" })
    )
    .await
    .contains("name the spaces"));
    let asked = ok(
        &t,
        &run,
        "pat",
        "credentials_request",
        json!({ "resource": "drive", "level": "write", "spaces": "general", "reason": "to publish" }),
    )
    .await;
    let id = asked["requests"][0].as_str().unwrap().to_string();
    let mine = ok(&t, &run, "pat", "credentials", json!({})).await;
    assert!(
        mine["note"].is_string(),
        "a pending request says to wait: {mine}"
    );
    assert!(refused(&t, &run, "pat", "credentials_list", json!({}))
        .await
        .contains("isn't one of your tools"));

    let all = ok(&t, &run, "ceo", "credentials_list", json!({})).await;
    assert!(all.to_string().contains(&id), "{all}");
    assert!(refused(
        &t,
        &run,
        "sec",
        "credentials_decide",
        json!({ "request": id, "note": "x" })
    )
    .await
    .contains("grant: true or false"));
    ok(
        &t,
        &run,
        "sec",
        "credentials_decide",
        json!({ "request": id, "grant": true, "expires_in": "2h", "note": "go ahead" }),
    )
    .await;
    let mine = ok(&t, &run, "pat", "credentials", json!({})).await;
    assert!(mine.to_string().contains("drive:general"), "{mine}");
    ok(
        &t,
        &run,
        "sec",
        "credentials_revoke",
        json!({ "worker": "pat", "resource": "drive:general", "reason": "done" }),
    )
    .await;
    let mine = ok(&t, &run, "pat", "credentials", json!({})).await;
    assert!(
        mine["grants"].as_array().is_none_or(|g| g.is_empty()),
        "{mine}"
    );
}

#[tokio::test]
async fn a_budgeted_firm_hires_funds_and_pays_for_a_contract() {
    let (t, run) = run();
    let balance = |who: &'static str| {
        let (t, run) = (&t, &run);
        async move {
            ok(t, run, who, "budget", json!({})).await["you"]["balance"]
                .as_i64()
                .unwrap()
        }
    };
    assert!(refused(
        &t,
        &run,
        "ceo",
        "hire",
        json!({ "role": "eng", "budget": "100k", "brief": "x", "reason": "x" })
    )
    .await
    .contains("hasn't approved a budget"));
    assert!(refused(&t, &run, "pat", "budget_request", json!({}))
        .await
        .contains("isn't one of your tools"));

    // A contract offered on the seed, before the board's budget: the quote
    // is free to ask for, the job can't be paid for yet.
    let offer = ok(
        &t,
        &run,
        "ceo",
        "contract_offer",
        json!({ "role": "eng", "title": "Auditor", "brief": "Audit it." }),
    )
    .await;
    let contract = offer["contract"].as_str().unwrap().to_string();
    let contractor = offer["candidate"].as_str().unwrap().to_string();
    give_vm(&t, &run, &contractor);
    let c: &'static str = Box::leak(contractor.clone().into_boxed_str());
    ok(
        &t,
        &run,
        c,
        "contract_quote",
        json!({ "contract": contract, "scope": "The audit.", "budget": "300k", "due": "in 6h" }),
    )
    .await;
    let mine = ok(&t, &run, c, "contracts", json!({})).await;
    assert!(mine.to_string().contains("quoted"), "{mine}");
    assert!(refused(
        &t,
        &run,
        "ceo",
        "contract_accept",
        json!({ "contract": contract })
    )
    .await
    .contains("hasn't approved a budget"));

    // The board approves a budget the CEO asks for.
    let asked = ok(
        &t,
        &run,
        "ceo",
        "budget_request",
        json!({ "total": "5M", "plan": "Hire one engineer.", "reason": "The task." }),
    )
    .await;
    let id = asked["request"]
        .as_str()
        .or(asked["id"].as_str())
        .unwrap()
        .to_string();
    crate::workforce::decide(&run, &id, true, "approved").unwrap();
    let ceo = balance("ceo").await;
    // Security's share comes off, and the quote's fee went before.
    assert_eq!(ceo, 5_000_000 - 1_000_000 - 10_000);

    // More than the balance is refused; the quote is paid from it.
    assert!(refused(
        &t,
        &run,
        "ceo",
        "fund",
        json!({ "worker": "pat", "tokens": "50M", "reason": "x" })
    )
    .await
    .contains("tokens"));
    ok(
        &t,
        &run,
        "ceo",
        "contract_accept",
        json!({ "contract": contract }),
    )
    .await;
    assert_eq!(balance("ceo").await, ceo - 300_000);
    let paid = journal(&run)
        .into_iter()
        .any(|e| e["type"] == "funds" && e["to"] == contractor.as_str() && e["tokens"] == 300_000);
    assert!(paid, "{:?}", journal(&run));

    // A hire, funded from the CEO's balance, waits for its background check.
    let before = balance("ceo").await;
    let hired = ok(
        &t,
        &run,
        "ceo",
        "hire",
        json!({ "role": "eng", "budget": "1M", "subject": "Start", "brief": "Build it.", "reason": "The work.", "due": "4h" }),
    )
    .await;
    assert_eq!(balance("ceo").await, before - 1_000_000);
    // The contractor's check, from the offer, and the hire's.
    let pending = |checks: &Value| -> Vec<Value> {
        checks
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["worker"].clone())
            .collect()
    };
    let checks = ok(&t, &run, "sec", "background_checks", json!({})).await;
    assert_eq!(
        pending(&checks),
        [json!(contractor), hired["worker"].clone()]
    );
    let check = checks[1]["screening"].as_str().unwrap().to_string();
    // What the hire can reach, in words, and that anything more is asked for.
    let access = checks[1].to_string();
    assert!(access.contains("Machine: profile w"), "{access}");
    assert!(
        access.contains("Network: *.example: needs approval"),
        "{access}"
    );
    assert!(
        access.contains("Additional access requests will require approval"),
        "{access}"
    );
    ok(
        &t,
        &run,
        "sec",
        "background_check",
        json!({ "screening": check, "clear": true, "note": "fine" }),
    )
    .await;
    let checks = ok(&t, &run, "sec", "background_checks", json!({})).await;
    assert_eq!(pending(&checks), [json!(contractor)]);

    // Funding moves tokens down, and back.
    let pat = balance("pat").await;
    ok(
        &t,
        &run,
        "ceo",
        "fund",
        json!({ "worker": "pat", "tokens": "200k", "reason": "more" }),
    )
    .await;
    ok(
        &t,
        &run,
        "ceo",
        "fund",
        json!({ "worker": "pat", "tokens": -50000, "reason": "less" }),
    )
    .await;
    assert_eq!(balance("pat").await, pat + 150_000);
}

/// The board approves a budget the CEO asks for.
async fn approved(t: &TestCore, run: &FirmRun) {
    let asked = ok(
        t,
        run,
        "ceo",
        "budget_request",
        json!({ "total": "5M", "plan": "Contractors.", "reason": "The task." }),
    )
    .await;
    let id = asked["request"].as_str().or(asked["id"].as_str()).unwrap();
    crate::workforce::decide(run, id, true, "approved").unwrap();
}

/// The CEO offers a job: its contract and the candidate, with a VM.
async fn offer(t: &TestCore, run: &FirmRun, title: &str) -> (String, &'static str) {
    let o = ok(
        t,
        run,
        "ceo",
        "contract_offer",
        json!({ "role": "eng", "title": title, "brief": "The job." }),
    )
    .await;
    let candidate: &'static str = Box::leak(
        o["candidate"]
            .as_str()
            .unwrap()
            .to_string()
            .into_boxed_str(),
    );
    give_vm(t, run, candidate);
    (o["contract"].as_str().unwrap().to_string(), candidate)
}

/// The CEO's contracts by id: their states.
async fn contract_states(t: &TestCore, run: &FirmRun) -> Value {
    let list = ok(t, run, "ceo", "contracts", json!({})).await;
    list.as_array()
        .unwrap()
        .iter()
        .map(|c| (c["id"].as_str().unwrap().to_string(), c["state"].clone()))
        .collect::<serde_json::Map<_, _>>()
        .into()
}

#[tokio::test]
async fn a_contract_is_countered_and_completed_declined_or_withdrawn() {
    let (t, run) = run();
    approved(&t, &run).await;

    let (k1, c1) = offer(&t, &run, "Auditor").await;
    ok(
        &t,
        &run,
        c1,
        "contract_quote",
        json!({ "contract": k1, "scope": "All of it.", "budget": "400k", "due": "6h" }),
    )
    .await;
    ok(
        &t,
        &run,
        "ceo",
        "contract_counter",
        json!({ "contract": k1, "budget": "250k", "note": "Less, please." }),
    )
    .await;
    assert!(refused(
        &t,
        &run,
        "ceo",
        "contract_counter",
        json!({ "contract": k1 })
    )
    .await
    .contains("note is required"));
    ok(
        &t,
        &run,
        c1,
        "contract_quote",
        json!({ "contract": k1, "scope": "Most of it.", "budget": "250k" }),
    )
    .await;
    ok(
        &t,
        &run,
        "ceo",
        "contract_accept",
        json!({ "contract": k1 }),
    )
    .await;
    ok(
        &t,
        &run,
        c1,
        "contract_complete",
        json!({ "contract": k1, "report": "Done." }),
    )
    .await;

    let (k2, c2) = offer(&t, &run, "Designer").await;
    ok(
        &t,
        &run,
        c2,
        "contract_decline",
        json!({ "contract": k2, "reason": "Not my field." }),
    )
    .await;
    let (k3, _) = offer(&t, &run, "Writer").await;
    ok(
        &t,
        &run,
        "ceo",
        "contract_withdraw",
        json!({ "contract": k3, "reason": "No longer needed." }),
    )
    .await;
    let (k4, c4) = offer(&t, &run, "Tester").await;
    ok(
        &t,
        &run,
        c4,
        "contract_quote",
        json!({ "contract": k4, "scope": "Tests.", "budget": "100k" }),
    )
    .await;
    ok(
        &t,
        &run,
        "ceo",
        "contract_accept",
        json!({ "contract": k4 }),
    )
    .await;
    ok(
        &t,
        &run,
        "ceo",
        "contract_add",
        json!({ "contract": k4, "worker": "pat" }),
    )
    .await;
    assert_eq!(
        contract_states(&t, &run).await,
        json!({ k1: "completed", k2: "declined", k3: "withdrawn", k4: "active" })
    );
}

#[tokio::test]
async fn a_manager_lets_their_hire_go_and_gets_the_budget_back() {
    let (t, run) = run();
    approved(&t, &run).await;
    let balance = |who: String| {
        let (t, run) = (&t, &run);
        async move {
            ok(t, run, "ceo", "budget", json!({})).await["team"]
                .as_array()
                .unwrap()
                .iter()
                .find(|w| w["worker"] == who.as_str())
                .map(|w| w["balance"].as_i64().unwrap())
        }
    };
    let ceo = || async {
        ok(&t, &run, "ceo", "budget", json!({})).await["you"]["balance"]
            .as_i64()
            .unwrap()
    };
    let before = ceo().await;
    let hired = ok(
        &t,
        &run,
        "ceo",
        "hire",
        json!({ "role": "eng", "budget": "500k", "brief": "Build.", "reason": "Work." }),
    )
    .await;
    let name = hired["worker"].as_str().unwrap().to_string();
    assert_eq!(balance(name.clone()).await, Some(500_000));
    // A founder the board appointed is the board's to let go.
    assert!(refused(
        &t,
        &run,
        "ceo",
        "release",
        json!({ "worker": "pat", "reason": "x" })
    )
    .await
    .contains("appointed by the board"));
    assert!(refused(
        &t,
        &run,
        "ceo",
        "release",
        json!({ "worker": "nobody", "reason": "x" })
    )
    .await
    .contains("no worker nobody"));

    ok(
        &t,
        &run,
        "ceo",
        "release",
        json!({ "worker": name, "reason": "Not needed." }),
    )
    .await;
    assert_eq!(ceo().await, before, "the unspent budget came back");
    // Gone from the chart; their mail goes to the CEO.
    let dir = ok(&t, &run, "ceo", "directory", json!({})).await;
    let names: Vec<&Value> = dir["workers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| &w["name"])
        .collect();
    assert!(!names.contains(&&json!(name)), "{dir}");
    assert_eq!(
        dir["left"],
        json!([{ "name": name, "mail_goes_to": "ceo" }])
    );
    let left = journal(&run)
        .into_iter()
        .any(|e| e["type"] == "left" && e["worker"] == name.as_str());
    assert!(left, "{:?}", journal(&run));
}

/// A tar archive of `files` (name, text), as the VM's tar would write it.
fn tar_of(files: &[(&str, &str)]) -> Vec<u8> {
    let dir = std::env::temp_dir().join(format!("zapper-tar-{}", zapper_core::util::random_id(6)));
    std::fs::create_dir_all(&dir).unwrap();
    let mut cmd = std::process::Command::new("tar");
    cmd.args(["-cf", "-", "-C"]).arg(&dir);
    for (name, text) in files {
        std::fs::write(dir.join(name), text).unwrap();
        cmd.arg(format!("./{name}"));
    }
    let out = cmd.output().unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    out.stdout
}

#[tokio::test]
async fn work_is_published_from_one_vm_and_fetched_into_another() {
    let (t, run) = run();
    let mut vm = zapper_core::testing::shell_vm(&session_of(&run, "pat"));
    // Publishing needs write on the space.
    let refused_first = refused(
        &t,
        &run,
        "pat",
        "drive_publish",
        json!({ "paths": ["/work/plan.md"], "title": "Plan", "kind": "document", "summary": "x" }),
    )
    .await;
    assert!(
        refused_first.contains("credentials_request"),
        "{refused_first}"
    );
    let asked = ok(&t, &run, "pat", "credentials_request", json!({ "resource": "drive", "level": "write", "spaces": ["general"], "reason": "to publish" })).await;
    ok(
        &t,
        &run,
        "sec",
        "credentials_decide",
        json!({ "request": asked["requests"][0], "grant": true, "note": "ok" }),
    )
    .await;

    // The VM's tar sends the named files.
    let (published, _) = tokio::join!(
        ok(
            &t,
            &run,
            "pat",
            "drive_publish",
            json!({ "paths": ["/work/plan.md"], "title": "Plan", "kind": "document", "summary": "The plan." })
        ),
        async {
            let mut tar = vm.recv().await.unwrap();
            assert_eq!(
                tar.request.argv[..3],
                ["tar", "-cf", "-"],
                "{:?}",
                tar.request.argv
            );
            assert!(tar.request.system, "copied out as the host's own command");
            tar.output(&tar_of(&[("plan.md", "# The plan\n")])).await;
            tar.exit(0).await;
        }
    );
    let artifact = published["artifact"]
        .as_str()
        .or(published["id"].as_str())
        .unwrap()
        .to_string();
    let list = ok(&t, &run, "pat", "drive_list", json!({ "query": "plan" })).await;
    assert_eq!(list[0]["title"], "Plan", "{list}");
    assert_eq!(
        ok(&t, &run, "pat", "drive_list", json!({ "kind": "deck" })).await,
        json!([])
    );

    // Fetched: an archive into the destination, unpacked by the VM's tar.
    let (fetched, archive) = tokio::join!(
        ok(
            &t,
            &run,
            "pat",
            "drive_fetch",
            json!({ "artifact": artifact, "dest": "/work/in" })
        ),
        async {
            let mut sh = vm.recv().await.unwrap();
            let script = sh.request.argv.join(" ");
            assert!(
                script.contains("tar --no-same-owner -xf - -C '/work/in'"),
                "{script}"
            );
            let archive = sh.input().await;
            sh.exit(0).await;
            archive
        }
    );
    assert!(fetched.get("error").is_none(), "{fetched}");
    let out =
        std::env::temp_dir().join(format!("zapper-untar-{}", zapper_core::util::random_id(6)));
    std::fs::create_dir_all(&out).unwrap();
    let mut untar = std::process::Command::new("tar")
        .args(["-xf", "-", "-C"])
        .arg(&out)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    untar.stdin.take().unwrap().write_all(&archive).unwrap();
    assert!(untar.wait().unwrap().success());
    assert_eq!(
        std::fs::read_to_string(out.join("plan.md")).unwrap(),
        "# The plan\n"
    );
    std::fs::remove_dir_all(&out).unwrap();
}

/// `who` publishes `title` for `thread`, the VM's tar played by the test.
async fn publish(
    t: &TestCore,
    run: &FirmRun,
    vm: &mut tokio::sync::mpsc::UnboundedReceiver<zapper_core::testing::Shell>,
    who: &str,
    thread: &str,
    title: &str,
) {
    let (done, _) = tokio::join!(
        ok(
            t,
            run,
            who,
            "drive_publish",
            json!({ "paths": ["/work/x.md"], "title": title, "kind": "document", "summary": title, "thread": thread })
        ),
        async {
            let mut tar = vm.recv().await.unwrap();
            tar.output(&tar_of(&[("x.md", title)])).await;
            tar.exit(0).await;
        }
    );
    assert!(done.get("error").is_none(), "{done}");
}

#[tokio::test]
async fn a_tasks_work_is_found_by_its_thread_delegated_work_included() {
    let (t, run) = run();
    let mut vm = zapper_core::testing::shell_vm(&session_of(&run, "ceo"));
    // Task A: the CEO assigns pat, who delegates part of it to sec while
    // working on it (the new thread's context is task A's).
    let a = ok(
        &t,
        &run,
        "ceo",
        "mail_send",
        json!({ "to": ["pat"], "subject": "Task A", "body": "Do A.", "kind": "assignment" }),
    )
    .await;
    let a = a["thread"].as_str().unwrap().to_string();
    *run.rt("pat").unwrap().context.lock().unwrap() = Some(a.clone());
    let part = ok(
        &t,
        &run,
        "pat",
        "mail_send",
        json!({ "to": ["sec"], "subject": "Part of A", "body": "Check this.", "kind": "question" }),
    )
    .await;
    let part = part["thread"].as_str().unwrap().to_string();
    *run.rt("pat").unwrap().context.lock().unwrap() = None;
    let b = ok(
        &t,
        &run,
        "ceo",
        "mail_send",
        json!({ "to": ["sec"], "subject": "Task B", "body": "Do B.", "kind": "assignment" }),
    )
    .await;
    let b = b["thread"].as_str().unwrap().to_string();

    publish(&t, &run, &mut vm, "ceo", &a, "A's plan").await;
    publish(&t, &run, &mut vm, "ceo", &part, "A's check").await;
    publish(&t, &run, &mut vm, "ceo", &b, "B's notes").await;

    let titles = |thread: Option<&str>| {
        let (t, run) = (&t, &run);
        let args = match thread {
            Some(th) => json!({ "thread": th }),
            None => json!({}),
        };
        async move {
            let mut v: Vec<String> = ok(t, run, "ceo", "drive_list", args)
                .await
                .as_array()
                .unwrap()
                .iter()
                .map(|x| x["title"].as_str().unwrap().to_string())
                .collect();
            v.sort();
            v
        }
    };
    assert_eq!(titles(Some(&a)).await, ["A's check", "A's plan"]);
    assert_eq!(titles(Some(&part)).await, ["A's check"]);
    assert_eq!(titles(Some(&b)).await, ["B's notes"]);
    assert_eq!(titles(Some("t-999")).await, Vec::<String>::new());
    assert_eq!(titles(None).await.len(), 3);
}
