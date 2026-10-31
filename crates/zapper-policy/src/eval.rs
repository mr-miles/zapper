use std::collections::BTreeMap;

use anyhow::{bail, Context, Result};
use globset::{Glob, GlobBuilder, GlobMatcher};
use serde::{Deserialize, Serialize};
use zapper_proto::ExecRequest;

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
    /// The calling session, for `{session}` in rules' paths: who asks, not
    /// part of the request.
    #[serde(skip)]
    pub session: Option<String>,
    /// The calling session's labels, for `{label:KEY}` in rules' paths.
    #[serde(skip)]
    pub labels: BTreeMap<String, String>,
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
    /// The matched rule's protocol: a database's routes the connection to
    /// the database auth-relay instead of the HTTP/tunnel path.
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

/// A rule's path with `{session}` and `{label:KEY}` replaced by the calling
/// session's ID and labels, each value escaped so it matches only itself.
/// When the request has no such session or label: `None` (the rule then
/// matches nothing), or, `wild` (a deny, which fails closed), any one
/// segment. Other `{…}` stay globset's alternatives.
fn expand(pattern: &str, req: &NetRequest, wild: bool) -> Option<String> {
    let mut out = String::with_capacity(pattern.len());
    let mut rest = pattern;
    while let Some(i) = rest.find('{') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let value = if let Some(after) = tail.strip_prefix("{session}") {
            rest = after;
            req.session.as_deref()
        } else if let Some(key) = tail.strip_prefix("{label:").and_then(|t| t.split_once('}')) {
            rest = key.1;
            req.labels.get(key.0).map(String::as_str)
        } else {
            out.push('{');
            rest = &tail[1..];
            continue;
        };
        let Some(value) = value else {
            if !wild {
                return None;
            }
            out.push('*');
            continue;
        };
        for c in value.chars() {
            // `!` is literal outside a class; inside one (`[!]`) it would
            // start a negated class, so it is left as it is.
            if "*?[]{}\\,".contains(c) {
                out.push('[');
                out.push(c);
                out.push(']');
            } else {
                out.push(c);
            }
        }
    }
    out.push_str(rest);
    Some(out)
}

/// Checks a path's `{label:KEY}` placeholders name a key.
fn check_placeholders(pattern: &str) -> Result<()> {
    let mut rest = pattern;
    while let Some(i) = rest.find("{label:") {
        let tail = &rest[i + "{label:".len()..];
        let Some((key, after)) = tail.split_once('}') else {
            bail!("{{label:… has no closing }}");
        };
        if key.is_empty()
            || !key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            bail!("{{label:{key}}} doesn't name a label: letters, digits, - and _");
        }
        rest = after;
    }
    Ok(())
}

fn args_glob(pattern: &str) -> Result<GlobMatcher> {
    Ok(Glob::new(pattern)?.compile_matcher())
}

/// Checks that every glob in a layer compiles.
pub fn check_globs(policy: &Policy) -> Result<()> {
    for r in &policy.net {
        host_glob(&r.host).with_context(|| format!("host {:?}", r.host))?;
        if let Some(p) = &r.path {
            check_placeholders(p).with_context(|| format!("path {p:?}"))?;
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
            let Some(pattern) = expand(pattern, req, self.action == Action::Deny) else {
                return false;
            };
            let path = path.split(['?', '#']).next().unwrap_or("");
            if !path_glob(&pattern).is_ok_and(|g| g.is_match(path)) {
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
    matches: impl Fn(&R) -> bool,
    default: impl Fn(&Policy) -> Option<Action>,
) -> (Decision, Option<&'a R>) {
    // Final rules first, from the least specific layer down.
    for (scope, policy) in layers.iter().rev() {
        if let Some(r) = rules(policy).iter().find(|r| r.is_final() && matches(r)) {
            return (r.decision(scope), Some(r));
        }
    }
    for (scope, policy) in layers {
        if let Some(r) = rules(policy).iter().find(|r| !r.is_final() && matches(r)) {
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

/// What `evaluate` needs of a rule of either kind.
trait RuleInfo {
    fn action(&self) -> Action;
    fn id(&self) -> Option<&String>;
    fn is_final(&self) -> bool;

    fn decision(&self, scope: &Scope) -> Decision {
        Decision {
            action: self.action(),
            rule: self.id().cloned(),
            scope: Some(scope.clone()),
        }
    }
}

impl RuleInfo for NetRule {
    fn action(&self) -> Action {
        self.action
    }
    fn id(&self) -> Option<&String> {
        self.id.as_ref()
    }
    fn is_final(&self) -> bool {
        self.final_
    }
}

impl RuleInfo for ExecRule {
    fn action(&self) -> Action {
        self.action
    }
    fn id(&self) -> Option<&String> {
        self.id.as_ref()
    }
    fn is_final(&self) -> bool {
        self.final_
    }
}

/// Evaluates a connection or request. Effects are only set when allowed.
pub fn evaluate_net(layers: &Layers<'_>, req: &NetRequest) -> (Decision, NetEffects) {
    let (decision, rule) = evaluate(layers, |p| &p.net, |r| r.matches(req), |p| p.defaults.net);
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
    let mut settled = false;
    for r in finals.chain(others).filter(|r| host_matches(&r.host, name)) {
        match r.action {
            Action::Allow => allow = true,
            Action::Ask => ask = true,
            Action::Deny => {}
        }
        if unconditional(r) {
            settled = true;
            break;
        }
    }
    if !settled {
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
    evaluate(layers, |p| &p.exec, |r| r.matches(req), |p| p.defaults.exec).0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Config;

    fn rule_with_path(path: &str) -> NetRule {
        serde_yaml::from_str(&format!(
            "{{ action: allow, host: drive.internal, path: {path:?} }}"
        ))
        .unwrap()
    }

    fn get(path: &str, session: Option<&str>, labels: &[(&str, &str)]) -> NetRequest {
        NetRequest {
            host: "drive.internal".into(),
            port: 443,
            method: Some("GET".into()),
            path: Some(path.into()),
            session: session.map(String::from),
            labels: labels
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    /// `{session}` and `{label:KEY}` stand for the calling session's ID and
    /// labels; a request without them matches nothing, and other braces stay
    /// globset's alternatives.
    #[test]
    fn path_placeholders_are_the_callers() {
        let own = rule_with_path("/outbox/{session}/**");
        assert!(own.matches(&get("/outbox/ab3cd4/x.csv", Some("ab3cd4"), &[])));
        assert!(!own.matches(&get("/outbox/zz9yy8/x.csv", Some("ab3cd4"), &[])));
        assert!(!own.matches(&get("/outbox/ab3cd4/x.csv", None, &[])));

        let team = rule_with_path("/teams/{label:team}/**");
        assert!(team.matches(&get("/teams/a/notes.md", Some("s1"), &[("team", "a")])));
        assert!(!team.matches(&get("/teams/b/notes.md", Some("s1"), &[("team", "a")])));
        assert!(!team.matches(&get("/teams/a/notes.md", Some("s1"), &[])));

        let either = rule_with_path("/{inbox,shared}/**");
        assert!(either.matches(&get("/shared/x", Some("s1"), &[])));
        assert!(!either.matches(&get("/outbox/x", Some("s1"), &[])));
    }

    /// Every printable character alone, and every pair of the characters a
    /// glob gives meaning to, matches itself and no other such value, as
    /// the fuzz target `placeholder` checks for any value.
    #[test]
    fn a_short_value_matches_itself_and_no_other() {
        let team = rule_with_path("/t/{label:team}/**");
        // `?` ends a path (the query starts), so no segment holds it.
        let special: Vec<char> = "*[]{}\\,!-^".chars().collect();
        let mut values: Vec<String> = (0x21u8..0x7f)
            .map(char::from)
            .filter(|c| !"/?#".contains(*c))
            .map(String::from)
            .collect();
        for a in &special {
            for b in &special {
                values.push(format!("{a}{b}"));
            }
        }
        let req = |path: String, v: &str| get(&path, Some("s1"), &[("team", v)]);
        for v in &values {
            assert!(team.matches(&req(format!("/t/{v}/x"), v)), "{v:?}");
        }
        let specials: Vec<String> = values
            .iter()
            .filter(|v| v.chars().all(|c| special.contains(&c)))
            .cloned()
            .collect();
        for v in specials.iter().filter(|v| v.len() == 1) {
            for w in &specials {
                if w != v {
                    assert!(
                        !team.matches(&req(format!("/t/{w}/x"), v)),
                        "{v:?} matched {w:?}"
                    );
                }
            }
        }
    }

    /// A deny whose placeholder the request can't fill (a session without
    /// the label, a request without its session) fails closed: it applies
    /// to any value there. An allow fails open to nothing, as before.
    #[test]
    fn a_deny_with_a_placeholder_it_cant_fill_applies_to_any() {
        let deny: NetRule = serde_yaml::from_str(
            "{ action: deny, host: drive.internal, path: \"/teams/{label:team}/secret/**\" }",
        )
        .unwrap();
        assert!(deny.matches(&get("/teams/b/secret/x", Some("s1"), &[])));
        assert!(deny.matches(&get("/teams/b/secret/x", None, &[])));
        assert!(!deny.matches(&get("/teams/b/open/x", Some("s1"), &[])));
        assert!(deny.matches(&get("/teams/a/secret/x", Some("s1"), &[("team", "a")])));
        assert!(!deny.matches(&get("/teams/b/secret/x", Some("s1"), &[("team", "a")])));
        let allow = rule_with_path("/teams/{label:team}/**");
        assert!(!allow.matches(&get("/teams/b/x", Some("s1"), &[])));
    }

    /// A substituted value matches only itself, whatever it holds.
    #[test]
    fn placeholder_values_are_literal() {
        let team = rule_with_path("/teams/{label:team}/**");
        assert!(!team.matches(&get("/teams/a/x", Some("s1"), &[("team", "*")])));
        assert!(team.matches(&get("/teams/*/x", Some("s1"), &[("team", "*")])));
        assert!(!team.matches(&get("/teams/a/x", Some("s1"), &[("team", "{a,b}")])));
    }

    /// Every character a glob gives meaning to, in a placeholder's value,
    /// matches only itself: the rule still compiles, matches the value, and
    /// matches nothing else.
    #[test]
    fn every_glob_character_in_a_value_is_literal() {
        let team = rule_with_path("/t/{label:team}/**");
        // (`?` and `#` can't be in a path: what follows them isn't matched.)
        for v in [
            "a!b", "a*b", "a[b", "a]b", "a\\b", "a,b", "a{b", "a}b", "[!x]", "!", "x-_.y",
        ] {
            let path = format!("/t/{v}/x");
            assert!(
                team.matches(&get(&path, Some("s1"), &[("team", v)])),
                "{v:?} doesn't match itself"
            );
            assert!(
                !team.matches(&get("/t/aXb/x", Some("s1"), &[("team", v)])),
                "{v:?} matches another value"
            );
        }
    }

    #[test]
    fn label_placeholders_name_a_key() {
        let check = |path: &str| {
            Config::from_yaml(&format!(
                "policy:\n  net:\n    - {{ action: allow, host: x.internal, path: {path:?} }}\n"
            ))
            .map(|_| ())
            .map_err(|e| format!("{e:#}"))
        };
        check("/t/{label:team}/**").unwrap();
        check("/o/{session}/**").unwrap();
        for bad in ["/t/{label:}/x", "/t/{label:a b}/x", "/t/{label:team"] {
            let e = check(bad).unwrap_err();
            assert!(e.contains("label"), "{bad}: {e}");
        }
    }

    /// Host patterns for one AWS service stop at that service, and no
    /// pattern ending in a domain matches a look-alike that merely contains
    /// it (the cases OneCLI tests for its AWS grants).
    #[test]
    fn aws_host_patterns_reach_only_what_they_name() {
        // A whole-account secret: every AWS host, nothing else.
        let all = "*.amazonaws.com";
        for host in [
            "s3.us-east-1.amazonaws.com",
            "iam.amazonaws.com",
            "S3.EU-WEST-2.AMAZONAWS.COM",
            "sts.amazonaws.com.",
        ] {
            assert!(host_matches(all, host), "{host}");
        }
        for host in [
            "s3.us-east-1.amazonaws.com.evil.test",
            "amazonaws.com.evil.test",
            "evilamazonaws.com",
            "amazonaws.com",
        ] {
            assert!(!host_matches(all, host), "{host}");
        }
        // S3 alone, path-style and virtual-hosted: not its sibling services.
        let s3 = ["s3.*.amazonaws.com", "*.s3.*.amazonaws.com"];
        let any = |host: &str| s3.iter().any(|p| host_matches(p, host));
        for host in [
            "s3.us-east-1.amazonaws.com",
            "bucket.s3.eu-west-2.amazonaws.com",
            "s3.dualstack.us-east-1.amazonaws.com",
        ] {
            assert!(any(host), "{host}");
        }
        for host in [
            "s3tables.us-east-1.amazonaws.com",
            "s3-control.us-east-1.amazonaws.com",
            "s3express-control.us-east-1.amazonaws.com",
            "ec2.us-east-1.amazonaws.com",
            "iam.amazonaws.com",
            "sts.us-east-1.amazonaws.com",
            "s3.us-east-1.amazonaws.com.evil.test",
            "bucket.s3.us-east-1.amazonaws.com.evil.test",
        ] {
            assert!(!any(host), "{host}");
        }
        // The tempting `s3*` does swallow the siblings: what not to write.
        assert!(host_matches(
            "s3*.amazonaws.com",
            "s3tables.us-east-1.amazonaws.com"
        ));
        // A secret's hosts match the same way.
        let secret: crate::Secret =
            serde_yaml::from_str("{ file: /x, mode: aws, hosts: ['s3.*.amazonaws.com'] }").unwrap();
        assert!(secret.allows_host("s3.us-east-1.amazonaws.com"));
        assert!(!secret.allows_host("s3tables.us-east-1.amazonaws.com"));
        assert!(!secret.allows_host("s3.us-east-1.amazonaws.com.evil.test"));
    }

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
            ..Default::default()
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
            ..Default::default()
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
