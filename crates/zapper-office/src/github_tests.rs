//! A firm in GitHub: a grant is a level on one repository, decided like any
//! other; it mints the worker's token (a command, here a script that logs how
//! it was called), puts it in their session with the rules that use it, and
//! takes it all back, revoking the token at GitHub, when the grant goes.
//! GitHub itself is a local fake that records the revocations.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::Value;
use zapper_core::testing::TestCore;

use super::{access, test_run_with, FirmRun, StartOptions};

/// The fake GitHub: the bearer token of each `DELETE /installation/token`.
fn github() -> &'static Arc<Mutex<Vec<String>>> {
    static FAKE: OnceLock<Arc<Mutex<Vec<String>>>> = OnceLock::new();
    FAKE.get_or_init(|| {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        std::env::set_var(
            zapper_github::API_ENV,
            format!("http://{}", listener.local_addr().unwrap()),
        );
        let log = seen.clone();
        std::thread::spawn(move || {
            for sock in listener.incoming().flatten() {
                let mut sock = sock;
                let mut buf = vec![0u8; 4096];
                let n = sock.read(&mut buf).unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).into_owned();
                let line = head.lines().next().unwrap_or("").to_string();
                let json = |status: &str, body: &str| {
                    format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
                };
                let reply = match line.split(' ').take(2).collect::<Vec<_>>()[..] {
                    ["DELETE", "/installation/token"] => {
                        let bearer = head
                            .lines()
                            .find(|l| l.to_ascii_lowercase().starts_with("authorization:"))
                            .and_then(|l| l.rsplit(' ').next())
                            .unwrap_or_default()
                            .to_string();
                        log.lock().unwrap().push(bearer);
                        "HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n".to_string()
                    }
                    ["GET", p] if p == "/repos/acme/api/installation" || p == "/repos/acme/web/installation" => json("200 OK", r#"{"id":42}"#),
                    ["GET", p] if p.starts_with("/app/installations?") => json("200 OK", r#"[{"id":42,"account":{"login":"Acme"},"repository_selection":"selected"}]"#),
                    ["GET", "/orgs/acme/installation"] => json("200 OK", r#"{"id":42,"repository_selection":"selected"}"#),
                    ["POST", "/app/installations/42/access_tokens"] => json("201 Created", r#"{"token":"ghs_listing","expires_at":"2099-01-01T00:00:00Z"}"#),
                    ["GET", p] if p.starts_with("/installation/repositories") => json("200 OK", r#"{"repositories":[{"full_name":"Acme/web"},{"full_name":"Acme/api"}]}"#),
                    _ => json("404 Not Found", r#"{"message":"Not Found"}"#),
                };
                let _ = sock.write_all(reply.as_bytes());
            }
        });
        seen
    })
}

/// A firm in GitHub whose minter logs its arguments to a file and prints a
/// token numbered by how often it ran, or fails when the file `fail` exists.
fn firm(name: &str) -> (TestCore, Arc<FirmRun>, PathBuf) {
    github();
    let log = std::env::temp_dir().join(format!("zapper-gh-{name}-{}.log", std::process::id()));
    let _ = std::fs::remove_file(&log);
    let fail = log.with_extension("fail");
    let _ = std::fs::remove_file(&fail);
    let yaml = format!(
        "
profiles:
  w: {{ agent: {{ kind: claude-code }} }}
firms:
  acme:
    roles:
      ceo: {{ profile: w, agent: ceo }}
      eng: {{ profile: w, agent: software-engineer }}
    workers:
      - {{ name: ceo, role: ceo }}
      - {{ name: pat, role: eng, reports_to: ceo }}
      - {{ name: lee, role: eng, reports_to: ceo }}
    git: {{ github: {{ config: acme.yaml }} }}
    credentials:
      github:
        mint:
          - sh
          - -c
          - |
            [ -e {fail} ] && {{ echo 'HTTP 422: permission not granted to the App' >&2; exit 1; }}
            echo \"$@\" >> {log}
            printf '{{\"value\":\"ghs_%s\",\"expires_at\":\"2099-01-01T00:00:00Z\"}}' \"$(wc -l < {log} | tr -d ' ')\"
          - mint
        levels:
          read: {{ actions: read, artifact_metadata: read, checks: read, contents: read, issues: read, pull_requests: read, statuses: read }}
          write: {{ contents: write, pull_requests: write, issues: write, actions: read, checks: read, statuses: read, artifact_metadata: read }}
",
        log = log.display(),
        fail = fail.display(),
    );
    let t = crate::test_core(&yaml);
    // The repositories a run lists from GitHub, here given outright.
    let opts = StartOptions {
        repos: Some(vec!["Acme/api".into(), "acme/web".into()]),
        ..Default::default()
    };
    let run = test_run_with(&t.core, "acme", &opts);
    (t, run, log)
}

/// The arguments of each time the minter ran for the CEO (who holds the
/// level `read` on every repository from the start), or for anyone else,
/// once there are `n` of them.
fn mints(log: &PathBuf, ceo: bool, n: usize) -> Vec<String> {
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        let lines: Vec<String> = std::fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .filter(|l| l.contains("--repo acme/web") == ceo)
            .map(str::to_string)
            .collect();
        if lines.len() >= n || Instant::now() > until {
            return lines;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn journal(run: &FirmRun) -> Vec<Value> {
    std::fs::read_to_string(run.dir.join("journal.jsonl"))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn wait_for(what: &str, mut ok: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(10);
    while !ok() {
        assert!(Instant::now() < until, "never: {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn session(run: &FirmRun, worker: &str) -> Arc<zapper_core::Session> {
    run.rt(worker).unwrap().session().unwrap()
}

fn rule_ids(run: &FirmRun, worker: &str) -> Vec<String> {
    session(run, worker)
        .rules()
        .iter()
        .filter_map(|r| r.id().map(str::to_string))
        .filter(|i| i.starts_with("credentials-github"))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_grant_mints_the_workers_token_for_their_repository_and_level() {
    let (t, run, log) = firm("grant");
    let id = access::request(&run, "pat", "git:ACME/API", "write", "building the api").unwrap();
    // A GitHub level is for an officer (or the CEO), who decides it as asked.
    let q = run.access.lock().unwrap().requests[&id].clone();
    assert_eq!(
        (q.resource.as_str(), q.level.as_str()),
        ("git:Acme/api", "write")
    );
    assert_eq!(
        q.detail,
        "actions: read, artifact_metadata: read, checks: read, contents: write, issues: write, pull_requests: write, statuses: read"
    );
    assert_eq!(q.decider.as_deref(), Some("ceo"));
    assert!(
        access::decide(&t.core, &run, "ceo", &id, true, Some("read"), None, "ok")
            .unwrap_err()
            .to_string()
            .contains("granted as asked or denied")
    );
    access::decide(&t.core, &run, "ceo", &id, true, None, None, "ok").unwrap();

    assert_eq!(
        access::level_name(&run, "pat", "git:Acme/api").as_deref(),
        Some("write")
    );
    // Minted for that repository and exactly that level (sorted).
    assert_eq!(
        mints(&log, false, 1),
        ["--app acme.yaml --repo Acme/api --perm actions=read --perm artifact_metadata=read --perm checks=read --perm contents=write --perm issues=write --perm pull_requests=write --perm statuses=read"]
    );
    // In the worker's session: the secret, and the two rules that use it.
    assert_eq!(session(&run, "pat").secret_names(), ["gh-pat"]);
    let mut rules = rule_ids(&run, "pat");
    rules.sort();
    assert_eq!(rules, ["credentials-github", "credentials-github-api"]);
    assert!(
        session(&run, "lee").secret_names().is_empty(),
        "no one else's"
    );
    // The token is never in the journal.
    let j = std::fs::read_to_string(run.dir.join("journal.jsonl")).unwrap();
    assert!(!j.contains("ghs_"), "{j}");
    assert!(j.contains("\"action\":\"set\"") && j.contains("\"repos\":[\"Acme/api\"]"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_worker_works_in_one_repository_and_the_owners_hold_them_all() {
    let (t, run, log) = firm("one");
    access::board_grant(&t.core, &run, "pat", "git:acme/api", "write", None, "x").unwrap();
    let e = access::board_grant(&t.core, &run, "pat", "git:acme/web", "read", None, "x")
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("one GitHub repository") && e.contains("Acme/api"),
        "{e}"
    );
    let e = access::request(&run, "pat", "git:acme/web", "read", "also web")
        .unwrap_err()
        .to_string();
    assert!(e.contains("one GitHub repository"), "{e}");
    let e = access::request(&run, "pat", "git", "read", "x")
        .unwrap_err()
        .to_string();
    assert!(e.contains("name the repository"), "{e}");

    // The CEO holds both, at the level `read`, with one token.
    access::owner_access(&t.core, &run, "ceo", "board");
    assert_eq!(
        mints(&log, true, 1),
        ["--app acme.yaml --repo Acme/api --repo acme/web --perm actions=read --perm artifact_metadata=read --perm checks=read --perm contents=read --perm issues=read --perm pull_requests=read --perm statuses=read"],
        "one token for both repositories"
    );
    assert_eq!(session(&run, "ceo").secret_names(), ["gh-ceo"]);

    // And no more: a higher level would be on that token for every repository.
    let e = access::request(&run, "ceo", "git:acme/api", "write", "x")
        .unwrap_err()
        .to_string();
    assert!(e.contains("policy owner"), "{e}");
    let e = access::board_grant(&t.core, &run, "ceo", "git:acme/api", "write", None, "x")
        .unwrap_err()
        .to_string();
    assert!(e.contains("policy owner"), "{e}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_changed_level_is_a_new_token_and_the_old_one_is_revoked_at_github() {
    let (t, run, log) = firm("extend");
    access::board_grant(&t.core, &run, "pat", "git:acme/api", "read", None, "x").unwrap();
    assert_eq!(mints(&log, false, 1).len(), 1);
    let first = mint_spec(&run, "pat");
    wait_for("the first token is minted", || {
        zapper_policy::mint::peek(&first).is_some()
    });
    let old_token = zapper_policy::mint::peek(&first).unwrap();
    let renewing = access::request(&run, "pat", "git:acme/api", "read", "again")
        .unwrap_err()
        .to_string();
    assert!(renewing.contains("you have read"), "{renewing}");

    // Another level on the same repository: an extension, decided like a grant.
    let id = access::request(&run, "pat", "git:acme/api", "write", "needs to push").unwrap();
    assert_eq!(
        access::request(&run, "pat", "git:acme/api", "write", "again").unwrap(),
        id,
        "asked again while pending: the same request"
    );
    access::decide(&t.core, &run, "ceo", &id, true, None, None, "ok").unwrap();

    assert!(mints(&log, false, 2)[1]
        .contains("--perm actions=read --perm artifact_metadata=read --perm checks=read --perm contents=write --perm issues=write --perm pull_requests=write --perm statuses=read"));
    assert_eq!(
        access::level_name(&run, "pat", "git:Acme/api").as_deref(),
        Some("write")
    );
    assert_eq!(
        session(&run, "pat").secret_names(),
        ["gh-pat"],
        "the same secret, new command"
    );
    wait_for("the first token is revoked", || {
        github().lock().unwrap().contains(&old_token)
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_revoked_grant_takes_everything_back() {
    let (t, run, log) = firm("revoke");
    let id = access::request(&run, "pat", "git:acme/api", "write", "needs to push").unwrap();
    assert_eq!(
        run.access.lock().unwrap().requests[&id].decider.as_deref(),
        Some("ceo"),
        "no officers: the CEO decides"
    );
    access::decide(&t.core, &run, "ceo", &id, true, None, None, "ok").unwrap();
    assert_eq!(mints(&log, false, 1).len(), 1);
    let spec = mint_spec(&run, "pat");
    wait_for("minted", || zapper_policy::mint::peek(&spec).is_some());
    let token = zapper_policy::mint::peek(&spec).unwrap();

    access::revoke(&t.core, &run, "ceo", "pat", "git:acme/api", "done").unwrap();
    assert!(session(&run, "pat").secret_names().is_empty());
    assert!(rule_ids(&run, "pat").is_empty());
    assert_eq!(zapper_policy::mint::peek(&spec), None, "forgotten");
    wait_for("revoked at GitHub", || {
        github().lock().unwrap().contains(&token)
    });
    assert!(journal(&run)
        .iter()
        .any(|e| e["action"] == "removed" && e["worker"] == "pat"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stopping_the_run_revokes_every_token_at_github() {
    let (t, run, _log) = firm("down");
    access::board_grant(&t.core, &run, "pat", "git:acme/api", "write", None, "x").unwrap();
    access::owner_access(&t.core, &run, "ceo", "board");
    let specs = [mint_spec(&run, "pat"), mint_spec(&run, "ceo")];
    wait_for("both minted", || {
        specs.iter().all(|s| zapper_policy::mint::peek(s).is_some())
    });
    let tokens: Vec<String> = specs
        .iter()
        .map(|s| zapper_policy::mint::peek(s).unwrap())
        .collect();

    crate::run::down(&t.core, &run, "test").await;
    for s in &specs {
        assert_eq!(zapper_policy::mint::peek(s), None, "forgotten");
    }
    let seen = github().lock().unwrap().clone();
    assert!(tokens.iter().all(|x| seen.contains(x)), "{seen:?}");
    let revoked: Vec<String> = journal(&run)
        .iter()
        .filter(|e| e["action"] == "token revoked" && e["reason"] == "the run stopped")
        .map(|e| e["worker"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(revoked.len(), 2, "{revoked:?}");
    assert!(revoked.contains(&"pat".into()) && revoked.contains(&"ceo".into()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_token_that_cannot_be_made_is_told_to_the_worker() {
    let (t, run, log) = firm("fail");
    std::fs::write(log.with_extension("fail"), "").unwrap();
    access::board_grant(&t.core, &run, "pat", "git:acme/api", "write", None, "x").unwrap();
    wait_for("the failure is recorded", || {
        journal(&run)
            .iter()
            .any(|e| e["event"] == "enforce_failed" && e["worker"] == "pat")
    });
    let j = journal(&run);
    let e = j.iter().find(|e| e["event"] == "enforce_failed").unwrap();
    assert!(
        e["error"]
            .as_str()
            .unwrap()
            .contains("permission not granted to the App"),
        "{e}"
    );
    wait_for("the worker is told", || {
        run.post
            .with(|po| po.inbox("pat", false, 50))
            .iter()
            .any(|m| {
                m["subject"]
                    .as_str()
                    .is_some_and(|s| s.contains("could not be set up"))
            })
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gh_and_git_are_set_up_without_a_credential_and_the_levels_are_listed() {
    let (_t, run, _log) = firm("files");
    let files = crate::facility::credentials_for(&run, "pat", "eng", "/home/pat").unwrap();
    let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(
        paths,
        ["/home/pat/.gitconfig", "/home/pat/.config/gh/hosts.yml"]
    );
    let hosts = files
        .iter()
        .find(|f| f.path.ends_with("hosts.yml"))
        .unwrap();
    assert!(hosts.content.contains("replaced-by-the-proxy") && hosts.mode == 0o600);
    assert!(!files
        .iter()
        .any(|f| f.content.contains("ghs_") || f.path.ends_with(".git-credentials")));

    let services = crate::facility::services_for(&run, "eng");
    assert!(services.git.is_empty(), "no repositories of its own");
    assert_eq!(
        services.github,
        [
            "https://github.com/Acme/api.git",
            "https://github.com/acme/web.git"
        ]
    );
    // The preamble names them and the firm's levels, and how work reaches main.
    let p = crate::prompts::preamble(
        "acme",
        &run.config,
        &run.config,
        "pat",
        &services,
        "",
        None,
        "",
    );
    assert!(
        p.contains("GitHub repositories: https://github.com/Acme/api.git"),
        "{p}"
    );
    assert!(
        p.contains("write (actions: read, artifact_metadata: read, checks: read, contents: write"),
        "{p}"
    );
    assert!(p.contains("which the board merges"), "{p}");

    let mine = access::mine(&run, "pat");
    let p = &mine["github_levels"]["levels"];
    assert_eq!(
        p["write"]["allows"],
        "actions: read, artifact_metadata: read, checks: read, contents: write, issues: write, pull_requests: write, statuses: read"
    );
    assert!(mine["resources"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r == "git:Acme/api"));

    // A background check grants a level, on one repository.
    let ok = access::check_clearance(
        &run,
        "ceo",
        false,
        &serde_json::json!([{ "resource": "git:acme/api", "level": "write" }]),
    )
    .unwrap();
    assert_eq!(ok[0].0, "git:Acme/api");
    assert_eq!(ok[0].1.level, "write");
    let e = access::check_clearance(
        &run,
        "ceo",
        false,
        &serde_json::json!([{ "resource": "git:acme/api", "level": "admin" }]),
    )
    .unwrap_err()
    .to_string();
    assert!(e.contains("levels: "), "{e}");
    let e = access::check_clearance(
        &run,
        "ceo",
        false,
        &serde_json::json!([
            { "resource": "git:acme/api", "level": "write" },
            { "resource": "git:acme/web", "level": "write" },
        ]),
    )
    .unwrap_err()
    .to_string();
    assert!(e.contains("one GitHub repository"), "{e}");
}

fn mint_spec(run: &FirmRun, worker: &str) -> zapper_policy::mint::Spec {
    let s = session(run, worker);
    let secrets = s.secret_names();
    assert_eq!(secrets, [format!("gh-{worker}")]);
    s.secret_source(&secrets[0]).unwrap().mint_spec().unwrap()
}

/// An App's credentials file, made from the test key.
fn app_file(name: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let pem = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../zapper-github/tests/fixtures/test-key-pkcs1.pem"),
    )
    .unwrap();
    let body: String = pem.lines().map(|l| format!("    {l}\n")).collect();
    let f = std::env::temp_dir().join(format!("zapper-gh-app-{name}-{}.yaml", std::process::id()));
    std::fs::write(
        &f,
        format!("github:\n  clientId: Iv23test\n  appSlug: acme-agents\n  privateKey: |\n{body}"),
    )
    .unwrap();
    std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o600)).unwrap();
    f
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_finds_the_account_and_its_repositories_from_the_app() {
    use super::github::resolve_repositories;
    github();
    let gh = zapper_github::GitHub::default();
    let file = app_file("resolve");
    let names = |r: &[&str]| r.iter().map(|x| x.to_string()).collect::<Vec<_>>();

    // Nothing given: the account the App is installed on, and every repository
    // it reaches, as GitHub spells them.
    let (owner, repos) = resolve_repositories(&gh, &file, &[]).await.unwrap();
    assert_eq!(
        (owner.as_str(), repos),
        ("Acme", names(&["Acme/api", "Acme/web"]))
    );

    // Given: checked against what it reaches, and spelled as GitHub does.
    let (_, repos) = resolve_repositories(&gh, &file, &names(&["acme/WEB", "ACME/web"]))
        .await
        .unwrap();
    assert_eq!(repos, ["Acme/web"]);
    let e = format!(
        "{:#}",
        resolve_repositories(&gh, &file, &names(&["acme/api", "Acme/apii"]))
            .await
            .unwrap_err()
    );
    assert!(
        e.contains("not installed on Acme/apii")
            && e.contains("The App reaches: Acme/api, Acme/web"),
        "{e}"
    );
    let e = format!(
        "{:#}",
        resolve_repositories(&gh, &file, &names(&["other/api"]))
            .await
            .unwrap_err()
    );
    assert!(e.contains("works in Acme's repositories"), "{e}");

    let none = std::env::temp_dir().join("zapper-gh-no-such-app.yaml");
    let e = format!(
        "{:#}",
        resolve_repositories(&gh, &none, &[]).await.unwrap_err()
    );
    assert!(
        e.contains("no GitHub App at") && e.contains("zapper github setup"),
        "{e}"
    );
    let _ = std::fs::remove_file(file);
}
