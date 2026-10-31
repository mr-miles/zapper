//! Who decides a session's `ask`s, through the proxy: the operator, as an
//! approval the connection waits for. A firm's officers, as a registered
//! decider, are the office's tests.

use crate::prelude::*;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::testing::{dial, net_event, TestCore};
use crate::{NewSession, Session};

/// Echoes what it is sent.
struct Echo;

impl crate::ServiceHandler for Echo {
    fn serve(&self, _s: Arc<Session>, mut io: Box<dyn crate::ServiceIo>) {
        tokio::spawn(async move {
            let mut buf = [0u8; 64];
            if let Ok(n) = io.read(&mut buf).await {
                let _ = io.write_all(&buf[..n]).await;
            }
        });
    }
}

#[tokio::test]
async fn an_ask_waits_for_the_operators_answer() {
    let t = TestCore::new(
        "profiles:\n  web:\n    policy:\n      net:\n        - { action: ask, host: echo.internal }\n",
    );
    let s = t
        .core
        .test_session(
            NewSession::plain("web".into(), t.workdir(), Default::default()),
            true,
        )
        .unwrap();
    t.core
        .add_service(crate::ServiceSpec {
            name: "echo.internal".into(),
            port: 7,
            sessions: Default::default(),
            target: crate::ServiceTarget::Handler {
                tls: false,
                handler: Arc::new(Echo),
            },
            owner: "test".into(),
        })
        .unwrap();
    let mut io = dial(&t, &s, "echo.internal", 7);
    let pending = loop {
        if let Some(p) = t.core.approvals().into_iter().next() {
            break p;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    };
    let p = serde_json::to_value(&pending).unwrap();
    assert_eq!(p["subject"], "connect echo.internal:7", "{p}");
    t.core
        .answer(p["id"].as_str().unwrap(), true, None)
        .unwrap();
    io.write_all(b"hi").await.unwrap();
    let mut buf = [0u8; 2];
    io.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"hi");
    assert_eq!(net_event(&t, &s, 0).await["action"], "allow");
}

/// A session's approvals go when it stops, refused, and so does one whose
/// asker is dropped: none stays listed with nobody waiting on it.
#[tokio::test]
async fn approvals_go_with_their_session_and_their_asker() {
    let t = TestCore::new("profiles:\n  web: {}\n");
    let s = t
        .core
        .test_session(
            NewSession::plain("web".into(), t.workdir(), Default::default()),
            true,
        )
        .unwrap();
    let ask = |subject: &str| {
        let (d, s, subject) = (t.core.daemon().clone(), s.clone(), subject.to_string());
        tokio::spawn(async move {
            let rule =
                serde_yaml::from_str("{ kind: net, action: allow, host: x.example }").unwrap();
            d.ask(&s, zapper_policy::Kind::Net, subject, rule).await
        })
    };
    let listed = || async {
        for _ in 0..100 {
            if !t.core.approvals().is_empty() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("never listed");
    };

    let dropped = ask("connect a.example:443");
    listed().await;
    dropped.abort();
    let _ = dropped.await;
    assert!(t.core.approvals().is_empty(), "unlisted with its asker");

    let waiting = ask("connect b.example:443");
    listed().await;
    crate::daemon::session::stop(t.core.daemon(), &s, "stopped").await;
    assert!(t.core.approvals().is_empty(), "unlisted with its session");
    assert!(!waiting.await.unwrap(), "refused");
}
