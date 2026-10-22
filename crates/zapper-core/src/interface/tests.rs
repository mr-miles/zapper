//! `Core` over a daemon without VMs (`testing::TestCore`).

use std::collections::BTreeMap;

use zapper_policy::Policy;

use crate::daemon::session::Status;
use crate::testing::TestCore;
use crate::NewSession;

fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn new_session(t: &TestCore, profile: &str) -> NewSession {
    NewSession::plain(profile.into(), t.workdir(), Policy::default())
}

#[test]
fn only_a_stopped_session_is_forgotten() {
    let t = TestCore::new("profiles:\n  web: {}\n");
    let running = t.core.test_session(new_session(&t, "web"), true).unwrap();
    let starting = t.core.test_session(new_session(&t, "web"), false).unwrap();
    for s in [&running, &starting] {
        let e = t.core.forget_session(&s.id).err().unwrap().to_string();
        assert!(e.contains("still running"), "{e}");
        assert!(t.core.session(&s.id).is_ok(), "still listed");
    }
    running
        .status
        .send_replace(Status::Stopped("stopped by test".into()));
    t.core.forget_session(&running.id).unwrap();
    assert!(t.core.session(&running.id).is_err());
    starting.status.send_replace(Status::Failed("no VM".into()));
    t.core.forget_session(&starting.id).unwrap();
    assert!(t.core.sessions().is_empty());
}

#[test]
fn labels_are_in_the_info_and_find_sessions() {
    let t = TestCore::new("profiles:\n  web: {}\n");
    let mut a = new_session(&t, "web");
    a.labels = labels(&[("firm", "acme"), ("member", "pat"), ("office", "worker")]);
    let a = t.core.test_session(a, true).unwrap();
    let mut b = new_session(&t, "web");
    b.labels = labels(&[("team", "blue")]);
    let b = t.core.test_session(b, true).unwrap();

    let i = t.core.session_info(&a);
    assert_eq!(i.firm.as_deref(), Some("acme"));
    assert_eq!(i.member.as_deref(), Some("pat"));
    assert_eq!(i.labels["office"], "worker");
    assert!(i.driver.is_null(), "not driven");
    assert!(t.core.session_info(&b).firm.is_none());

    let ids = |k: &str, v: &str| -> Vec<String> {
        t.core
            .sessions_labelled(k, v)
            .iter()
            .map(|s| s.id.clone())
            .collect()
    };
    assert_eq!(ids("firm", "acme"), std::slice::from_ref(&a.id));
    assert_eq!(ids("team", "blue"), std::slice::from_ref(&b.id));
    assert!(ids("firm", "other").is_empty());
}
