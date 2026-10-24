//! Starting a run and the runs listed: a firm starts only if its sessions
//! fit beside the running ones, and the list of runs says how the live one
//! is doing.

use zapper_core::testing::TestCore;
use zapper_core::NewSession;
use zapper_policy::Policy;

const FIRM: &str = "
profiles:
  w: { agent: { kind: claude-code } }
firms:
  acme:
    roles:
      ceo: { profile: w, agent: ceo }
    workers:
      - { name: ceo, role: ceo }
      - { name: ada, role: ceo, reports_to: ceo }
";

#[tokio::test]
async fn a_firm_starts_only_beside_the_sessions_still_running() {
    let t = TestCore::with_daemon("max_sessions: 2", FIRM);
    crate::start(&t.core);
    let session = |t: &TestCore| {
        let new = NewSession::plain("w".into(), t.workdir(), Policy::default());
        t.core.test_session(new, true).unwrap()
    };
    let (a, b) = (session(&t), session(&t));
    t.core.stop_session(&b, "done").await;
    let e = crate::up_with(&t.core, "acme", &Default::default())
        .await
        .err()
        .unwrap()
        .to_string();
    // The stopped one doesn't count.
    assert!(e.contains("and 1 are running"), "{e}");
    assert!(e.contains("daemon.max_sessions is 2"), "{e}");
    drop(a);
}

#[test]
fn the_live_run_is_listed_with_its_state() {
    let t = crate::test_core(FIRM);
    let run = crate::test_run(&t.core, "acme");
    let runs = crate::list_runs(&t.core, "acme");
    let live = runs.iter().find(|r| r["run"] == run.id.as_str()).unwrap();
    assert_eq!(live["state"], "running", "{live}");
}

/// A config that is valid but likely not meant: the board is told when the
/// run starts.
#[test]
fn the_board_is_warned_of_officers_without_a_budget() {
    let firm = "
profiles:
  w: { agent: { kind: claude-code } }
firms:
  acme:
    budget: { seed: 100000, security_share: 0 }
    security: { officers: [sec] }
    roles:
      ceo: { profile: w, agent: ceo }
      sec: { profile: w, agent: security-officer, officer: true }
    workers:
      - { name: ceo, role: ceo }
      - { name: sec, role: sec, reports_to: ceo }
";
    let t = crate::test_core(firm);
    let run = crate::test_run(&t.core, "acme");
    let inbox = run.post.with(|po| po.inbox(crate::firm::BOARD, true, 10));
    assert_eq!(inbox.len(), 1, "{inbox:?}");
    assert_eq!(inbox[0]["subject"], "A warning about the firm's config");

    // With a share, nothing to say.
    let t = crate::test_core(&firm.replace("security_share: 0", "security_share: 0.2"));
    let run = crate::test_run(&t.core, "acme");
    assert!(run
        .post
        .with(|po| po.inbox(crate::firm::BOARD, true, 10))
        .is_empty());
}
