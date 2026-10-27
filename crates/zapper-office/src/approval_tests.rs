//! Who decides a firm session's `ask`s, through the proxy: the firm's
//! security officers get an access request while the connection is refused
//! at once, above `final` rules; a session of no firm asks the operator; and
//! once the firm stops, its sessions' asks are the operator's again.

use std::sync::Arc;
use zapper_core::prelude::*;

use zapper_core::testing::{dial, net_event, net_events};
use zapper_core::{NewSession, Session};

use crate::security::{self, DecideArgs};
use crate::{test_core, test_run, FirmRun};

const FIRM: &str = "
policy:
  net:
    - { action: deny, host: forbidden.example, final: true }
profiles:
  w:
    agent: { kind: claude-code }
    policy:
      net:
        - { action: ask, host: '*.example' }
firms:
  acme:
    roles:
      ceo: { profile: w, agent: ceo }
      eng: { profile: w, agent: software-engineer }
    workers:
      - { name: ceo, role: ceo }
      - { name: pat, role: eng, reports_to: ceo }
";

fn worker(run: &FirmRun, name: &str) -> Arc<Session> {
    run.rt(name)
        .unwrap()
        .session
        .lock()
        .unwrap()
        .clone()
        .unwrap()
}

fn requests(run: &FirmRun) -> Vec<serde_json::Value> {
    security::requests(run, false)
}

#[tokio::test]
async fn a_workers_ask_is_refused_at_once_and_goes_to_security() {
    let t = test_core(FIRM);
    let run = test_run(&t.core, "acme");
    let pat = worker(&run, "pat");
    let _io = dial(&t, &pat, "api.example", 443);
    let e = net_event(&t, &pat, 0).await;
    assert_eq!(e["action"], "deny", "{e}");
    assert!(t.core.approvals().is_empty(), "not the operator's");
    let r = requests(&run);
    assert_eq!(r.len(), 1, "{r:?}");
    assert_eq!(r[0]["worker"], "pat");
    assert_eq!(r[0]["host"], "api.example");
    assert_eq!(r[0]["port"], 443);
    let id = r[0]["id"].as_str().unwrap().to_string();
    // What the request log says of it.
    let log = pat.requests();
    let why = log[0]["error"].as_str().unwrap_or_default().to_string();
    assert_eq!(why, format!("access requested from security ({id})"));

    // Asking again is the same request, attempted twice.
    let _io = dial(&t, &pat, "api.example", 443);
    net_event(&t, &pat, 1).await;
    let r = requests(&run);
    assert_eq!(r.len(), 1);
    assert_eq!(r[0]["attempts"], 2);

    // `final` rules are above the officers: nothing to ask.
    let _io = dial(&t, &pat, "forbidden.example", 443);
    net_event(&t, &pat, 2).await;
    assert_eq!(requests(&run).len(), 1);
}

#[tokio::test]
async fn an_approval_for_once_is_used_up_by_the_next_request() {
    let t = test_core(FIRM);
    let run = test_run(&t.core, "acme");
    let pat = worker(&run, "pat");
    let _io = dial(&t, &pat, "api.example", 443);
    net_event(&t, &pat, 0).await;
    let id = requests(&run)[0]["id"].as_str().unwrap().to_string();
    security::decide(
        &run,
        crate::firm::BOARD,
        &id,
        DecideArgs {
            allow: true,
            scope: "once".into(),
            reason: "for the test".into(),
            methods: None,
            path: None,
            expires_in: None,
        },
    )
    .unwrap();
    let standing = security::standing(&run);
    assert_eq!(standing.len(), 1);
    let rule = standing[0]["id"].clone();

    // Allowed by the approval, which goes with it (the host knows
    // nothing of the name, so the connection itself then fails).
    let _io = dial(&t, &pat, "api.example", 443);
    let e = net_event(&t, &pat, 1).await;
    assert_eq!(e["rule"], rule, "{e}");
    assert!(e["error"].as_str().unwrap().starts_with("resolving"), "{e}");
    assert!(security::standing(&run).is_empty());
    let _io = dial(&t, &pat, "api.example", 443);
    assert_eq!(net_event(&t, &pat, 2).await["action"], "deny");
}

#[tokio::test]
async fn a_session_of_no_firm_asks_the_operator() {
    let t = test_core(FIRM);
    let _run = test_run(&t.core, "acme");
    let plain = t
        .core
        .test_session(
            NewSession::plain("w".into(), t.workdir(), Default::default()),
            true,
        )
        .unwrap();
    let _io = dial(&t, &plain, "api.example", 443);
    for _ in 0..200 {
        if !t.core.approvals().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(t.core.approvals().len(), 1);
    assert!(net_events(&t, &plain).is_empty(), "still waiting");
}

#[tokio::test]
async fn once_the_firm_stops_its_asks_are_the_operators_again() {
    let t = test_core(FIRM);
    let run = test_run(&t.core, "acme");
    security::unregister(&t.core, &run);
    let pat = worker(&run, "pat");
    let _io = dial(&t, &pat, "api.example", 443);
    for _ in 0..200 {
        if !t.core.approvals().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(t.core.approvals().len(), 1);
    assert!(requests(&run).is_empty());
}
