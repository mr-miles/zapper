//! A firm's run without VMs (`test_run`): finding a session's run, grants
//! and their network rules per repository, and the firm's service names.
//! The git server's checks are in `facility`'s tests, the office port's in
//! `ops`'.

use std::sync::Arc;

use zapper_policy::{AnyRule, Policy, BOARD};

use super::access::{self, Level};
use super::facility::{self, Internal};
use super::{test_run, FirmLink, FirmRun};
use crate::daemon::session::{NewSession, Session};
use crate::daemon::testing::TestDaemon;

const FIRM: &str = "
profiles:
  w: { agent: { kind: claude-code } }
firms:
  acme:
    roles:
      ceo: { profile: w, skills: { work: ceo } }
      eng: { profile: w, skills: { work: software-engineer } }
    workers:
      - { name: ceo, role: ceo }
      - { name: pat, role: eng, reports_to: ceo }
    git: { repos: [api, web] }
";

pub(crate) fn run() -> (TestDaemon, Arc<FirmRun>) {
    let t = TestDaemon::new(FIRM);
    let run = test_run(&t.d, "acme");
    (t, run)
}

pub(crate) fn session_of(run: &FirmRun, worker: &str) -> Arc<Session> {
    run.rt(worker)
        .unwrap()
        .session
        .lock()
        .unwrap()
        .clone()
        .unwrap()
}

/// The ids of a worker's credential rules, and where the first allows.
fn credential_rules(run: &FirmRun, worker: &str) -> Vec<(String, String, Vec<u16>)> {
    let mut v: Vec<(String, String, Vec<u16>)> = session_of(run, worker)
        .policy
        .read()
        .unwrap()
        .rules()
        .into_iter()
        .filter_map(|r| match r {
            AnyRule::Net(n)
                if n.id
                    .as_deref()
                    .is_some_and(|i| i.starts_with("credentials-")) =>
            {
                Some((n.id.unwrap(), n.host, n.ports))
            }
            _ => None,
        })
        .collect();
    v.sort();
    v
}

fn ids(rules: &[(String, String, Vec<u16>)]) -> Vec<&str> {
    rules.iter().map(|(i, ..)| i.as_str()).collect()
}

#[test]
fn a_session_finds_its_run_by_its_link() {
    let (t, run) = run();
    let pat = session_of(&run, "pat");
    let found = pat.firm.as_ref().unwrap().run().unwrap();
    assert_eq!(found.id, run.id);

    // A session from an earlier run of the firm: not this run's.
    let mut old = NewSession::plain("w".into(), t.workdir(), Policy::default());
    old.firm = Some(FirmLink {
        firm: "acme".into(),
        member: "pat".into(),
        worker: true,
        run: std::sync::Weak::new(),
        role: None,
    });
    let old = t.test_session(old, true).unwrap();
    assert!(old.firm.as_ref().unwrap().run().is_none());
}

#[test]
fn the_ceo_holds_every_repository_and_others_nothing() {
    let (_t, run) = run();
    let host = format!("git.{}", run.domain);
    let ceo = credential_rules(&run, "ceo");
    assert_eq!(ids(&ceo), ["credentials-git-api", "credentials-git-web"]);
    assert!(ceo.iter().all(|(_, h, p)| *h == host && *p == [443]));
    assert!(credential_rules(&run, "pat").is_empty());
}

#[test]
fn a_git_grant_is_per_repository_with_its_own_network_rule() {
    let (t, run) = run();
    access::board_grant(&t.d, &run, "pat", "git:api", Level::Write, None, "test").unwrap();
    assert_eq!(ids(&credential_rules(&run, "pat")), ["credentials-git-api"]);
    assert_eq!(access::level(&run, "pat", "git:api"), Level::Write);
    assert_eq!(access::level(&run, "pat", "git:web"), Level::None);

    access::board_grant(&t.d, &run, "pat", "git:web", Level::Read, None, "test").unwrap();
    assert_eq!(
        ids(&credential_rules(&run, "pat")),
        ["credentials-git-api", "credentials-git-web"]
    );

    // Revoking one repository leaves the other's rule, and so its host.
    access::revoke(&t.d, &run, BOARD, "pat", "git:api", "done").unwrap();
    assert_eq!(ids(&credential_rules(&run, "pat")), ["credentials-git-web"]);
    assert_eq!(access::level(&run, "pat", "git:api"), Level::None);

    // With two repositories, plain `git` names none of them.
    let e = access::board_grant(&t.d, &run, "pat", "git", Level::Read, None, "test")
        .unwrap_err()
        .to_string();
    assert!(e.contains("name the repository"), "{e}");
}

#[test]
fn the_firms_names_are_its_own() {
    let (_t, run) = run();
    let d = run.domain.clone();
    assert!(matches!(
        facility::route(&run, &format!("git.{d}"), 443),
        Some(Ok(Internal::Git))
    ));
    assert!(matches!(
        facility::route(&run, &format!("git.{d}"), 80),
        Some(Err(_))
    ));
    // No cluster in this firm: its names exist in the domain, unserved.
    assert!(matches!(
        facility::route(&run, &format!("cluster.{d}"), 6443),
        Some(Err(_))
    ));
    assert!(matches!(
        facility::route(&run, &format!("x.{d}"), 443),
        Some(Err(_))
    ));
    assert!(facility::route(&run, "example.com", 443).is_none());

    assert_eq!(facility::serves_name(&run, &format!("git.{d}")), Some(true));
    assert_eq!(
        facility::serves_name(&run, &format!("nope.{d}")),
        Some(false)
    );
    assert_eq!(facility::serves_name(&run, "example.com"), None);
}
