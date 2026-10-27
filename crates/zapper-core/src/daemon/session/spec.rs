//! Renders a profile into the spec the guest sets itself up from.
//!
//! `proxy` secrets appear here only as placeholders; `guest` secrets as their
//! values.

use std::collections::BTreeMap;

use anyhow::{anyhow, Context, Result};
use zapper_policy::proto::{GuestDisk, GuestFile, GuestSpec};
use zapper_policy::{
    placeholder, render, secret_refs, Config, FsRule, NetworkMode, ResolvedProfile, SecretMode,
};

const CA_PATH: &str = "/usr/local/share/ca-certificates/zapper.crt";
const PROXY: &str = "http://127.0.0.1:3128";

/// Renders `{{secret:NAME}}`: placeholders for proxy secrets, values for
/// guest secrets.
fn render_secrets(config: &Config, template: &str) -> Result<String> {
    let mut err = None;
    let out = render(template, |name| match config.secrets.get(name) {
        Some(s) if s.mode == SecretMode::Guest => s.source.resolve().unwrap_or_else(|e| {
            err.get_or_insert(e.context(format!("secret {name}")));
            String::new()
        }),
        Some(_) => placeholder(name),
        None => {
            err.get_or_insert(anyhow!("unknown secret {name}"));
            String::new()
        }
    });
    match err {
        Some(e) => Err(e),
        None => Ok(out),
    }
}

pub fn build(
    config: &Config,
    profile: &ResolvedProfile,
    session: &str,
    ca_pem: &str,
    disks: Vec<GuestDisk>,
    extra_files: Vec<GuestFile>,
    forwards: Vec<zapper_policy::proto::Forward>,
) -> Result<GuestSpec> {
    let agent = &profile.agent;
    let home = format!("/home/{}", agent.user);
    let nic = profile.engine.network == NetworkMode::Nic;
    let mut env: BTreeMap<String, String> = BTreeMap::new();
    if !nic {
        // In vsock mode, programs reach the network through the forwarder.
        for k in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"] {
            env.insert(k.into(), PROXY.into());
        }
        env.insert("NO_PROXY".into(), "localhost,127.0.0.1".into());
        env.insert("no_proxy".into(), "localhost,127.0.0.1".into());
    }
    for (k, v) in [
        ("NODE_EXTRA_CA_CERTS", CA_PATH),
        ("SSL_CERT_FILE", "/etc/ssl/certs/ca-certificates.crt"),
        ("REQUESTS_CA_BUNDLE", "/etc/ssl/certs/ca-certificates.crt"),
        ("ZAPPER_SESSION", session),
    ] {
        env.insert(k.into(), v.into());
    }
    let mut files = Vec::new();
    for (k, v) in &agent.env {
        env.insert(
            k.clone(),
            render_secrets(config, v).with_context(|| format!("agent.env.{k}"))?,
        );
    }
    let harness = crate::daemon::harness::get(&agent.kind)
        .ok_or_else(|| anyhow!("agent.kind {:?} is not a harness zapper knows", agent.kind))?;
    harness.guest(profile, &home, &mut env, &mut files);
    // `files` by guest path, and `config_files` in the harness's config
    // directory: both may hold secrets.
    let config_dir = harness.config_dir(&home);
    let all = agent.files.iter().map(|(p, c)| (p.clone(), c)).chain(
        agent
            .config_files
            .iter()
            .map(|(p, c)| (format!("{config_dir}/{}", p.trim_start_matches('/')), c)),
    );
    for (path, content) in all {
        let path = match path.strip_prefix("~/") {
            Some(rest) => format!("{home}/{rest}"),
            None => path,
        };
        let user_owned = path.starts_with(&home);
        // Files holding guest secrets are private to the user.
        let holds_secret = secret_refs(content).iter().any(|n| {
            config
                .secrets
                .get(n)
                .is_some_and(|s| s.mode == SecretMode::Guest)
        });
        let mode = if holds_secret { 0o600 } else { 0o644 };
        let content =
            render_secrets(config, content).with_context(|| format!("agent.files.{path}"))?;
        files.push(GuestFile {
            path,
            content,
            mode,
            user_owned,
        });
    }
    files.extend(extra_files);
    // All fs rules, most specific layer first; the guest applies them all.
    let mut fs: Vec<FsRule> = Vec::new();
    for (_, p) in &profile.layers {
        fs.extend(p.fs.iter().cloned());
    }
    fs.extend(config.policy.fs.iter().cloned());
    Ok(GuestSpec {
        session: session.into(),
        nic,
        hostname: format!("zapper-{session}"),
        user: agent.user.clone(),
        root: agent.root,
        home,
        workdir: agent.workdir.clone(),
        env,
        disks,
        files,
        fs,
        setup: profile.setup.clone(),
        ca_pem: ca_pem.into(),
        supervise_exec: true,
        forwards,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::harness::MANAGED_SETTINGS;
    use serde_json::Value;

    #[test]
    fn permission_mode_goes_to_managed_settings() {
        let config = Config::from_yaml("profiles:\n  c:\n    agent: { kind: claude-code, permission_mode: bypassPermissions, model: m }\n").unwrap();
        let profile = config.resolve("c").unwrap();
        let spec = build(&config, &profile, "s", "", vec![], vec![], vec![]).unwrap();
        let file = |p: &str| {
            spec.files
                .iter()
                .find(|f| f.path == p)
                .map(|f| serde_json::from_str::<Value>(&f.content).unwrap())
        };
        let user = file("/home/agent/.claude/settings.json").unwrap();
        assert_eq!(user["model"], "m");
        assert!(
            user.get("permissions").is_none(),
            "no defaultMode in user settings: {user}"
        );
        let managed = file(MANAGED_SETTINGS).unwrap();
        assert_eq!(managed["permissions"]["defaultMode"], "bypassPermissions");
        assert_eq!(managed["skipDangerousModePermissionPrompt"], true);
        assert!(
            !spec
                .files
                .iter()
                .find(|f| f.path == MANAGED_SETTINGS)
                .unwrap()
                .user_owned
        );
    }

    /// A `guest` secret's value reaches only the sessions whose profile
    /// uses it, in a private file there; a `proxy` secret's never does.
    #[test]
    fn guest_secrets_reach_only_profiles_that_use_them() {
        let config = Config::from_yaml(
            "secrets:\n  g: { value: guest-value-1, mode: guest }\n  p: { value: proxy-value-2, hosts: [api.example] }\nprofiles:\n  uses:\n    agent: { kind: generic, env: { G: \"{{secret:g}}\", P: \"{{secret:p}}\" }, files: { \"~/key\": \"{{secret:g}}\" } }\n  other:\n    agent: { kind: generic, env: { X: plain } }\n",
        )
        .unwrap();
        let spec = |p: &str| {
            let profile = config.resolve(p).unwrap();
            build(&config, &profile, "s", "", vec![], vec![], vec![]).unwrap()
        };
        let uses = spec("uses");
        assert_eq!(uses.env["G"], "guest-value-1");
        assert_eq!(uses.env["P"], placeholder("p"));
        let key = uses
            .files
            .iter()
            .find(|f| f.path == "/home/agent/key")
            .unwrap();
        assert_eq!((key.content.as_str(), key.mode), ("guest-value-1", 0o600));
        let all = serde_json::to_string(&uses).unwrap();
        assert!(!all.contains("proxy-value-2"), "{all}");
        let other = serde_json::to_string(&spec("other")).unwrap();
        assert!(
            !other.contains("guest-value-1") && !other.contains("proxy-value-2"),
            "{other}"
        );
    }
}
