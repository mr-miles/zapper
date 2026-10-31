//! A session's own policy layer and the shared layers kept for it.

use std::sync::{Arc, RwLock};

use zapper_policy::{AnyRule, Config, Policy, ResolvedProfile, Scope};

/// A session's rule state: its own policy layer, and the shared layers a
/// client keeps for it (a firm role, between the session's layer and its
/// profile's). The owner of the session's rules; `Session` wraps these with
/// the identity and events they need.
pub(crate) struct Rules {
    /// The session's id, in the ids `add` mints.
    session: String,
    own: RwLock<Policy>,
    shared: Vec<(Scope, Arc<RwLock<Policy>>)>,
}

impl Rules {
    pub fn new(session: &str, shared: Vec<(Scope, Arc<RwLock<Policy>>)>) -> Self {
        Self {
            session: session.to_string(),
            own: RwLock::new(Policy::default()),
            shared,
        }
    }

    /// A clone of the session's own layer.
    pub fn own(&self) -> Policy {
        self.own.read().unwrap().clone()
    }

    /// The rules of the session's own layer.
    pub fn list(&self) -> Vec<AnyRule> {
        self.own.read().unwrap().rules()
    }

    /// Adds a rule to the own layer under a fresh `s-<session>-<kind>-<n>`
    /// id, so session rule ids clash neither with config ones nor with
    /// another session's.
    pub fn add(&self, mut rule: AnyRule, position: Option<usize>) -> AnyRule {
        let mut policy = self.own.write().unwrap();
        let taken: Vec<String> = policy
            .rules()
            .iter()
            .filter_map(|r| r.id().map(str::to_string))
            .collect();
        let kind = rule.kind();
        let id = (1..)
            .map(|n| format!("s-{}-{kind}-{n}", self.session))
            .find(|i| !taken.contains(i))
            .unwrap();
        rule.set_id(id);
        policy.insert(rule.clone(), position);
        rule
    }

    /// Replaces the rule with `rule`'s id and puts it first; returns that id.
    /// Callers always pass an id'd rule (an officer's approval, a grant); the
    /// `"?"` fallback never fires.
    pub fn put(&self, rule: AnyRule) -> String {
        let id = rule.id().unwrap_or("?").to_string();
        let mut p = self.own.write().unwrap();
        p.remove(&id);
        p.insert(rule, Some(0));
        id
    }

    /// The own layer as it was kept (a persistent session's, restored):
    /// its rules with the ids they had, which clients remove them by.
    pub fn restore(&self, policy: Policy) {
        *self.own.write().unwrap() = policy;
    }

    /// Removes a rule from the own layer, returning it if it was there.
    pub fn remove(&self, id: &str) -> Option<AnyRule> {
        self.own.write().unwrap().remove(id)
    }

    /// The session's policy layers, most specific first: own, shared, the
    /// profile's (from the running config, falling back to those resolved at
    /// start), then global.
    pub fn layers(
        &self,
        id: &str,
        profile: &str,
        resolved: &ResolvedProfile,
        config: &Config,
    ) -> Vec<(Scope, Policy)> {
        let mut v = vec![(Scope::Session(id.to_string()), self.own())];
        for (scope, policy) in &self.shared {
            v.push((scope.clone(), policy.read().unwrap().clone()));
        }
        match config.resolve(profile) {
            Ok(p) => v.extend(p.layers),
            Err(_) => v.extend(resolved.layers.iter().cloned()),
        }
        v.push((Scope::Global, config.policy.clone()));
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn net_rule() -> AnyRule {
        AnyRule::Net(
            serde_json::from_value(serde_json::json!({ "action": "allow", "host": "x.example" }))
                .unwrap(),
        )
    }

    fn exec_rule() -> AnyRule {
        AnyRule::Exec(
            serde_json::from_value(serde_json::json!({ "action": "allow", "path": "/bin/ls" }))
                .unwrap(),
        )
    }

    #[test]
    fn rules_mint_per_kind_ids_and_reuse_a_freed_one() {
        let r = Rules::new("sid", vec![]);
        assert_eq!(r.add(net_rule(), None).id(), Some("s-sid-net-1"));
        assert_eq!(r.add(net_rule(), None).id(), Some("s-sid-net-2"));
        // Each kind has its own sequence.
        assert_eq!(r.add(exec_rule(), None).id(), Some("s-sid-exec-1"));
        // The lowest free id is reused once one is removed.
        assert!(r.remove("s-sid-net-1").is_some());
        assert_eq!(r.add(net_rule(), None).id(), Some("s-sid-net-1"));
        // Removing it again finds nothing.
        assert!(r.remove("s-sid-net-1").is_some());
        assert!(r.remove("s-sid-net-1").is_none());
    }

    #[test]
    fn put_replaces_a_rule_by_id_and_puts_it_first() {
        let r = Rules::new("sid", vec![]);
        r.add(net_rule(), None); // s-sid-net-1
        r.add(net_rule(), None); // s-sid-net-2
        let replacement: AnyRule = AnyRule::Net(
            serde_json::from_value(
                serde_json::json!({ "id": "s-sid-net-1", "action": "deny", "host": "y.example" }),
            )
            .unwrap(),
        );
        r.put(replacement);
        let rules = r.list();
        assert_eq!(rules.len(), 2, "replaced, not added");
        assert_eq!(
            rules[0].id(),
            Some("s-sid-net-1"),
            "the replacement is first"
        );
        assert!(
            matches!(&rules[0], AnyRule::Net(n) if n.action == zapper_policy::Action::Deny),
            "it is the new rule"
        );
    }

    #[test]
    fn layers_fall_back_to_the_resolved_profile_when_it_leaves_the_config() {
        let c1 = Config::from_yaml(
            "profiles:\n  web:\n    policy:\n      net:\n        - { action: allow, host: a.test }\n",
        )
        .unwrap();
        let resolved = c1.resolve("web").unwrap();
        // A running config that no longer has `web`.
        let c2 = Config::from_yaml("profiles:\n  other: {}\n").unwrap();
        let layers = Rules::new("sid", vec![]).layers("sid", "web", &resolved, &c2);
        let scopes: Vec<Scope> = layers.iter().map(|(s, _)| s.clone()).collect();
        assert_eq!(scopes.first(), Some(&Scope::Session("sid".into())));
        assert_eq!(scopes.last(), Some(&Scope::Global));
        // The profile layer came from the resolved snapshot, not from c2.
        assert!(
            layers
                .iter()
                .any(|(s, p)| matches!(s, Scope::Profile(n) if n == "web") && !p.net.is_empty()),
            "fell back to the resolved web layers: {scopes:?}"
        );
    }
}
