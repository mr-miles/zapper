//! The daemon without VMs (`testing::TestDaemon`): the config, approvals
//! and events, and sessions registered without booting.

use zapper_policy::{Policy, Scope};

use super::session::NewSession;
use super::testing::TestDaemon;

const RULES: &str = "
policy:
  net:
    - { id: global-deny, action: deny, host: '*.bad.example' }
profiles:
  web:
    policy:
      net:
        - { id: web-allow, action: allow, host: example.com }
  bot:
    agent: { kind: claude-code, driven: true }
";

fn new_session(t: &TestDaemon, profile: &str) -> NewSession {
    NewSession::plain(profile.into(), t.workdir(), Policy::default())
}

fn event_type(e: &super::Event) -> String {
    serde_json::to_value(e).unwrap()["type"]
        .as_str()
        .unwrap_or("")
        .to_string()
}

#[test]
fn a_change_applies_whole_or_not_at_all_and_is_an_event() {
    let t = TestDaemon::new(RULES);
    let mut events = t.d.events.subscribe();
    t.d.change("profile x added", |c| {
        c.profiles.insert("x".into(), Default::default());
        Ok(())
    })
    .unwrap();
    assert!(t.d.config.read().unwrap().profiles.contains_key("x"));
    assert_eq!(event_type(&events.try_recv().unwrap()), "config");
    assert!(t
        .d
        .recent_events(None)
        .iter()
        .any(|e| event_type(e) == "config"));

    // A profile extending one that doesn't exist: invalid, so nothing changes.
    let bad = t.d.change("bad", |c| {
        c.profiles.insert(
            "y".into(),
            serde_yaml::from_str("{ extends: nowhere }").unwrap(),
        );
        c.profiles.remove("x");
        Ok(())
    });
    assert!(bad.is_err());
    let c = t.d.config.read().unwrap().clone();
    assert!(c.profiles.contains_key("x") && !c.profiles.contains_key("y"));
}

#[test]
fn diff_save_and_reload() {
    let t = TestDaemon::new(RULES);
    assert!(t.d.diff().trim().is_empty(), "nothing unsaved at start");
    t.d.change("x", |c| {
        c.profiles.insert("saved".into(), Default::default());
        Ok(())
    })
    .unwrap();
    assert!(t.d.diff().contains("saved"));
    t.d.save().unwrap();
    assert!(t.d.diff().trim().is_empty());
    let on_disk = std::fs::read_to_string(t.startup()).unwrap();
    assert!(on_disk.contains("saved:"), "{on_disk}");
    assert!(t.d.startup_config().unwrap().profiles.contains_key("saved"));

    t.d.change("y", |c| {
        c.profiles.insert("unsaved".into(), Default::default());
        Ok(())
    })
    .unwrap();
    t.d.reload().unwrap();
    let c = t.d.config.read().unwrap().clone();
    assert!(c.profiles.contains_key("saved") && !c.profiles.contains_key("unsaved"));
}

#[test]
fn approvals_start_empty_and_unknown_ones_are_errors() {
    let t = TestDaemon::new("");
    assert!(t.d.approvals().is_empty());
    assert!(t.d.answer("nosuch", true, None).is_err());
    let status = super::record::status();
    assert!(status.is_object(), "{status}");
}

// Sessions, registered without a VM.

#[cfg(feature = "office")]
fn firm_link(
    member: &str,
    role: Option<(String, std::sync::Arc<std::sync::RwLock<Policy>>)>,
) -> super::office::FirmLink {
    super::office::FirmLink {
        firm: "acme".into(),
        member: member.into(),
        worker: true,
        run: std::sync::Weak::new(),
        role,
    }
}

#[test]
fn a_sessions_layers_its_own_then_its_profiles_then_global() {
    let t = TestDaemon::new(RULES);
    let s = t.test_session(new_session(&t, "web"), true).unwrap();
    let scopes: Vec<Scope> = s
        .layers(&t.d.config.read().unwrap())
        .into_iter()
        .map(|(s, _)| s)
        .collect();
    assert_eq!(
        scopes,
        [
            Scope::Session(s.id.clone()),
            Scope::Profile("web".into()),
            Scope::Global
        ]
    );
}

#[cfg(feature = "office")]
#[test]
fn a_firm_workers_layers_have_its_role_between_its_own_and_its_profiles() {
    let t = TestDaemon::new(RULES);
    let role = std::sync::Arc::new(std::sync::RwLock::new(Policy::default()));
    let mut new = new_session(&t, "web");
    new.firm = Some(firm_link("pat", Some(("eng".into(), role))));
    let s = t.test_session(new, true).unwrap();
    let scopes: Vec<Scope> = s
        .layers(&t.d.config.read().unwrap())
        .into_iter()
        .map(|(s, _)| s)
        .collect();
    assert_eq!(
        scopes,
        [
            Scope::Session(s.id.clone()),
            Scope::Role("acme/eng".into()),
            Scope::Profile("web".into()),
            Scope::Global
        ]
    );
}

#[test]
fn a_driven_session_has_a_driver() {
    let t = TestDaemon::new(RULES);
    let s = t.test_session(new_session(&t, "bot"), true).unwrap();
    assert!(s.driver.is_some());
    assert_eq!(s.info().driver["status"], "idle");
    let plain = t.test_session(new_session(&t, "web"), true).unwrap();
    assert!(plain.driver.is_none());
}

#[cfg(feature = "office")]
#[test]
fn a_firms_workers_are_driven_by_the_foreman_not_a_driver() {
    let t = TestDaemon::new(RULES);
    let mut worker = new_session(&t, "bot");
    worker.firm = Some(firm_link("pat", None));
    let worker = t.test_session(worker, true).unwrap();
    assert!(worker.driver.is_none());
    let i = worker.info();
    assert_eq!(i.firm.as_deref(), Some("acme"));
    assert_eq!(i.member.as_deref(), Some("pat"));
}

#[test]
fn with_the_driver_off_no_session_is_driven() {
    let t = TestDaemon::with_daemon("driver: false", "profiles:\n  web: {}\n");
    let s = t.test_session(new_session(&t, "web"), true).unwrap();
    assert!(s.driver.is_none());
}
