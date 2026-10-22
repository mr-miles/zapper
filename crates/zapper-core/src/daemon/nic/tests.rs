//! The guest's DNS server over a core without VMs: what the session's
//! queries are answered with, by name.

use std::net::Ipv4Addr;
use std::sync::Arc;

use super::query_dns;
use crate::testing::TestCore;
use crate::{NewSession, Session};

/// An A query for `name`.
fn query(name: &str) -> Vec<u8> {
    let mut q = vec![0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
    for label in name.split('.') {
        q.push(label.len() as u8);
        q.extend_from_slice(label.as_bytes());
    }
    q.extend_from_slice(&[0, 0, 1, 0, 1]);
    q
}

/// The answer to an A query for `name`: `noerror` with the address,
/// or the response code.
async fn ask(t: &TestCore, s: &Arc<Session>, name: &str) -> Result<Ipv4Addr, u8> {
    let r = query_dns(t.core.daemon(), s, &query(name)).await.unwrap();
    let rcode = r[3] & 0x0f;
    if rcode != 0 {
        return Err(rcode);
    }
    let a = &r[r.len() - 4..];
    Ok(Ipv4Addr::new(a[0], a[1], a[2], a[3]))
}

#[cfg(feature = "office")]
const NXDOMAIN: u8 = 3;
const REFUSED: u8 = 5;

#[cfg(feature = "office")]
const FIRM: &str = "
profiles:
  w:
    agent: { kind: claude-code }
    policy:
      net:
        - { action: allow, host: '*.acme.internal' }
        - { action: allow, host: acme.internal }
firms:
  acme:
    roles:
      ceo: { profile: w, skills: { work: ceo } }
    workers:
      - { name: ceo, role: ceo }
    git: { repos: [api] }
";

#[cfg(feature = "office")]
#[tokio::test]
async fn a_firms_names_are_answered_for_its_sessions_never_by_the_host() {
    let t = TestCore::new(FIRM);
    let run = crate::daemon::office::test_run(&t.core, "acme");
    let ceo = run
        .rt("ceo")
        .unwrap()
        .session
        .lock()
        .unwrap()
        .clone()
        .unwrap();
    let d = run.domain.clone();
    assert_eq!(d, "acme.internal");

    // The host has no such names: an address means the daemon answered.
    let git = ask(&t, &ceo, &format!("git.{d}")).await.unwrap();
    assert_eq!(git.octets()[..2], [198, 18], "{git}");
    assert_eq!(ask(&t, &ceo, &format!("nope.{d}")).await, Err(NXDOMAIN));
    assert_eq!(ask(&t, &ceo, &d).await, Err(NXDOMAIN));
    // Until the cluster boots, its name isn't served either.
    assert_eq!(ask(&t, &ceo, &format!("cluster.{d}")).await, Err(NXDOMAIN));

    // A session of no firm, with the same rules: the host is asked, and
    // knows nothing of the firm's names.
    let plain = t
        .core
        .test_session(
            NewSession::plain("w".into(), t.workdir(), Default::default()),
            true,
        )
        .unwrap();
    assert!(ask(&t, &plain, &format!("git.{d}")).await.is_err());
}

#[tokio::test]
async fn a_name_no_rule_allows_is_refused() {
    let t = TestCore::new("profiles:\n  web: {}\n");
    let s = t
        .core
        .test_session(
            NewSession::plain("web".into(), t.workdir(), Default::default()),
            true,
        )
        .unwrap();
    assert_eq!(ask(&t, &s, "example.com").await, Err(REFUSED));
}
