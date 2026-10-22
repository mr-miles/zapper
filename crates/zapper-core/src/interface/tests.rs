//! `Core` over a daemon without VMs (`testing::TestCore`).

use zapper_policy::Policy;

use crate::daemon::session::Status;
use crate::testing::TestCore;
use crate::NewSession;

fn new_session(t: &TestCore, profile: &str) -> NewSession {
    NewSession::plain(profile.into(), t.workdir(), Policy::default())
}

#[test]
fn only_a_stopped_session_is_forgotten() {
    let t = TestCore::new("profiles:\n  web: {}\n");
    let running = t.core.test_session(new_session(&t, "web"), true).unwrap();
    let starting = t.core.test_session(new_session(&t, "web"), false).unwrap();
    for s in [&running, &starting] {
        t.core.forget_session(&s.id);
        assert!(
            t.core.session(&s.id).is_ok(),
            "still listed: nothing else would manage its VM"
        );
    }
    running
        .status
        .send_replace(Status::Stopped("stopped by test".into()));
    t.core.forget_session(&running.id);
    assert!(t.core.session(&running.id).is_err());
    starting.status.send_replace(Status::Failed("no VM".into()));
    t.core.forget_session(&starting.id);
    assert!(t.core.sessions().is_empty());
}
