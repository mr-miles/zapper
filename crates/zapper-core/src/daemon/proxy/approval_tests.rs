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
