//! The board's mail through the office's routes, as the UI and the CLI
//! use them: sending, its inbox and anyone's, a thread, marking mail read.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;
use zapper_core::prelude::*;
use zapper_core::testing::TestCore;

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
";

async fn call(t: &TestCore, method: Method, uri: &str, body: Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::from(body.to_string()))
        .unwrap();
    let r = crate::routes(&t.core).oneshot(req).await.unwrap();
    let status = r.status();
    let bytes = http_body_util::BodyExt::collect(r.into_body())
        .await
        .unwrap()
        .to_bytes();
    let text = String::from_utf8_lossy(&bytes).into_owned();
    (
        status,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

async fn get(t: &TestCore, uri: &str) -> Value {
    let (status, v) = call(t, Method::GET, uri, Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{uri}: {v}");
    v
}

fn unread(inbox: &Value) -> usize {
    inbox
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["unread"] == json!(true))
        .count()
}

#[tokio::test]
async fn the_board_mails_reads_its_inbox_and_threads_and_marks_them_read() {
    let t = crate::test_core(FIRM);
    let run = crate::test_run(&t.core, "acme");
    let (status, sent) = call(
        &t,
        Method::POST,
        "/firms/acme/mail",
        json!({ "to": ["ceo"], "subject": "The task", "body": "Build a todo app.", "kind": "assignment" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{sent}");
    let thread = sent["thread"].as_str().unwrap().to_string();

    // The CEO answers in the thread, to the board.
    let mut reply = crate::mail::Draft::new(
        "ceo",
        vec!["board".into()],
        "",
        "On it.",
        crate::mail::Kind::Reply,
    );
    reply.in_reply_to = Some(sent["id"].as_str().unwrap().to_string());
    run.send(reply).unwrap();

    let inbox = get(&t, "/firms/acme/mail").await;
    assert_eq!(unread(&inbox), 1, "{inbox}");
    assert_eq!(
        get(&t, "/firms/acme/mail?unread=true")
            .await
            .as_array()
            .unwrap()
            .len(),
        1
    );
    // Anyone's inbox, for the board to look into.
    let ceos = get(&t, "/firms/acme/mail?for=ceo").await;
    assert_eq!(ceos[0]["subject"], "The task", "{ceos}");

    let t1 = get(&t, &format!("/firms/acme/threads/{thread}")).await;
    let bodies: Vec<&Value> = t1["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| &m["body"])
        .collect();
    assert_eq!(
        bodies,
        [&json!("Build a todo app."), &json!("On it.")],
        "{t1}"
    );
    let (status, _) = call(&t, Method::GET, "/firms/acme/threads/t-99", Value::Null).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let ids: Vec<Value> = inbox
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].clone())
        .collect();
    let (status, v) = call(
        &t,
        Method::POST,
        "/firms/acme/mail/read",
        json!({ "ids": ids }),
    )
    .await;
    assert_eq!((status, v), (StatusCode::OK, json!({ "ok": true })));
    assert_eq!(unread(&get(&t, "/firms/acme/mail").await), 0);
    // A firm that isn't running has no mail.
    let (status, _) = call(&t, Method::GET, "/firms/other/mail", Value::Null).await;
    assert_ne!(status, StatusCode::OK);
}

/// The board promotes a standing approval into the running config: the
/// rule goes to the profile of the role it covers, under a fresh id, and is
/// journaled; the startup file is untouched until `config save`; what isn't
/// a standing approval isn't promoted.
#[tokio::test]
async fn the_board_promotes_a_decision_into_the_running_config() {
    let t = crate::test_core(FIRM);
    let run = crate::test_run(&t.core, "acme");
    let req = zapper_policy::NetRequest {
        host: "docs.example".into(),
        port: 443,
        method: None,
        path: None,
    };
    let id = crate::security::ask(&run, "pat", &req, None, None, "the docs").unwrap();
    let (status, v) = call(
        &t,
        Method::POST,
        &format!("/firms/acme/security/requests/{id}"),
        json!({ "allow": true, "scope": "role", "reason": "engineers read docs" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let standing = crate::security::standing(&run);
    let rule = standing[0]["id"].as_str().unwrap().to_string();
    let startup = std::fs::read_to_string(t.core.startup_path()).unwrap();

    let (status, v) = call(
        &t,
        Method::POST,
        &format!("/firms/acme/security/rules/{rule}/promote"),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["scope"], "profile:w", "{v}");
    let added = v["rule"]["id"].as_str().unwrap().to_string();
    let config = t.core.config();
    let w = &config.profiles["w"].policy.net;
    let r = w
        .iter()
        .find(|r| r.id.as_deref() == Some(added.as_str()))
        .expect("in the running config");
    assert_eq!(
        (r.host.as_str(), r.action),
        ("docs.example", zapper_policy::Action::Allow)
    );
    assert_eq!(
        std::fs::read_to_string(t.core.startup_path()).unwrap(),
        startup,
        "not saved"
    );
    let promoted = std::fs::read_to_string(run.dir.join("journal.jsonl"))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .any(|e| {
            e["type"] == "access" && e["event"] == "promoted" && e["config_rule"] == added.as_str()
        });
    assert!(promoted);

    let (status, _) = call(
        &t,
        Method::POST,
        "/firms/acme/security/rules/a-net-99/promote",
        Value::Null,
    )
    .await;
    assert!(status.is_client_error(), "{status}");
}
