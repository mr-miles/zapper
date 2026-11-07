//! zapper's network rules as an OpenShell sandbox's, and the rules a
//! sandbox's network drafts as the officers see them (phase 3 of
//! `docs/openshell-migration.md`).
//!
//! The office keeps the rules as it always has (a worker's own layer, its
//! role's, its profile's, the global one) and gives each sandbox, as rules
//! of its own policy, the allows among them. OpenShell denies what no rule
//! allows, so a zapper `deny` or `ask` needs no rule there: the refused
//! connection becomes a drafted rule the officers review, as a proposal the
//! worker makes through `policy.local` does.

use std::collections::{BTreeMap, BTreeSet};

use openshell_sdk::raw::proto;
use serde_json::{json, Value};
use zapper_policy::{Action, NetProtocol, NetRule, Policy};

use crate::client::DraftRule;

/// The prefix of the keys the office's rules have in a sandbox's policy.
pub const PREFIX: &str = "zo_";
/// The programs a rule lets through when nothing narrower is configured:
/// zapper's rules name none.
pub const ANY_BINARY: &str = "/**";
/// The ports a rule that names none opens (zapper's: any).
const WEB_PORTS: [u16; 2] = [443, 80];

/// The key of zapper rule `id` in a sandbox's policy.
pub fn key(id: &str) -> String {
    let id: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("{PREFIX}{id}")
}

/// What a sandbox's rules are made from: its layers, most specific first, and
/// what `{session}` and `{label:KEY}` in them stand for.
pub struct Layers<'a> {
    pub layers: Vec<(String, &'a Policy)>,
    pub session: &'a str,
    pub labels: &'a BTreeMap<String, String>,
    pub binaries: &'a [String],
}

/// The sandbox's rules, by key. A layer's allow is left out where a more
/// specific layer denies the same host on a port it opens (OpenShell allows
/// what any rule allows, so the deny wouldn't hold), as is anything
/// OpenShell can't hold (see [`to_gateway`]).
pub fn sandbox_rules(l: &Layers) -> BTreeMap<String, proto::NetworkPolicyRule> {
    let mut out = BTreeMap::new();
    let mut denied: Vec<(String, Vec<u16>)> = Vec::new();
    for (scope, policy) in &l.layers {
        for (i, rule) in policy.net.iter().enumerate() {
            if rule.action != Action::Allow {
                denied.push((rule.host.to_ascii_lowercase(), rule.ports.clone()));
                continue;
            }
            let shadowed = denied.iter().any(|(host, ports)| {
                *host == rule.host.to_ascii_lowercase()
                    && (ports.is_empty()
                        || rule.ports.is_empty()
                        || rule.ports.iter().any(|p| ports.contains(p)))
            });
            if shadowed {
                continue;
            }
            let id = rule.id.clone().unwrap_or_else(|| format!("{scope}-{i}"));
            let Some(expanded) = expand_rule(rule, l.session, l.labels) else {
                continue;
            };
            if let Some(r) = to_gateway(&expanded, &key(&id), l.binaries) {
                out.entry(key(&id)).or_insert(r);
            }
        }
    }
    out
}

/// `rule` with `{session}` and `{label:KEY}` replaced by the sandbox's;
/// `None` if it names a label the sandbox doesn't have.
fn expand_rule(
    rule: &NetRule,
    session: &str,
    labels: &BTreeMap<String, String>,
) -> Option<NetRule> {
    let mut r = rule.clone();
    r.host = expand(&r.host, session, labels)?;
    if let Some(p) = &r.path {
        r.path = Some(expand(p, session, labels)?);
    }
    Some(r)
}

fn expand(s: &str, session: &str, labels: &BTreeMap<String, String>) -> Option<String> {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('{') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let end = tail.find('}')?;
        let name = &tail[1..end];
        if name == "session" {
            out.push_str(session);
        } else {
            // A label; anything else is a glob's alternatives, which
            // OpenShell's globs don't have.
            let k = name.strip_prefix("label:")?;
            out.push_str(labels.get(k)?);
        }
        rest = &tail[end + 1..];
    }
    out.push_str(rest);
    Some(out)
}

/// zapper allow `rule` as an OpenShell network rule named `name`, or `None`
/// if it doesn't become one:
///
/// - denials and asks (OpenShell denies what no rule allows);
/// - the firm's git and cluster (`credentials-…`, which follow grants): the
///   office edge serves git, and its grants are checked there;
/// - rules that put secrets or headers in requests (the gateway's providers
///   hold the credentials) or speak a database's protocol;
/// - hosts OpenShell's patterns can't say: `*` alone, or a wildcard in a name
///   of fewer than three labels.
pub fn to_gateway(
    rule: &NetRule,
    name: &str,
    binaries: &[String],
) -> Option<proto::NetworkPolicyRule> {
    if rule.action != Action::Allow
        || rule
            .id
            .as_deref()
            .is_some_and(|i| i.starts_with("credentials-"))
        || !rule.secrets.is_empty()
        || !rule.set_headers.is_empty()
        || rule.protocol != NetProtocol::Tcp
    {
        return None;
    }
    let host = rule.host.to_ascii_lowercase();
    if host.is_empty() || (host.contains('*') && host.split('.').count() < 3) {
        return None;
    }
    let ports: Vec<u32> = if rule.ports.is_empty() {
        WEB_PORTS.iter().map(|&p| p.into()).collect()
    } else {
        rule.ports.iter().map(|&p| p.into()).collect()
    };
    let mut endpoint = proto::NetworkEndpoint {
        host,
        ..Default::default()
    };
    if let [port] = ports[..] {
        endpoint.port = port;
    } else {
        endpoint.ports = ports;
    }
    if !rule.methods.is_empty() || rule.path.is_some() {
        rest(&mut endpoint, &rule.methods, rule.path.as_deref());
    }
    let binaries = if binaries.is_empty() {
        vec![ANY_BINARY.to_string()]
    } else {
        binaries.to_vec()
    };
    Some(proto::NetworkPolicyRule {
        name: name.to_string(),
        endpoints: vec![endpoint],
        binaries: binaries
            .into_iter()
            .map(|path| proto::NetworkBinary { path })
            .collect(),
    })
}

/// Makes `endpoint` inspect its requests and allow only `methods` (any, if
/// none) on `path` (any, if none).
fn rest(endpoint: &mut proto::NetworkEndpoint, methods: &[String], path: Option<&str>) {
    let path = path.unwrap_or("/**").to_string();
    let methods: Vec<String> = if methods.is_empty() {
        vec!["*".into()]
    } else {
        methods.iter().map(|m| m.to_ascii_uppercase()).collect()
    };
    endpoint.protocol = "rest".into();
    endpoint.enforcement = proto::NetworkEnforcementMode::Enforce as i32;
    endpoint.access = proto::NetworkAccessPreset::Unspecified as i32;
    endpoint.rules = methods
        .into_iter()
        .map(|method| proto::L7Rule {
            allow: Some(proto::L7Allow {
                method,
                path: path.clone(),
                ..Default::default()
            }),
        })
        .collect();
}

/// A drafted rule (`chunk`) of sandbox `session` as the officers see it;
/// `None` for one with no rule or endpoint.
pub fn draft(session: &str, chunk: &proto::PolicyChunk) -> Option<DraftRule> {
    let rule = chunk.proposed_rule.as_ref()?;
    let ep = rule.endpoints.first()?;
    let port = if ep.port != 0 {
        ep.port
    } else {
        ep.ports.first().copied().unwrap_or(0)
    };
    let mut methods = BTreeSet::new();
    let mut path = None;
    for r in rule.endpoints.iter().flat_map(|e| &e.rules) {
        if let Some(a) = &r.allow {
            if !a.method.is_empty() {
                methods.insert(a.method.to_ascii_uppercase());
            }
            if path.is_none() && !a.path.is_empty() {
                path = Some(a.path.clone());
            }
        }
    }
    let findings: Vec<&str> = [
        chunk.security_notes.as_str(),
        chunk.validation_result.as_str(),
        chunk.application_error.as_str(),
    ]
    .into_iter()
    .filter(|s| !s.trim().is_empty())
    .collect();
    Some(DraftRule {
        session: session.to_string(),
        chunk: chunk.id.clone(),
        host: ep.host.clone(),
        port: u16::try_from(port).unwrap_or(0),
        methods: methods.into_iter().collect(),
        path,
        binaries: rule.binaries.iter().map(|b| b.path.clone()).collect(),
        rationale: chunk.rationale.chars().take(2000).collect(),
        findings: findings.join("\n").chars().take(2000).collect(),
        proposed: chunk.stage == "agent",
        rule: rule_json(rule),
    })
}

/// A rule as the officers read it.
pub fn rule_json(rule: &proto::NetworkPolicyRule) -> Value {
    let endpoints: Vec<Value> = rule
        .endpoints
        .iter()
        .map(|e| {
            let mut v = json!({ "host": e.host });
            if e.port != 0 {
                v["port"] = json!(e.port);
            }
            if !e.ports.is_empty() {
                v["ports"] = json!(e.ports);
            }
            if !e.protocol.is_empty() {
                v["protocol"] = json!(e.protocol);
            }
            match proto::NetworkAccessPreset::try_from(e.access) {
                Ok(proto::NetworkAccessPreset::ReadOnly) => v["access"] = json!("read-only"),
                Ok(proto::NetworkAccessPreset::ReadWrite) => v["access"] = json!("read-write"),
                Ok(proto::NetworkAccessPreset::Full) => v["access"] = json!("full"),
                _ => {}
            }
            if e.enforcement == proto::NetworkEnforcementMode::Audit as i32 {
                v["enforcement"] = json!("audit");
            }
            if e.tls == proto::NetworkTlsMode::Skip as i32 {
                v["tls"] = json!("skip");
            }
            let allow: Vec<Value> = e
                .rules
                .iter()
                .filter_map(|r| r.allow.as_ref())
                .map(|a| json!({ "method": a.method, "path": a.path }))
                .collect();
            if !allow.is_empty() {
                v["allow"] = json!(allow);
            }
            if !e.deny_rules.is_empty() {
                v["deny_rules"] = json!(e.deny_rules.len());
            }
            if !e.allowed_ips.is_empty() {
                v["allowed_ips"] = json!(e.allowed_ips);
            }
            v
        })
        .collect();
    json!({
        "endpoints": endpoints,
        "binaries": rule.binaries.iter().map(|b| b.path.as_str()).collect::<Vec<_>>(),
    })
}

/// `rule` narrowed as an officer approves it: `methods` (`[]`: the whole
/// connection, uninspected) and `path` replace what each endpoint allows.
pub fn narrow(
    rule: &proto::NetworkPolicyRule,
    methods: Option<&[String]>,
    path: Option<&str>,
) -> proto::NetworkPolicyRule {
    let mut r = rule.clone();
    for e in &mut r.endpoints {
        if methods.is_some_and(|m| m.is_empty()) {
            e.protocol.clear();
            e.rules.clear();
            e.deny_rules.clear();
            e.access = proto::NetworkAccessPreset::Unspecified as i32;
            e.enforcement = proto::NetworkEnforcementMode::Unspecified as i32;
            continue;
        }
        let have: Vec<String> = e
            .rules
            .iter()
            .filter_map(|r| r.allow.as_ref())
            .map(|a| a.method.clone())
            .filter(|m| !m.is_empty())
            .collect();
        let have_path = e
            .rules
            .iter()
            .filter_map(|r| r.allow.as_ref())
            .map(|a| a.path.clone())
            .find(|p| !p.is_empty());
        let methods = methods.map(<[String]>::to_vec).unwrap_or(have);
        let path = path.map(str::to_string).or(have_path);
        rest(e, &methods, path.as_deref());
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allow(id: &str, host: &str, ports: &[u16], methods: &[&str], path: Option<&str>) -> NetRule {
        NetRule {
            id: Some(id.into()),
            action: Action::Allow,
            host: host.into(),
            ports: ports.to_vec(),
            methods: methods.iter().map(|m| m.to_string()).collect(),
            path: path.map(str::to_string),
            protocol: Default::default(),
            secrets: vec![],
            set_headers: Default::default(),
            final_: false,
            comment: None,
        }
    }

    #[test]
    fn a_host_and_port_is_a_connection_and_methods_make_it_inspected() {
        let r = to_gateway(
            &allow("a-net-1", "pypi.org", &[443], &[], None),
            "zo_a_net_1",
            &[],
        )
        .unwrap();
        assert_eq!(r.name, "zo_a_net_1");
        assert_eq!(r.binaries[0].path, ANY_BINARY);
        let e = &r.endpoints[0];
        assert_eq!((e.host.as_str(), e.port), ("pypi.org", 443));
        assert!(e.protocol.is_empty() && e.rules.is_empty(), "uninspected");

        let r = to_gateway(
            &allow(
                "x",
                "api.github.com",
                &[443],
                &["get", "PUT"],
                Some("/repos/**"),
            ),
            "k",
            &["/usr/bin/gh".into()],
        )
        .unwrap();
        let e = &r.endpoints[0];
        assert_eq!(e.protocol, "rest");
        assert_eq!(e.enforcement, proto::NetworkEnforcementMode::Enforce as i32);
        let rules: Vec<(&str, &str)> = e
            .rules
            .iter()
            .map(|r| r.allow.as_ref().unwrap())
            .map(|a| (a.method.as_str(), a.path.as_str()))
            .collect();
        assert_eq!(rules, [("GET", "/repos/**"), ("PUT", "/repos/**")]);
        assert_eq!(r.binaries[0].path, "/usr/bin/gh");

        let any = to_gateway(
            &allow("x", "example.com", &[], &[], Some("/docs/*")),
            "k",
            &[],
        )
        .unwrap();
        assert_eq!(
            any.endpoints[0].ports,
            [443, 80],
            "zapper's any port is the web's"
        );
        assert_eq!(
            any.endpoints[0].rules[0].allow.as_ref().unwrap().method,
            "*"
        );
    }

    #[test]
    fn what_openshell_cant_hold_stays_out() {
        let mut deny = allow("d", "evil.com", &[443], &[], None);
        deny.action = Action::Deny;
        let mut secret = allow("s", "api.example.com", &[443], &[], None);
        secret.secrets = vec!["token".into()];
        for r in [
            deny,
            secret,
            allow(
                "credentials-git-api",
                "git.acme.internal",
                &[443],
                &[],
                None,
            ),
            allow("w", "*", &[443], &[], None),
            allow("w", "*.com", &[443], &[], None),
        ] {
            assert!(to_gateway(&r, "k", &[]).is_none(), "{r:?}");
        }
        assert!(to_gateway(&allow("w", "*.github.com", &[443], &[], None), "k", &[]).is_some());
    }

    #[test]
    fn layers_become_one_sandbox_policy_and_a_nearer_deny_holds() {
        let labels = BTreeMap::from([("member".to_string(), "pat".to_string())]);
        let mut own = Policy::default();
        let mut d = allow("d-1", "files.example.com", &[443], &[], None);
        d.action = Action::Deny;
        own.net.push(d);
        own.net.push(allow(
            "a-net-2",
            "{label:member}.example.com",
            &[443],
            &[],
            None,
        ));
        own.net.push(allow(
            "a-net-3",
            "{label:nobody}.example.com",
            &[443],
            &[],
            None,
        ));
        let mut role = Policy::default();
        role.net
            .push(allow("a-net-1", "files.example.com", &[443], &[], None));
        role.net
            .push(allow("a-net-4", "pypi.org", &[443], &["GET"], None));
        let mut global = Policy::default();
        global.net.push(NetRule {
            id: None,
            ..allow("", "registry.npmjs.org", &[443], &[], None)
        });
        let l = Layers {
            layers: vec![
                ("session".into(), &own),
                ("role".into(), &role),
                ("global".into(), &global),
            ],
            session: "zo-1",
            labels: &labels,
            binaries: &[],
        };
        let rules = sandbox_rules(&l);
        let keys: Vec<&str> = rules.keys().map(String::as_str).collect();
        assert_eq!(keys, ["zo_a_net_2", "zo_a_net_4", "zo_global_0"]);
        assert_eq!(rules["zo_a_net_2"].endpoints[0].host, "pat.example.com");
    }

    fn chunk(stage: &str) -> proto::PolicyChunk {
        proto::PolicyChunk {
            id: "c-1".into(),
            status: "pending".into(),
            rule_name: "github_contents".into(),
            proposed_rule: Some(proto::NetworkPolicyRule {
                name: "github_contents".into(),
                endpoints: vec![proto::NetworkEndpoint {
                    host: "api.github.com".into(),
                    port: 443,
                    protocol: "rest".into(),
                    enforcement: proto::NetworkEnforcementMode::Enforce as i32,
                    rules: vec![proto::L7Rule {
                        allow: Some(proto::L7Allow {
                            method: "put".into(),
                            path: "/repos/acme/api/contents/**".into(),
                            ..Default::default()
                        }),
                    }],
                    ..Default::default()
                }],
                binaries: vec![proto::NetworkBinary {
                    path: "/usr/bin/gh".into(),
                }],
            }),
            rationale: "Update the docs for t-12".into(),
            validation_result: "credential_reach_expansion".into(),
            stage: stage.into(),
            ..Default::default()
        }
    }

    #[test]
    fn a_draft_shows_the_rule_it_would_apply() {
        let d = draft("zo-1", &chunk("agent")).unwrap();
        assert_eq!((d.host.as_str(), d.port), ("api.github.com", 443));
        assert_eq!(d.methods, ["PUT"]);
        assert_eq!(d.path.as_deref(), Some("/repos/acme/api/contents/**"));
        assert_eq!(d.binaries, ["/usr/bin/gh"]);
        assert!(d.proposed);
        assert_eq!(d.findings, "credential_reach_expansion");
        assert_eq!(d.rule["endpoints"][0]["allow"][0]["method"], "put");
        assert!(
            !draft("zo-1", &chunk("initial")).unwrap().proposed,
            "drafted from a refusal"
        );
    }

    #[test]
    fn an_officer_narrows_a_draft_or_opens_the_connection() {
        let rule = chunk("agent").proposed_rule.unwrap();
        let get = narrow(&rule, Some(&["GET".into()]), None);
        let a = get.endpoints[0].rules[0].allow.as_ref().unwrap();
        assert_eq!(
            (a.method.as_str(), a.path.as_str()),
            ("GET", "/repos/acme/api/contents/**")
        );
        let docs = narrow(&rule, None, Some("/repos/acme/api/contents/docs/**"));
        let a = docs.endpoints[0].rules[0].allow.as_ref().unwrap();
        assert_eq!(
            (a.method.as_str(), a.path.as_str()),
            ("PUT", "/repos/acme/api/contents/docs/**")
        );
        let open = narrow(&rule, Some(&[]), None);
        assert!(open.endpoints[0].protocol.is_empty() && open.endpoints[0].rules.is_empty());
    }

    #[test]
    fn keys_are_names_the_gateway_takes() {
        assert_eq!(key("a-net-12"), "zo_a_net_12");
        assert_eq!(key("Role:eng/1"), "zo_role_eng_1");
    }
}
