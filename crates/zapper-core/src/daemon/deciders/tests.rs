//! Deciders through the proxy: which sessions' `ask`s they get, what the
//! session is told, and the operator's again once a decider goes.

use crate::prelude::*;
use std::sync::{Arc, Mutex};

use anyhow::bail;
use tokio::io::AsyncReadExt;
use zapper_policy::NetRequest;

use super::{Decider, Referral};
use crate::testing::TestCore;
use crate::testing::{dial, net_event};
use crate::{NewSession, Session};

/// Refers every `ask` to "the bot" as request `b-N`, or refuses them all.
#[derive(Default)]
struct Bot {
    refuse: bool,
    asked: Mutex<Vec<(String, String)>>,
    allowed: Mutex<Vec<String>>,
}

impl Decider for Bot {
    fn ask<'a>(
        &'a self,
        s: &'a Session,
        req: &'a NetRequest,
    ) -> futures_util::future::BoxFuture<'a, anyhow::Result<Referral>> {
        Box::pin(async move {
            if self.refuse {
                bail!("the bot is away");
            }
            let mut asked = self.asked.lock().unwrap();
            asked.push((s.id.clone(), req.host.clone()));
            Ok(Referral {
                id: format!("b-{}", asked.len()),
                to: "the bot".into(),
                then: "retry when it says so".into(),
            })
        })
    }

    fn allowed<'a>(&'a self, _s: &'a Session, rule: &'a str) -> futures_util::future::BoxFuture<'a, ()> {
        Box::pin(async move {
            self.allowed.lock().unwrap().push(rule.to_string());
        })
    }
}

const CONFIG: &str = "
profiles:
  web:
    policy:
      net:
        - { id: ok, action: allow, host: ok.example }
        - { action: ask, host: '*.example' }
";

fn session(t: &TestCore, team: &str) -> Arc<Session> {
    let mut new = NewSession::plain("web".into(), t.workdir(), Default::default());
    new.labels = [("team".to_string(), team.to_string())].into();
    t.core.test_session(new, true).unwrap()
}

fn team(name: &str) -> std::collections::BTreeMap<String, String> {
    [("team".to_string(), name.to_string())].into()
}

/// Waits for an operator's approval to be pending.
async fn pending(t: &TestCore) -> usize {
    for _ in 0..200 {
        let n = t.core.approvals().len();
        if n > 0 {
            return n;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    0
}

#[tokio::test]
async fn a_decider_gets_its_sessions_asks_and_they_are_refused_at_once() {
    let t = TestCore::new(CONFIG);
    let a = session(&t, "a");
    let bot = Arc::new(Bot::default());
    t.core.add_decider(team("a"), bot.clone());

    let _io = dial(&t, &a, "api.example", 443);
    let e = net_event(&t, &a, 0).await;
    assert_eq!(e["action"], "deny", "{e}");
    assert_eq!(
        bot.asked.lock().unwrap().as_slice(),
        [(a.id.clone(), "api.example".to_string())]
    );
    let log = a.requests.lock().unwrap().list();
    assert_eq!(log[0]["error"], "access requested from the bot (b-1)");
    assert!(t.core.approvals().is_empty(), "not the operator's");

    // Allowed requests: the decider hears which rule allowed them.
    let _io = dial(&t, &a, "ok.example", 443);
    net_event(&t, &a, 1).await;
    assert_eq!(bot.allowed.lock().unwrap().as_slice(), ["ok"]);
}

/// The proxy's answer to `s`'s `CONNECT target`, as the guest reads it.
async fn connect(t: &TestCore, s: &Arc<Session>, target: &str) -> String {
    use tokio::io::AsyncWriteExt;
    let (mut guest, host) = tokio::net::UnixStream::pair().unwrap();
    tokio::spawn(crate::daemon::proxy::serve(
        t.core.daemon().clone(),
        s.clone(),
        host,
    ));
    guest
        .write_all(format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut reply = vec![0u8; 1024];
    let n = guest.read(&mut reply).await.unwrap();
    String::from_utf8_lossy(&reply[..n]).into_owned()
}

#[tokio::test]
async fn the_guest_is_told_where_its_request_went() {
    let t = TestCore::new(CONFIG);
    let a = session(&t, "a");
    t.core.add_decider(team("a"), Arc::new(Bot::default()));
    let reply = connect(&t, &a, "api.example:443").await;
    assert!(reply.starts_with("HTTP/1.1 403"), "{reply}");
    assert!(
        reply.contains(
            "access to api.example:443 requested from the bot (request b-1); retry when it says so"
        ),
        "{reply}"
    );
}

#[tokio::test]
async fn a_decider_that_fails_refuses_as_a_denial() {
    let t = TestCore::new(CONFIG);
    let a = session(&t, "a");
    t.core.add_decider(
        team("a"),
        Arc::new(Bot {
            refuse: true,
            ..Default::default()
        }),
    );
    let reply = connect(&t, &a, "api.example:443").await;
    assert!(reply.starts_with("HTTP/1.1 403"), "{reply}");
    assert!(reply.contains("api.example:443 denied by "), "{reply}");
    assert_eq!(net_event(&t, &a, 0).await["action"], "deny");
    assert!(t.core.approvals().is_empty());
}

#[tokio::test]
async fn the_earliest_decider_for_a_session_decides_until_it_goes() {
    let t = TestCore::new(CONFIG);
    let (a, b) = (session(&t, "a"), session(&t, "b"));
    let first = Arc::new(Bot::default());
    let all = Arc::new(Bot::default());
    let id = t.core.add_decider(team("a"), first.clone());
    t.core.add_decider(Default::default(), all.clone());

    let _io = dial(&t, &a, "x.example", 443);
    net_event(&t, &a, 0).await;
    let _io = dial(&t, &b, "x.example", 443);
    net_event(&t, &b, 0).await;
    assert_eq!(first.asked.lock().unwrap().len(), 1);
    assert_eq!(all.asked.lock().unwrap().len(), 1, "every session's");

    assert!(t.core.remove_decider(id));
    assert!(!t.core.remove_decider(id));
    let _io = dial(&t, &a, "x.example", 443);
    net_event(&t, &a, 1).await;
    assert_eq!(all.asked.lock().unwrap().len(), 2, "the next one's now");
}

#[tokio::test]
async fn with_no_decider_the_operator_decides() {
    let t = TestCore::new(CONFIG);
    let a = session(&t, "a");
    let id = t.core.add_decider(team("a"), Arc::new(Bot::default()));
    t.core.remove_decider(id);
    let _io = dial(&t, &a, "api.example", 443);
    assert_eq!(pending(&t).await, 1);
}

// A decider served on a socket (`add_decider_socket`), as the office will be
// once it is a process of its own.

/// A decider on socket `name` in `state/services/`: it refers every `ask` to
/// "the bot" as `b-N` (or refuses, when `refuse`), notes nothing for an
/// `allowed`, and sends every call it gets to the receiver.
fn decider_socket(
    t: &TestCore,
    name: &str,
    refuse: bool,
) -> tokio::sync::mpsc::UnboundedReceiver<zapper_policy::facility::AskCall> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use zapper_policy::facility::{AskCall, AskMethod, AskReply, Referral};
    let listener = tokio::net::UnixListener::bind(t.core.socket_dir().join(name)).unwrap();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let (tx, seen) = (tx.clone(), seen.clone());
            tokio::spawn(async move {
                let (r, mut w) = stream.into_split();
                let mut line = String::new();
                if BufReader::new(r).read_line(&mut line).await.is_err() {
                    return;
                }
                let Ok(call) = serde_json::from_str::<AskCall>(&line) else {
                    return;
                };
                let _ = tx.send(call.clone());
                if matches!(call.method, AskMethod::Allowed) {
                    return; // a note, not answered
                }
                let reply = if refuse {
                    AskReply {
                        referral: None,
                        error: Some("the bot is away".into()),
                    }
                } else {
                    let n = seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                    AskReply {
                        referral: Some(Referral {
                            id: format!("b-{n}"),
                            to: "the bot".into(),
                            then: "retry when it says so".into(),
                        }),
                        error: None,
                    }
                };
                let mut b = serde_json::to_vec(&reply).unwrap();
                b.push(b'\n');
                let _ = w.write_all(&b).await;
            });
        }
    });
    rx
}

#[tokio::test]
async fn a_socket_decider_refers_an_ask_and_hears_what_a_rule_allowed() {
    let t = TestCore::new(CONFIG);
    let a = session(&t, "a");
    let mut calls = decider_socket(&t, "decider.sock", false);
    t.core.add_decider_socket(team("a"), "decider.sock").unwrap();

    // An ask is referred over the socket; the operator is not asked.
    let _io = dial(&t, &a, "api.example", 443);
    assert_eq!(net_event(&t, &a, 0).await["action"], "deny");
    let log = a.requests.lock().unwrap().list();
    assert_eq!(log[0]["error"], "access requested from the bot (b-1)");
    assert!(t.core.approvals().is_empty(), "not the operator's");
    let ask = calls.recv().await.unwrap();
    assert_eq!(
        (ask.session.as_str(), ask.host.as_str(), ask.port),
        (a.id.as_str(), "api.example", 443)
    );
    assert!(matches!(ask.method, zapper_policy::facility::AskMethod::Ask));

    // An allowed request: the decider is told which rule allowed it.
    let _io = dial(&t, &a, "ok.example", 443);
    net_event(&t, &a, 1).await;
    let note = calls.recv().await.unwrap();
    assert!(matches!(
        note.method,
        zapper_policy::facility::AskMethod::Allowed
    ));
    assert_eq!(note.rule.as_deref(), Some("ok"));
}

#[tokio::test]
async fn a_socket_decider_tells_the_guest_where_its_request_went() {
    let t = TestCore::new(CONFIG);
    let a = session(&t, "a");
    let _calls = decider_socket(&t, "decider.sock", false);
    t.core.add_decider_socket(team("a"), "decider.sock").unwrap();
    let reply = connect(&t, &a, "api.example:443").await;
    assert!(reply.starts_with("HTTP/1.1 403"), "{reply}");
    assert!(
        reply.contains(
            "access to api.example:443 requested from the bot (request b-1); retry when it says so"
        ),
        "{reply}"
    );
}

#[tokio::test]
async fn a_socket_decider_that_is_away_refuses_as_a_denial() {
    let t = TestCore::new(CONFIG);
    let a = session(&t, "a");
    let _calls = decider_socket(&t, "d2.sock", true);
    t.core.add_decider_socket(team("a"), "d2.sock").unwrap();
    let reply = connect(&t, &a, "api.example:443").await;
    assert!(reply.starts_with("HTTP/1.1 403"), "{reply}");
    assert!(reply.contains("api.example:443 denied by "), "{reply}");
    assert!(t.core.approvals().is_empty());
}
