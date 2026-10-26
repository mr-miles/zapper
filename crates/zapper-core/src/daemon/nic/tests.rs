//! The guest's DNS server over a core without VMs: what the session's
//! queries are answered with, by name.

use std::net::{Ipv4Addr, Ipv6Addr};
use std::sync::Arc;

use super::query_dns;
use crate::testing::TestCore;
use crate::{NewSession, Session};

/// A query for `name` of record type `rtype` (A = 1, AAAA = 28).
fn query_type(name: &str, rtype: u8) -> Vec<u8> {
    let mut q = vec![0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
    for label in name.split('.') {
        q.push(label.len() as u8);
        q.extend_from_slice(label.as_bytes());
    }
    q.extend_from_slice(&[0, 0, rtype, 0, 1]);
    q
}

fn query(name: &str) -> Vec<u8> {
    query_type(name, 1)
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

/// The answer to an AAAA query for `name`.
async fn ask6(t: &TestCore, s: &Arc<Session>, name: &str) -> Result<Ipv6Addr, u8> {
    let r = query_dns(t.core.daemon(), s, &query_type(name, 28))
        .await
        .unwrap();
    let rcode = r[3] & 0x0f;
    if rcode != 0 {
        return Err(rcode);
    }
    Ok(Ipv6Addr::from(
        <[u8; 16]>::try_from(&r[r.len() - 16..]).unwrap(),
    ))
}

const REFUSED: u8 = 5;
const SERVFAIL: u8 = 2;
const NXDOMAIN: u8 = 3;

/// Registers `db.internal` as a service for `s` so its name resolves without
/// a real host lookup, and returns the session.
async fn with_db_service(config: &str) -> (TestCore, Arc<Session>) {
    let t = TestCore::new(config);
    let s = t
        .core
        .test_session(
            NewSession::plain("web".into(), t.workdir(), Default::default()),
            true,
        )
        .unwrap();
    t.core
        .add_service(crate::ServiceSpec {
            name: "db.internal".into(),
            port: 5432,
            sessions: Default::default(),
            target: crate::ServiceTarget::Session {
                id: "abcdef".into(),
                port: 5432,
            },
            owner: "test".into(),
        })
        .unwrap();
    (t, s)
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

#[tokio::test]
async fn a_service_resolves_only_for_the_sessions_it_picks() {
    let t = TestCore::new("profiles:\n  web:\n    policy:\n      net:\n        - { action: allow, host: db.internal }\n");
    let session = |team: &str| {
        let mut new = NewSession::plain("web".into(), t.workdir(), Default::default());
        new.labels = [("team".to_string(), team.to_string())].into();
        t.core.test_session(new, true).unwrap()
    };
    let (a, b) = (session("a"), session("b"));
    t.core
        .add_service(crate::ServiceSpec {
            name: "db.internal".into(),
            port: 5432,
            sessions: [("team".to_string(), "a".to_string())].into(),
            target: crate::ServiceTarget::Session {
                id: "abcdef".into(),
                port: 5432,
            },
            owner: "test".into(),
        })
        .unwrap();
    assert!(ask(&t, &a, "db.internal").await.is_ok());
    assert!(
        ask(&t, &b, "db.internal").await.is_err(),
        "the host's to say"
    );
}

#[tokio::test]
async fn a_and_aaaa_get_synthetic_addresses_of_their_family() {
    let (t, s) = with_db_service(
        "profiles:\n  web:\n    policy:\n      net:\n        - { action: allow, host: db.internal }\n",
    )
    .await;
    // A gives a 198.18.0.0/15 address, AAAA a 2001:2::/48 one, for one name.
    let v4 = ask(&t, &s, "db.internal").await.unwrap();
    assert_eq!(&v4.octets()[..2], &[198, 18], "{v4} is in 198.18.0.0/15");
    let v6 = ask6(&t, &s, "db.internal").await.unwrap();
    assert_eq!(
        &v6.segments()[..2],
        &[0x2001, 0x2],
        "{v6} is in 2001:2::/48"
    );
}

#[tokio::test]
async fn a_flood_of_queries_fails_closed_with_servfail() {
    let t = TestCore::new("profiles:\n  web: {}\n");
    let s = t
        .core
        .test_session(
            NewSession::plain("web".into(), t.workdir(), Default::default()),
            true,
        )
        .unwrap();
    // The per-session rate limiter must start refusing (SERVFAIL) under a
    // flood rather than resolving without bound. (A name no rule allows is
    // REFUSED, but the rate check comes first, so it still spends a token.)
    let mut saw_servfail = false;
    for _ in 0..200 {
        if ask(&t, &s, "example.com").await == Err(SERVFAIL) {
            saw_servfail = true;
            break;
        }
    }
    assert!(saw_servfail, "a flood must hit the rate limit and SERVFAIL");
}

#[tokio::test]
async fn a_non_address_query_for_an_allowed_name_is_nodata() {
    let (t, s) = with_db_service(
        "profiles:\n  web:\n    policy:\n      net:\n        - { action: allow, host: db.internal }\n",
    )
    .await;
    // An MX query (type 15) for a name that resolves gets NOERROR with no
    // answer record — a synthetic address is only ever an A or AAAA.
    let r = query_dns(t.core.daemon(), &s, &query_type("db.internal", 15))
        .await
        .unwrap();
    assert_eq!(r[3] & 0x0f, 0, "NOERROR");
    assert_eq!((r[6], r[7]), (0, 0), "no answer record (NODATA)");
}

#[tokio::test]
async fn an_allowed_name_in_a_zone_that_nothing_serves_is_nxdomain() {
    // A zone answers from the daemon, never the host's resolver: a name under
    // it that no service serves is NXDOMAIN, not a leaked host lookup.
    let t = TestCore::new(
        "profiles:\n  web:\n    policy:\n      net:\n        - { action: allow, host: \"*.acme.internal\" }\n",
    );
    let s = t
        .core
        .test_session(
            NewSession::plain("web".into(), t.workdir(), Default::default()),
            true,
        )
        .unwrap();
    t.core
        .add_zone("acme.internal", Default::default(), "test")
        .unwrap();
    assert_eq!(ask(&t, &s, "nothere.acme.internal").await, Err(NXDOMAIN));
}
