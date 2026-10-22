//! Who decides a session's `ask`s, through the proxy: the operator, as an
//! approval the connection waits for, or a firm's security officers, who
//! get an access request while the connection is refused at once. `final`
//! rules are above both.

use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::service_tests::{dial, net_event};
use crate::testing::TestCore;
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

#[cfg(feature = "office")]
mod firm {
    use super::*;
    use crate::daemon::office::security::{self, DecideArgs};
    use crate::daemon::office::{test_run, FirmRun};
    use crate::daemon::proxy::service_tests::net_events;

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
      ceo: { profile: w, skills: { work: ceo } }
      eng: { profile: w, skills: { work: software-engineer } }
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
        let t = TestCore::new(FIRM);
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
        let log = pat.requests.lock().unwrap().list();
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
        let t = TestCore::new(FIRM);
        let run = test_run(&t.core, "acme");
        let pat = worker(&run, "pat");
        let _io = dial(&t, &pat, "api.example", 443);
        net_event(&t, &pat, 0).await;
        let id = requests(&run)[0]["id"].as_str().unwrap().to_string();
        security::decide(
            &run,
            zapper_policy::BOARD,
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
        let t = TestCore::new(FIRM);
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
}
