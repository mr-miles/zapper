//! The office port's requests, as a worker's VM makes them.

use std::sync::Arc;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use zapper_policy::Policy;

use super::super::run_tests::{run, session_of};
use crate::daemon::session::{NewSession, Session};
use crate::daemon::testing::TestDaemon;

/// One request over the office port, as `s`'s VM would make it.
async fn office(t: &TestDaemon, s: &Arc<Session>, request: &str) -> Value {
    let (host, mut guest) = UnixStream::pair().unwrap();
    let serve = tokio::spawn(super::handle(t.d.clone(), s.clone(), host));
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
    old.labels = super::super::labels("acme", "an-earlier-run", "pat", true);
    let old = t.test_session(old, true).unwrap();
    let r = office(&t, &old, r#"{"op":"directory"}"#).await;
    assert_eq!(r["error"], "the firm isn't running");
}
