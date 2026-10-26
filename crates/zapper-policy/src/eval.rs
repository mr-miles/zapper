use anyhow::{Context, Result};
use globset::{Glob, GlobBuilder, GlobMatcher};
use serde::{Deserialize, Serialize};

use crate::{Action, ExecRule, NetRule, Policy, Scope};

/// Policy layers in evaluation order: most specific first, global last.
pub type Layers<'a> = [(Scope, &'a Policy)];

/// A connection, or a request on an inspected connection.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NetRequest {
    pub host: String,
    pub port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ExecRequest {
    /// Resolved executable path.
    pub path: String,
    #[serde(default)]
    pub argv: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Decision {
    pub action: Action,
    /// Matching rule, `None` for the default action.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
    /// Layer of the matching rule or default, `None` for the built-in deny.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<Scope>,
}

/// What an allowed net request should have applied to it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NetEffects {
    pub secrets: Vec<String>,
    pub set_headers: Vec<(String, String)>,
    /// The matched rule's protocol: `postgres` routes the connection to the
    /// database auth-relay instead of the HTTP/tunnel path.
    pub protocol: crate::config::NetProtocol,
}

fn host_glob(pattern: &str) -> Result<GlobMatcher> {
    let g = GlobBuilder::new(pattern).case_insensitive(true).build()?;
    Ok(g.compile_matcher())
}

/// Case-insensitive host glob match, ignoring a trailing dot.
pub fn host_matches(pattern: &str, host: &str) -> bool {
    host_glob(pattern).is_ok_and(|g| g.is_match(host.trim_end_matches('.')))
}

fn path_glob(pattern: &str) -> Result<GlobMatcher> {
    let g = GlobBuilder::new(pattern).literal_separator(true).build()?;
    Ok(g.compile_matcher())
}

fn args_glob(pattern: &str) -> Result<GlobMatcher> {
    Ok(Glob::new(pattern)?.compile_matcher())
}

/// Checks that every glob in a layer compiles.
pub fn check_globs(policy: &Policy) -> Result<()> {
    for r in &policy.net {
        host_glob(&r.host).with_context(|| format!("host {:?}", r.host))?;
        if let Some(p) = &r.path {
            path_glob(p).with_context(|| format!("path {p:?}"))?;
        }
    }
    for r in &policy.exec {
        path_glob(&r.path).with_context(|| format!("path {:?}", r.path))?;
        if let Some(a) = &r.args {
            args_glob(a).with_context(|| format!("args {a:?}"))?;
        }
    }
    for r in &policy.fs {
        path_glob(&r.path).with_context(|| format!("path {:?}", r.path))?;
    }
    Ok(())
}

impl NetRule {
    /// Matches host and port only.
    pub fn matches_endpoint(&self, host: &str, port: u16) -> bool {
        (self.ports.is_empty() || self.ports.contains(&port)) && host_matches(&self.host, host)
    }

    pub fn matches(&self, req: &NetRequest) -> bool {
        if !self.matches_endpoint(&req.host, req.port) {
            return false;
        }
        if !self.methods.is_empty() {
            let Some(m) = &req.method else { return false };
            if !self.methods.iter().any(|x| x.eq_ignore_ascii_case(m)) {
                return false;
            }
        }
        if let Some(pattern) = &self.path {
            let Some(path) = &req.path else { return false };
            let path = path.split(['?', '#']).next().unwrap_or("");
            if !path_glob(pattern).is_ok_and(|g| g.is_match(path)) {
                return false;
            }
        }
        true
    }
}

impl ExecRule {
    pub fn matches(&self, req: &ExecRequest) -> bool {
        let target = if self.path.contains('/') {
            req.path.as_str()
        } else {
            req.path.rsplit('/').next().unwrap_or(&req.path)
        };
        if !path_glob(&self.path).is_ok_and(|g| g.is_match(target)) {
            return false;
        }
        match &self.args {
            None => true,
            Some(pattern) => {
                let args = req.argv.get(1..).unwrap_or_default().join(" ");
                args_glob(pattern).is_ok_and(|g| g.is_match(&args))
            }
        }
    }
}

/// Generic layered first-match evaluation.
fn evaluate<'a, R: 'a + RuleInfo>(
    layers: &'a Layers<'a>,
    rules: impl Fn(&'a Policy) -> &'a [R],
    is_final: impl Fn(&R) -> bool,
    matches: impl Fn(&R) -> bool,
    default: impl Fn(&Policy) -> Option<Action>,
) -> (Decision, Option<&'a R>) {
    // Final rules first, from the least specific layer down.
    for (scope, policy) in layers.iter().rev() {
        if let Some(r) = rules(policy).iter().find(|r| is_final(r) && matches(r)) {
            return (r.decision(scope), Some(r));
        }
    }
    for (scope, policy) in layers {
        if let Some(r) = rules(policy).iter().find(|r| !is_final(r) && matches(r)) {
            return (r.decision(scope), Some(r));
        }
    }
    for (scope, policy) in layers {
        if let Some(action) = default(policy) {
            return (
                Decision {
                    action,
                    rule: None,
                    scope: Some(scope.clone()),
                },
                None,
            );
        }
    }
    (
        Decision {
            action: Action::Deny,
            rule: None,
            scope: None,
        },
        None,
    )
}

trait RuleInfo {
    fn decision(&self, scope: &Scope) -> Decision;
}

impl RuleInfo for NetRule {
    fn decision(&self, scope: &Scope) -> Decision {
        Decision {
            action: self.action,
            rule: self.id.clone(),
            scope: Some(scope.clone()),
        }
    }
}

impl RuleInfo for ExecRule {
    fn decision(&self, scope: &Scope) -> Decision {
        Decision {
            action: self.action,
            rule: self.id.clone(),
            scope: Some(scope.clone()),
        }
    }
}

/// Evaluates a connection or request. Effects are only set when allowed.
pub fn evaluate_net(layers: &Layers<'_>, req: &NetRequest) -> (Decision, NetEffects) {
    let (decision, rule) = evaluate(
        layers,
        |p| &p.net,
        |r| r.final_,
        |r| r.matches(req),
        |p| p.defaults.net,
    );
    let effects = match rule {
        Some(r) if decision.action == Action::Allow => NetEffects {
            secrets: r.secrets.clone(),
            set_headers: r
                .set_headers
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            protocol: r.protocol,
        },
        _ => NetEffects::default(),
    };
    (decision, effects)
}

/// Whether connections to this endpoint must be decrypted: true when any rule
/// that could match a request to it depends on, or changes, the request.
pub fn needs_inspection(layers: &Layers<'_>, host: &str, port: u16) -> bool {
    layers.iter().any(|(_, p)| {
        p.net
            .iter()
            .any(|r| r.needs_inspection() && r.matches_endpoint(host, port))
    })
}

/// What a session can do with a name, over every port and request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    /// Some request to it would be allowed outright.
    Allow,
    /// Requests to it can only be asked about.
    Ask,
    /// No request to it could be allowed.
    Never,
}

/// What a session can do with `name`, over every port, method and path: the
/// rules in evaluation order (final rules from the global layer down, then
/// the layers, then the default). A matching rule without `ports`, `methods`
/// or `path` decides every request still undecided; a narrower one decides
/// only some, so the search goes on past it.
pub fn reach(layers: &Layers<'_>, name: &str) -> Reach {
    let unconditional =
        |r: &NetRule| r.ports.is_empty() && r.methods.is_empty() && r.path.is_none();
    let finals = layers
        .iter()
        .rev()
        .flat_map(|(_, p)| p.net.iter().filter(|r| r.final_));
    let others = layers
        .iter()
        .flat_map(|(_, p)| p.net.iter().filter(|r| !r.final_));
    let (mut allow, mut ask) = (false, false);
    let mut settled = None;
    for r in finals.chain(others).filter(|r| host_matches(&r.host, name)) {
        match r.action {
            Action::Allow => allow = true,
            Action::Ask => ask = true,
            Action::Deny => {}
        }
        if unconditional(r) {
            settled = Some(r.action);
            break;
        }
    }
    if settled.is_none() {
        match layers.iter().find_map(|(_, p)| p.defaults.net) {
            Some(Action::Allow) => allow = true,
            Some(Action::Ask) => ask = true,
            _ => {}
        }
    }
    if allow {
        Reach::Allow
    } else if ask {
        Reach::Ask
    } else {
        Reach::Never
    }
}

/// Whether the DNS server looks `name` up: some request to it could be
/// allowed, outright or after an `ask`.
pub fn resolvable(layers: &Layers<'_>, name: &str) -> bool {
    reach(layers, name) != Reach::Never
}

pub fn evaluate_exec(layers: &Layers<'_>, req: &ExecRequest) -> Decision {
    evaluate(
        layers,
        |p| &p.exec,
        |r| r.final_,
        |r| r.matches(req),
        |p| p.defaults.exec,
    )
    .0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Config;

    const CONFIG: &str = r#"
secrets:
  gh: { value: "real-token", hosts: ["api.github.com"] }
policy:
  defaults: { net: deny, exec: allow }
  net:
    - { action: deny, host: "169.254.169.254", final: true }
    - { action: allow, host: "*.ubuntu.com", ports: [80] }
  exec:
    - { action: deny, path: sudo, final: true }
profiles:
  base:
    policy:
      net:
        - { action: allow, host: "api.github.com", methods: [GET], secrets: [gh] }
        - { action: ask, host: "*.github.com" }
  dev:
    extends: base
    policy:
      defaults: { net: ask }
      net:
        - { action: allow, host: "169.254.169.254" }
        - { action: deny, host: "api.github.com", path: "/repos/*/delete/**" }
      exec:
        - { action: allow, path: sudo }
        - { action: deny, path: "/usr/bin/curl", args: "*evil*" }
"#;

    fn net(
        c: &Config,
        profile: &str,
        host: &str,
        port: u16,
        method: Option<&str>,
        path: Option<&str>,
    ) -> Decision {
        let p = c.resolve(profile).unwrap();
        let mut layers: Vec<(Scope, &Policy)> =
            p.layers.iter().map(|(s, p)| (s.clone(), p)).collect();
        layers.push((Scope::Global, &c.policy));
        let req = NetRequest {
            host: host.into(),
            port,
            method: method.map(Into::into),
            path: path.map(Into::into),
        };
        evaluate_net(&layers, &req).0
    }

    #[test]
    fn layered_net() {
        let c = Config::from_yaml(CONFIG).unwrap();
        // Global final deny wins over the profile's allow.
        let d = net(&c, "dev", "169.254.169.254", 80, None, None);
        assert_eq!((d.action, d.rule.as_deref()), (Action::Deny, Some("net-1")));
        // Profile rule wins over global.
        assert_eq!(
            net(&c, "dev", "api.github.com", 443, Some("GET"), Some("/user")).action,
            Action::Allow
        );
        // Child profile rule wins over parent.
        let d = net(
            &c,
            "dev",
            "api.github.com",
            443,
            Some("GET"),
            Some("/repos/a/delete/x"),
        );
        assert_eq!(d.action, Action::Deny);
        assert_eq!(d.scope, Some(Scope::Profile("dev".into())));
        // Parent's ask for other github hosts.
        assert_eq!(
            net(&c, "dev", "Gist.GitHub.com", 443, None, None).action,
            Action::Ask
        );
        // Global rule.
        assert_eq!(
            net(&c, "dev", "archive.ubuntu.com", 80, None, None).action,
            Action::Allow
        );
        assert_eq!(
            net(&c, "dev", "archive.ubuntu.com", 443, None, None).action,
            Action::Ask
        );
        // Defaults: dev says ask, base inherits the global deny.
        assert_eq!(
            net(&c, "dev", "example.com", 443, None, None).scope,
            Some(Scope::Profile("dev".into()))
        );
        assert_eq!(
            net(&c, "base", "example.com", 443, None, None).action,
            Action::Deny
        );
    }

    #[test]
    fn inspection_and_effects() {
        let c = Config::from_yaml(CONFIG).unwrap();
        let base = &c.profiles["base"].policy;
        let layers = [
            (Scope::Profile("base".into()), base),
            (Scope::Global, &c.policy),
        ];
        assert!(needs_inspection(&layers, "api.github.com", 443));
        assert!(!needs_inspection(&layers, "gist.github.com", 443));
        let req = NetRequest {
            host: "api.github.com".into(),
            port: 443,
            method: Some("get".into()),
            path: Some("/".into()),
        };
        assert_eq!(evaluate_net(&layers, &req).1.secrets, ["gh"]);
        // Without the request line the method rule does not match.
        let req = NetRequest {
            host: "api.github.com".into(),
            port: 443,
            ..Default::default()
        };
        assert_eq!(evaluate_net(&layers, &req).0.action, Action::Ask);
    }

    #[test]
    fn dns_resolvable() {
        let c = Config::from_yaml(CONFIG).unwrap();
        let base = &c.profiles["base"].policy;
        let layers = [
            (Scope::Profile("base".into()), base),
            (Scope::Global, &c.policy),
        ];
        assert!(resolvable(&layers, "api.github.com"));
        assert!(resolvable(&layers, "archive.ubuntu.com"));
        assert!(!resolvable(&layers, "example.com"));
        assert!(!resolvable(&layers, "169.254.169.254"));
        let dev = &c.profiles["dev"].policy;
        let layers = [
            (Scope::Profile("dev".into()), dev),
            (Scope::Global, &c.policy),
        ];
        assert!(resolvable(&layers, "example.com"), "dev defaults to ask");
    }

    #[test]
    fn reach_over_all_requests() {
        let reach_of = |yaml: &str, name: &str| {
            let c = Config::from_yaml(yaml).unwrap();
            reach(&[(Scope::Global, &c.policy)], name)
        };
        // A narrower deny first doesn't hide a broader allow.
        let narrow_deny = "policy:\n  net:\n    - { action: deny, host: a.example, path: '/admin/**' }\n    - { action: allow, host: a.example }\n";
        assert_eq!(reach_of(narrow_deny, "a.example"), Reach::Allow);
        // An unconditional deny first does.
        let deny_all = "policy:\n  net:\n    - { action: deny, host: a.example }\n    - { action: allow, host: a.example }\n";
        assert_eq!(reach_of(deny_all, "a.example"), Reach::Never);
        // A port-limited allow makes the name reachable.
        let port = "policy:\n  net:\n    - { action: allow, host: a.example, ports: [22] }\n";
        assert_eq!(reach_of(port, "a.example"), Reach::Allow);
        // A final deny wins over a profile allow; a narrower final deny doesn't.
        let fin = "policy:\n  net:\n    - { action: deny, host: a.example, final: true }\n    - { action: allow, host: a.example }\n";
        assert_eq!(reach_of(fin, "a.example"), Reach::Never);
        let fin_narrow = "policy:\n  net:\n    - { action: deny, host: a.example, ports: [80], final: true }\n    - { action: allow, host: a.example }\n";
        assert_eq!(reach_of(fin_narrow, "a.example"), Reach::Allow);
        // Ask only, and the defaults.
        let ask = "policy:\n  net:\n    - { action: ask, host: a.example }\n";
        assert_eq!(reach_of(ask, "a.example"), Reach::Ask);
        assert_eq!(
            reach_of("policy: { defaults: { net: ask } }\n", "b.example"),
            Reach::Ask
        );
        assert_eq!(
            reach_of("policy: { defaults: { net: allow } }\n", "b.example"),
            Reach::Allow
        );
        assert_eq!(reach_of("policy: {}\n", "b.example"), Reach::Never);
    }

    #[test]
    fn layered_exec() {
        let c = Config::from_yaml(CONFIG).unwrap();
        let p = c.resolve("dev").unwrap();
        let layers = [
            (Scope::Profile("dev".into()), &p.layers[0].1),
            (Scope::Global, &c.policy),
        ];
        let exec = |path: &str, argv: &[&str]| {
            let req = ExecRequest {
                path: path.into(),
                argv: argv.iter().map(|s| s.to_string()).collect(),
                cwd: None,
            };
            evaluate_exec(&layers, &req).action
        };
        assert_eq!(exec("/usr/bin/sudo", &["sudo", "ls"]), Action::Deny);
        assert_eq!(
            exec("/usr/bin/curl", &["curl", "https://evil.example"]),
            Action::Deny
        );
        assert_eq!(
            exec("/usr/bin/curl", &["curl", "https://ok.example"]),
            Action::Allow
        );
        assert_eq!(exec("/bin/ls", &["ls"]), Action::Allow);
    }

    #[test]
    fn validation_and_roundtrip() {
        let c = Config::from_yaml(CONFIG).unwrap();
        let again = Config::from_yaml(&c.to_yaml()).unwrap();
        assert_eq!(c, again);
        let bad = "profiles:\n  a: { extends: b }\n  b: { extends: a }\n";
        assert!(Config::from_yaml(bad)
            .unwrap_err()
            .to_string()
            .contains("cycle"));
        let bad = "policy:\n  net: [{ action: allow, secrets: [nope] }]\n";
        assert!(Config::from_yaml(bad)
            .unwrap_err()
            .to_string()
            .contains("unknown secret"));
    }

    #[test]
    fn rule_ops() {
        let mut c = Config::from_yaml(CONFIG).unwrap();
        assert_eq!(c.find_rule("exec-1"), Some(Scope::Global));
        let scope = c.find_rule("net-3").unwrap();
        assert_eq!(scope, Scope::Profile("base".into()));
        let rule = c.policy_mut(&scope).unwrap().remove("net-3").unwrap();
        assert_eq!(rule.id(), Some("net-3"));
        c.policy.insert(rule, Some(0));
        assert_eq!(c.policy.net[0].id.as_deref(), Some("net-3"));
    }
}
