//! `Core` over a daemon without VMs (`testing::TestCore`): the config,
//! rules, approvals and events, and sessions registered without booting.

use crate::prelude::*;
use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use serde_json::Value;
use zapper_policy::{AnyRule, Policy, Scope};

use crate::testing::TestCore;
use crate::{NewSession, RuleRemoval, Status};

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

fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn new_session(t: &TestCore, profile: &str) -> NewSession {
    NewSession::plain(profile.into(), t.workdir(), Policy::default())
}

fn event_type(e: &crate::Event) -> String {
    serde_json::to_value(e).unwrap()["type"]
        .as_str()
        .unwrap_or("")
        .to_string()
}

#[test]
fn a_change_applies_whole_or_not_at_all_and_is_an_event() {
    let t = TestCore::new(RULES);
    let mut events = t.core.subscribe();
    t.core
        .change("profile x added", |c| {
            c.profiles.insert("x".into(), Default::default());
            Ok(())
        })
        .unwrap();
    assert!(t.core.config().profiles.contains_key("x"));
    assert_eq!(event_type(&events.try_recv().unwrap()), "config");
    assert!(t
        .core
        .recent_events(None)
        .iter()
        .any(|e| event_type(e) == "config"));

    // A profile extending one that doesn't exist: invalid, so nothing changes.
    let bad = t.core.change("bad", |c| {
        c.profiles.insert(
            "y".into(),
            serde_yaml::from_str("{ extends: nowhere }").unwrap(),
        );
        c.profiles.remove("x");
        Ok(())
    });
    assert!(bad.is_err());
    let c = t.core.config();
    assert!(c.profiles.contains_key("x") && !c.profiles.contains_key("y"));
    assert!(t.core.with_config(|c| c.profiles.contains_key("x")));
}

/// A change made while another is between its copy of the config and
/// applying it: both are kept.
#[test]
fn changes_at_once_both_apply() {
    let t = TestCore::new(RULES);
    let core = t.core.clone();
    let (inside, entered) = std::sync::mpsc::channel();
    let first = std::thread::spawn(move || {
        core.change("a", |c| {
            inside.send(()).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(300));
            c.profiles.insert("a".into(), Default::default());
            Ok(())
        })
        .unwrap()
    });
    entered.recv().unwrap();
    t.core
        .change("b", |c| {
            c.profiles.insert("b".into(), Default::default());
            Ok(())
        })
        .unwrap();
    first.join().unwrap();
    let c = t.core.config();
    assert!(c.profiles.contains_key("a"), "the first change kept");
    assert!(c.profiles.contains_key("b"));
}

#[test]
fn diff_save_and_reload() {
    let t = TestCore::new(RULES);
    assert!(t.core.diff().trim().is_empty(), "nothing unsaved at start");
    t.core
        .change("x", |c| {
            c.profiles.insert("saved".into(), Default::default());
            Ok(())
        })
        .unwrap();
    assert!(t.core.diff().contains("saved"));
    t.core.save().unwrap();
    assert!(t.core.diff().trim().is_empty());
    let on_disk = std::fs::read_to_string(t.startup()).unwrap();
    assert!(on_disk.contains("saved:"), "{on_disk}");
    assert!(t
        .core
        .startup_config()
        .unwrap()
        .profiles
        .contains_key("saved"));

    t.core
        .change("y", |c| {
            c.profiles.insert("unsaved".into(), Default::default());
            Ok(())
        })
        .unwrap();
    t.core.reload().unwrap();
    let c = t.core.config();
    assert!(c.profiles.contains_key("saved") && !c.profiles.contains_key("unsaved"));
}

#[test]
fn rule_layers_and_removing_rules() {
    let t = TestCore::new(RULES);
    let scopes: Vec<Scope> = t.core.rule_layers().into_iter().map(|(s, _)| s).collect();
    assert!(scopes.contains(&Scope::Global));
    assert!(scopes.contains(&Scope::Profile("web".into())));

    match t.core.remove_rule("web-allow").unwrap() {
        RuleRemoval::Removed(scope, rule) => {
            assert_eq!(scope, Scope::Profile("web".into()));
            assert!(matches!(*rule, AnyRule::Net(_)));
        }
        RuleRemoval::Revoked => panic!("not a firm's"),
    }
    assert!(t.core.config().profiles["web"].policy.net.is_empty());
    let again = t.core.remove_rule("web-allow").err().unwrap().to_string();
    assert!(again.starts_with("no rule"), "{again}");
    let session_style = t.core.remove_rule("s-nothing").err().unwrap().to_string();
    assert!(session_style.starts_with("no rule"), "{session_style}");
}

#[test]
fn the_layers_a_request_is_decided_by() {
    let t = TestCore::new(RULES);
    let scopes = |s: Option<&str>, p: Option<&str>| -> Vec<Scope> {
        t.core
            .layers_for(s, p)
            .unwrap()
            .into_iter()
            .map(|(s, _)| s)
            .collect()
    };
    assert_eq!(scopes(None, None), [Scope::Global]);
    assert_eq!(
        scopes(None, Some("web")),
        [Scope::Profile("web".into()), Scope::Global]
    );
    assert!(t.core.layers_for(Some("nosuch"), None).is_err());
    assert!(t.core.layers_for(None, Some("nosuch")).is_err());
}

#[test]
fn audit_paths_take_session_ids_only() {
    let t = TestCore::new("");
    assert!(t.core.audit_path("../etc").is_err());
    assert!(t.core.audit_path("a/b").is_err());
    // A session id's form, of a session the daemon no longer lists.
    let id = crate::util::random_id(6);
    let p = t.core.audit_path(&id).unwrap();
    assert!(
        p.ends_with(format!("state/sessions/{id}/audit.jsonl")),
        "{}",
        p.display()
    );
}

#[test]
fn approvals_start_empty_and_unknown_ones_are_errors() {
    let t = TestCore::new("");
    assert!(t.core.approvals().is_empty());
    assert!(t.core.answer("nosuch", true, None).is_err());
    let status = t.core.record_status();
    assert!(status.is_object(), "{status}");
}

#[tokio::test]
async fn shutdown_with_nothing_running() {
    let t = TestCore::new("");
    t.core.shutdown().await;
    assert!(t.core.sessions().is_empty());
}

// Sessions, registered without a VM.

#[test]
fn a_sessions_layers_its_own_then_shared_then_its_profiles_then_global() {
    let t = TestCore::new(RULES);
    let role = Arc::new(RwLock::new(Policy::default()));
    let mut new = new_session(&t, "web");
    new.shared_layers = vec![(Scope::Role("acme/eng".into()), role)];
    let s = t.core.test_session(new, true).unwrap();
    let scopes: Vec<Scope> = s
        .layers(&t.core.config())
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
    // The session's own layer is listed with the others.
    assert!(t
        .core
        .rule_layers()
        .iter()
        .any(|(sc, _)| *sc == Scope::Session(s.id.clone())));
}

#[test]
fn labels_are_in_the_info_and_find_sessions() {
    let t = TestCore::new(RULES);
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
            .sessions()
            .iter()
            .filter(|s| s.label(k) == Some(v))
            .map(|s| s.id.clone())
            .collect()
    };
    assert_eq!(ids("firm", "acme"), std::slice::from_ref(&a.id));
    assert_eq!(ids("team", "blue"), std::slice::from_ref(&b.id));
    assert!(ids("firm", "other").is_empty());
}

#[test]
fn a_driven_session_has_a_driver_unless_a_firm_drives_it() {
    let t = TestCore::new(RULES);
    let s = t.core.test_session(new_session(&t, "bot"), true).unwrap();
    assert!(s.driver.is_some());
    let info = t.core.session_info(&s);
    assert_eq!(info.driver["status"], "idle");

    let mut worker = new_session(&t, "bot");
    worker.labels = labels(&[("firm", "acme"), ("member", "pat")]);
    let worker = t.core.test_session(worker, true).unwrap();
    assert!(
        worker.driver.is_none(),
        "the foreman drives a firm's workers"
    );

    let plain = t.core.test_session(new_session(&t, "web"), true).unwrap();
    assert!(plain.driver.is_none());
}

#[test]
fn with_the_driver_off_session_info_says_disabled() {
    let t = TestCore::with_daemon("driver: false", "profiles:\n  web: {}\n");
    let s = t.core.test_session(new_session(&t, "web"), true).unwrap();
    assert!(s.driver.is_none());
    assert_eq!(t.core.session_info(&s).driver, Value::from("disabled"));
}

#[test]
fn only_a_stopped_session_is_forgotten() {
    let t = TestCore::new(RULES);
    let running = t.core.test_session(new_session(&t, "web"), true).unwrap();
    let starting = t.core.test_session(new_session(&t, "web"), false).unwrap();
    for s in [&running, &starting] {
        let e = t.core.forget_session(&s.id).err().unwrap().to_string();
        assert!(e.contains("still running"), "{e}");
        assert!(t.core.session(&s.id).is_ok(), "still listed");
    }
    running
        .runtime
        .force(Status::Stopped("stopped by test".into()));
    t.core.forget_session(&running.id).unwrap();
    assert!(t.core.session(&running.id).is_err());
    starting.runtime.force(Status::Failed("no VM".into()));
    t.core.forget_session(&starting.id).unwrap();
    assert!(t.core.sessions().is_empty());
}

#[test]
fn removing_a_sessions_rule_leaves_another_sessions_alone() {
    let t = TestCore::new(RULES);
    let a = t.core.test_session(new_session(&t, "web"), true).unwrap();
    let b = t.core.test_session(new_session(&t, "web"), true).unwrap();
    let rule = || -> AnyRule {
        serde_yaml::from_str("{ kind: net, action: allow, host: api.example.com }").unwrap()
    };
    let in_a = t.core.add_session_rule(&a, rule(), None);
    let in_b = t.core.add_session_rule(&b, rule(), None);
    assert_ne!(in_a.id(), in_b.id(), "each session's first rule");

    match t.core.remove_rule(in_b.id().unwrap()).unwrap() {
        RuleRemoval::Removed(scope, _) => assert_eq!(scope, Scope::Session(b.id.clone())),
        RuleRemoval::Revoked => panic!("not a firm's"),
    }
    assert_eq!(a.rules().len(), 1, "A keeps its rule");
    assert!(b.rules().is_empty());

    // An id both hold (put by a client): removed from neither.
    let shared = || -> AnyRule {
        serde_yaml::from_str("{ kind: net, id: s-both, action: allow, host: x.example }").unwrap()
    };
    a.put_rule(shared());
    b.put_rule(shared());
    let e = t.core.remove_rule("s-both").err().unwrap().to_string();
    assert!(e.contains(&a.id) && e.contains(&b.id), "{e}");
    assert_eq!((a.rules().len(), b.rules().len()), (2, 1));
}

/// A subscriber of one session's events, resuming, is sent that session's
/// alone: another's that came meanwhile are no part of it.
#[test]
fn a_resumed_sessions_events_are_its_own() {
    let t = TestCore::new(RULES);
    let a = t.core.test_session(new_session(&t, "web"), true).unwrap();
    let b = t.core.test_session(new_session(&t, "web"), true).unwrap();
    let rule = || -> AnyRule {
        serde_yaml::from_str("{ kind: net, action: allow, host: api.example.com }").unwrap()
    };
    let (_, start) = t.core.catch_up(None, None, true);
    let after = crate::Cursor {
        epoch: t.core.epoch(),
        seq: start.unwrap_or(0),
    };
    for _ in 0..3 {
        t.core.add_session_rule(&a, rule(), None);
        t.core.add_session_rule(&b, rule(), None);
    }
    let (items, _) = t.core.catch_up(Some(&a.id), Some(&after), false);
    let sessions: Vec<Option<String>> = items
        .iter()
        .filter_map(|i| match i {
            crate::Catchup::Event(e) => Some(e.session.clone()),
            crate::Catchup::Gap(_) => None,
        })
        .collect();
    assert_eq!(sessions.len(), 3, "{sessions:?}");
    assert!(sessions.iter().all(|s| s.as_deref() == Some(a.id.as_str())));
}

#[test]
fn rules_added_to_a_session_are_its_own_and_an_event() {
    let t = TestCore::new(RULES);
    let s = t.core.test_session(new_session(&t, "web"), true).unwrap();
    let mut events = t.core.subscribe();
    let rule: AnyRule =
        serde_yaml::from_str("{ kind: net, action: allow, host: api.example.com }").unwrap();
    let added = t.core.add_session_rule(&s, rule, None);
    let id = added.id().unwrap().to_string();
    assert!(id.starts_with("s-"), "{id}");
    assert_eq!(event_type(&events.try_recv().unwrap()), "rule");
    match t.core.remove_rule(&id).unwrap() {
        RuleRemoval::Removed(scope, _) => assert_eq!(scope, Scope::Session(s.id.clone())),
        RuleRemoval::Revoked => panic!("not a firm's"),
    }
}

/// The `rule` events since `events` subscribed: each `(state, id, scope)`.
fn rule_events(
    events: &mut tokio::sync::broadcast::Receiver<crate::Event>,
) -> Vec<(String, String, String)> {
    let mut out = vec![];
    while let Ok(e) = events.try_recv() {
        if let crate::EventBody::Rule {
            state, id, scope, ..
        } = e.body
        {
            out.push((state, id, scope));
        }
    }
    out
}

fn ev(state: &str, id: &str, scope: &str) -> (String, String, String) {
    (state.into(), id.into(), scope.into())
}

#[test]
fn every_way_a_rule_changes_is_a_rule_event() {
    let t = TestCore::new(RULES);
    let s = t.core.test_session(new_session(&t, "web"), true).unwrap();
    let mut events = t.core.subscribe();
    let rule = |yaml: &str| -> AnyRule { serde_yaml::from_str(yaml).unwrap() };
    let session = format!("session:{}", s.id);

    // A session's own layer: added, put by a client, removed.
    let added = t.core.add_session_rule(
        &s,
        rule("{ kind: net, action: allow, host: a.example }"),
        None,
    );
    let a = added.id().unwrap().to_string();
    s.put_rule(rule(
        "{ id: grant-1, kind: net, action: allow, host: b.example }",
    ));
    t.core.remove_rule(&a).unwrap();
    assert_eq!(
        rule_events(&mut events),
        [
            ev("added", &a, &session),
            ev("added", "grant-1", &session),
            ev("removed", &a, &session),
        ]
    );

    // The config's layers: one added, one removed, one changed under its id.
    let mut c = t.core.config();
    c.policy.remove("global-deny");
    c.policy.insert(
        rule("{ id: g-new, kind: net, action: allow, host: n.example }"),
        None,
    );
    let web = c.profiles.get_mut("web").unwrap();
    web.policy.remove("web-allow");
    web.policy.insert(
        rule("{ id: web-allow, kind: net, action: deny, host: example.com }"),
        None,
    );
    t.core.replace_config(c).unwrap();
    let mut got = rule_events(&mut events);
    got.sort();
    let mut want = vec![
        ev("removed", "global-deny", "global"),
        ev("added", "g-new", "global"),
        ev("removed", "web-allow", "profile:web"),
        ev("added", "web-allow", "profile:web"),
    ];
    want.sort();
    assert_eq!(got, want);

    // A change that leaves the rules as they were says nothing of them.
    t.core.replace_config(t.core.config()).unwrap();
    assert_eq!(rule_events(&mut events), []);
}

#[test]
fn a_rule_in_a_named_layer_is_listed_and_an_event_when_removed() {
    let t = TestCore::new(RULES);
    // A firm role's shared layer the core keeps by name.
    let layer = t.core.daemon().named_layer("acme/eng");
    layer.write().unwrap().insert(
        serde_yaml::from_str("{ id: role-1, kind: net, action: allow, host: r.example }").unwrap(),
        None,
    );
    // It is listed among the rule layers, as `role:acme/eng`.
    assert!(t
        .core
        .rule_layers()
        .iter()
        .any(|(s, p)| matches!(s, Scope::Role(r) if r == "acme/eng")
            && p.rules().iter().any(|r| r.id() == Some("role-1"))));
    // Removing it takes it out of the layer and says so.
    let mut events = t.core.subscribe();
    assert!(matches!(
        t.core.remove_rule("role-1").unwrap(),
        RuleRemoval::Revoked
    ));
    assert!(
        layer.read().unwrap().rules().is_empty(),
        "gone from the layer"
    );
    assert_eq!(
        rule_events(&mut events),
        [ev("removed", "role-1", "role:acme/eng")]
    );
}
