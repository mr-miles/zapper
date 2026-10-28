//! A worker's GitHub access: their grant becomes a token, held by the
//! daemon and put into the worker's requests to GitHub at the network.
//!
//! A worker holds one repository at one level (the policy owners hold every
//! repository of the firm at the level `read`), and their token is minted
//! for exactly that: `zapper mint github-app` as a secret of their session
//! (`gh-<worker>`), refreshed ahead of its hour by the proxy. Two rules on
//! the worker's layer set `Authorization` on every request to `github.com`
//! (git over HTTPS) and `api.github.com` (REST and GraphQL), replacing
//! anything the guest sent, so nothing in the VM holds or names the token.
//!
//! The token is the boundary, not the rules' paths: GraphQL has one URL, and
//! `gh` needs it.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::json;
use zapper_core::Core;
use zapper_policy::{mint, Action, AnyRule, NetRule, Secret, SecretMode, SecretSource};

use super::FirmRun;

/// The firm for a run that starts now: a firm in GitHub gets the account its
/// App is installed on and the repositories the App reaches there, listed from
/// GitHub, unless some were given outright, which are checked instead.
pub async fn with_repositories(mut firm: crate::firm::Firm) -> anyhow::Result<crate::firm::Firm> {
    let Some(gh) = firm.git.as_ref().and_then(|g| g.github.as_ref()) else {
        return Ok(firm);
    };
    let file = zapper_github::app_file(&gh.config)?;
    let (owner, repos) =
        resolve_repositories(&zapper_github::GitHub::default(), &file, &gh.repos).await?;
    if let Some(gh) = firm.git.as_mut().and_then(|g| g.github.as_mut()) {
        gh.owner = owner;
        gh.repos = repos;
    }
    Ok(firm)
}

/// The account the App in `file` is installed on, and its repositories: every
/// one the App reaches (sorted, as GitHub spells them), or the `given` ones,
/// which must be among them. A missing file, an App installed nowhere or on
/// several accounts, a repository it doesn't reach, or more repositories than
/// one token can name are errors that say what to do.
pub async fn resolve_repositories(
    gh: &zapper_github::GitHub,
    file: &std::path::Path,
    given: &[String],
) -> anyhow::Result<(String, Vec<String>)> {
    if !file.exists() {
        anyhow::bail!(
            "no GitHub App at {}; create it with `zapper github setup --org <account>` (or --user <login>)",
            file.display()
        );
    }
    let app = zapper_github::App::load(file, &mut |w| tracing::warn!("{w}"))?;
    let inst = gh.the_installation(&app).await?;
    let owner = inst.account.clone().unwrap_or_default();
    let mut reach = gh.installation_repositories(&app, inst.id).await?;
    reach.sort_by_key(|r| r.to_ascii_lowercase());
    reach.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    if given.is_empty() {
        if reach.len() > zapper_github::MAX_REPOS {
            anyhow::bail!(
                "the App reaches {} repositories of {owner}; a token can name at most {}: limit the installation to the repositories agents should reach",
                reach.len(),
                zapper_github::MAX_REPOS
            );
        }
        return Ok((owner, reach));
    }
    let mut chosen: Vec<String> = Vec::new();
    let mut missing = Vec::new();
    for r in given {
        let (o, _) = zapper_github::split_repo(r)?;
        if !o.eq_ignore_ascii_case(&owner) {
            anyhow::bail!("{r}: this firm works in {owner}'s repositories (the App's account)");
        }
        match reach.iter().find(|x| x.eq_ignore_ascii_case(r)) {
            Some(x) if !chosen.contains(x) => chosen.push(x.clone()),
            Some(_) => {}
            None => missing.push(r.as_str()),
        }
    }
    if !missing.is_empty() {
        anyhow::bail!(
            "the GitHub App is not installed on {}; install it there (`zapper github status --org {owner}`) or check the name. The App reaches: {}",
            missing.join(", "),
            reach.join(", ")
        );
    }
    Ok((owner, chosen))
}

/// What is in place for a worker's session.
#[derive(Clone)]
pub struct Applied {
    session: String,
    spec: mint::Spec,
}

/// The session secret of a worker's token.
pub fn secret_name(worker: &str) -> String {
    format!("gh-{worker}")
}

const RULE_GIT: &str = "credentials-github";
const RULE_API: &str = "credentials-github-api";
const HOSTS: [&str; 2] = ["github.com", "api.github.com"];

/// The repositories and permissions of a worker's live GitHub grants: what
/// their one token is minted for. A worker holds one repository, so this is
/// that grant's level; the policy owners hold all of them at one level.
/// If grants ever differed, the permissions are the highest of each.
fn scope(run: &FirmRun, worker: &str) -> Option<(Vec<String>, BTreeMap<String, String>)> {
    let grants = super::access::github_grants(run, worker);
    if grants.is_empty() {
        return None;
    }
    let levels = &run.config.credentials.github.levels;
    let mut perms: BTreeMap<String, String> = BTreeMap::new();
    let mut repos = Vec::new();
    for (resource, level) in &grants {
        repos.push(resource.strip_prefix("git:")?.to_string());
        for (k, v) in &levels.get(level)?.permissions {
            let rank = |l: &str| ["read", "write", "admin"].iter().position(|x| *x == l);
            match perms.get(k) {
                Some(have) if rank(have) >= rank(v) => {}
                _ => {
                    perms.insert(k.clone(), v.clone());
                }
            }
        }
    }
    repos.sort_by_key(|r| r.to_ascii_lowercase());
    Some((repos, perms))
}

/// The command that mints the worker's token. Repositories and permissions
/// are sorted, so an unchanged grant is the same command and the same cached
/// token. `--app` is the credentials file the firm's config names.
fn spec(run: &FirmRun, repos: &[String], perms: &BTreeMap<String, String>) -> mint::Spec {
    let mut argv: Vec<String> = match &run.config.credentials.github.mint {
        m if m.is_empty() => ["zapper", "mint", "github-app"].map(String::from).to_vec(),
        m => m.clone(),
    };
    let file = run
        .config
        .git
        .as_ref()
        .and_then(|g| g.github.as_ref())
        .map(|g| g.config.clone())
        .unwrap_or_default();
    argv.extend(["--app".into(), file]);
    for r in repos {
        argv.extend(["--repo".into(), r.clone()]);
    }
    for (k, v) in perms {
        argv.extend(["--perm".into(), format!("{k}={v}")]);
    }
    mint::Spec::new(&argv, None, None)
}

fn rules(worker: &str, comment: &str) -> Vec<AnyRule> {
    let header = [(
        "Authorization".to_string(),
        format!("Bearer {{{{secret:{}}}}}", secret_name(worker)),
    )];
    [(RULE_GIT, HOSTS[0]), (RULE_API, HOSTS[1])]
        .into_iter()
        .map(|(id, host)| {
            AnyRule::Net(NetRule {
                id: Some(id.into()),
                action: Action::Allow,
                host: host.into(),
                ports: vec![443],
                methods: vec![],
                path: None,
                secrets: vec![],
                set_headers: header.clone().into(),
                final_: false,
                comment: Some(comment.into()),
                protocol: Default::default(),
            })
        })
        .collect()
}

/// Makes the worker's session match their GitHub grants: the token's secret
/// and rules in place and the token minted, or all of it gone and the token
/// revoked at GitHub. Called whenever a grant is made, changed or taken, and
/// when the worker's VM comes up.
pub fn sync(d: &Core, run: &Arc<FirmRun>, worker: &str) {
    let _ = d;
    let session = run.rt(worker).and_then(|w| w.session());
    let wanted = scope(run, worker).map(|(repos, perms)| {
        let s = spec(run, &repos, &perms);
        (s, repos, perms)
    });
    let applied = run.facility.github.lock().unwrap().get(worker).cloned();
    match (wanted, applied) {
        (None, None) => {}
        (None, Some(old)) => {
            // Their last grant is gone: revoke the token at GitHub, and
            // take the secret and rules out of the session if it is up.
            revoke_at_github(run, worker, mint::peek(&old.spec));
            if let Some(s) = &session {
                s.remove_rule(RULE_GIT);
                s.remove_rule(RULE_API);
                s.remove_secret(&secret_name(worker));
            } else {
                mint::forget(&old.spec);
            }
            run.facility.github.lock().unwrap().remove(worker);
            run.journal.append(
                "credentials",
                json!({ "event": "github", "action": "removed", "worker": worker }),
            );
        }
        (Some((spec, repos, perms)), applied) => {
            let Some(session) = session else {
                return; // applied again when their VM comes up
            };
            if applied
                .as_ref()
                .is_some_and(|a| a.session == session.id && a.spec == spec)
            {
                return;
            }
            let old_token = applied
                .as_ref()
                .filter(|a| a.spec != spec)
                .and_then(|a| mint::peek(&a.spec));
            let secret = Secret {
                source: SecretSource::Command {
                    command: spec.argv.clone(),
                    timeout_secs: None,
                    ttl_secs: None,
                },
                mode: SecretMode::Proxy,
                hosts: HOSTS.map(String::from).to_vec(),
                plaintext: false,
                user: None,
                database: None,
                mechanism: None,
            };
            if let Err(e) = session.put_secret(&secret_name(worker), secret) {
                failed(run, worker, &format!("{e:#}"));
                return;
            }
            let comment = format!("credentials: GitHub {}", repos.join(", "));
            for r in rules(worker, &comment) {
                session.put_rule(r);
            }
            run.facility.github.lock().unwrap().insert(
                worker.to_string(),
                Applied {
                    session: session.id.clone(),
                    spec: spec.clone(),
                },
            );
            run.journal.append(
                "credentials",
                json!({ "event": "github", "action": if applied.is_some() { "updated" } else { "set" }, "worker": worker, "repos": repos, "permissions": perms }),
            );
            // Mint now, off the request path, so a refusal (a permission the
            // App lacks, a repository it isn't installed on) is reported to
            // the worker instead of failing their first push.
            let (run, worker) = (run.clone(), worker.to_string());
            let prewarm = move || {
                match mint::resolve(&spec) {
                    Ok(_) => revoke_at_github(&run, &worker, old_token),
                    Err(e) => failed(&run, &worker, &format!("{e:#}")),
                };
            };
            match tokio::runtime::Handle::try_current() {
                Ok(h) => {
                    h.spawn_blocking(prewarm);
                }
                Err(_) => {
                    std::thread::spawn(prewarm);
                }
            }
        }
    }
}

/// The run is stopping: every token it minted is revoked at GitHub and
/// forgotten, rather than left to lapse within the hour. Called once the
/// workers' sessions are stopped, so nothing mints another; each revocation
/// gets ten seconds.
pub async fn revoke_all(run: &Arc<FirmRun>) {
    let applied: Vec<(String, Applied)> = run.facility.github.lock().unwrap().drain().collect();
    let revokes = applied.into_iter().filter_map(|(worker, a)| {
        let token = mint::peek(&a.spec);
        mint::forget(&a.spec);
        token.map(|value| async move {
            let token = zapper_github::Token {
                value,
                expires_at: String::new(),
            };
            let gh = zapper_github::GitHub::default();
            let result =
                tokio::time::timeout(std::time::Duration::from_secs(10), gh.revoke(&token))
                    .await
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("timed out")));
            (worker, result)
        })
    });
    for (worker, result) in futures_util::future::join_all(revokes).await {
        run.journal.append(
            "credentials",
            json!({ "event": "github", "action": if result.is_ok() { "token revoked" } else { "token revoke failed" }, "worker": worker, "reason": "the run stopped", "error": result.err().map(|e| format!("{e:#}")) }),
        );
    }
}

/// The token couldn't be made: the grant stands, but nothing reaches GitHub.
fn failed(run: &FirmRun, worker: &str, why: &str) {
    run.journal.append(
        "credentials",
        json!({ "event": "enforce_failed", "worker": worker, "resource": "github", "error": why }),
    );
    run.notice(
        worker,
        "GitHub access could not be set up",
        &format!("[Automatic notice] Your GitHub access was granted, but a token could not be made for it: {why}\nTell the CEO; until it is fixed your requests to GitHub will fail.\n"),
    );
}

/// Invalidates a token at GitHub, in the background; it lapses within the
/// hour anyway, so a failure is only recorded.
fn revoke_at_github(run: &Arc<FirmRun>, worker: &str, token: Option<String>) {
    let Some(value) = token else {
        return;
    };
    let (run, worker) = (run.clone(), worker.to_string());
    let revoke = async move {
        let token = zapper_github::Token {
            value,
            expires_at: String::new(),
        };
        let result = zapper_github::GitHub::default().revoke(&token).await;
        run.journal.append(
            "credentials",
            json!({ "event": "github", "action": if result.is_ok() { "token revoked" } else { "token revoke failed" }, "worker": worker, "error": result.err().map(|e| format!("{e:#}")) }),
        );
    };
    // Called from threads that aren't in the runtime (the mint's).
    match tokio::runtime::Handle::try_current() {
        Ok(h) => {
            h.spawn(revoke);
        }
        Err(_) => {
            if let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                rt.block_on(revoke);
            }
        }
    }
}
