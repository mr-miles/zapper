//! A firm whose repositories have a remote (proposal 09). The remote's git
//! is bare repositories on disk (`file://`), its API a local fake that
//! keeps pull requests, checks and reviews, and the daemon's tokens come
//! from a script. Agents' pushes go through the firm's server's hooks as
//! git runs them.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::{json, Value};
use zapper_core::testing::TestCore;

use super::{access, remote, test_run_remote, FirmRun};

/// What the fake GitHub holds.
#[derive(Default)]
struct Fake {
    /// Pull requests, by repository.
    pulls: HashMap<String, Vec<Value>>,
    /// Check runs, by commit.
    checks: HashMap<String, Value>,
    /// Reviews, by repository and pull request.
    reviews: HashMap<(String, u64), Value>,
    /// The tokens revoked.
    revoked: Vec<String>,
}

struct World {
    base: PathBuf,
    fake: Arc<Mutex<Fake>>,
}

/// The remote's bare repositories are under `base/remote/Acme`.
fn remote_dir(base: &Path, repo: &str) -> PathBuf {
    base.join("remote").join("Acme").join(format!("{repo}.git"))
}

fn sh(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// One fake GitHub and one config home for the whole process; every test
/// has repositories of its own on it.
fn world() -> &'static World {
    static W: OnceLock<World> = OnceLock::new();
    W.get_or_init(|| {
        let base = std::env::temp_dir().join(format!("zapper-remote-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("remote/Acme")).unwrap();
        // The App's credentials, where `git.remote.github.config` finds them.
        let apps = base.join("config/zapper/github-apps");
        std::fs::create_dir_all(&apps).unwrap();
        let key = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../zapper-github/tests/fixtures/test-key-pkcs1.pem");
        let f = apps.join("acme.yaml");
        std::fs::write(
            &f,
            format!(
                "github:\n  clientId: Iv23test\n  appSlug: acme-agents\n  privateKeyPath: {}\n",
                key.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&f, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
        std::env::set_var("XDG_CONFIG_HOME", base.join("config"));
        std::env::set_var(
            remote::GIT_URL_ENV,
            format!("file://{}", base.join("remote").display()),
        );
        let fake = Arc::new(Mutex::new(Fake::default()));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::env::set_var(zapper_github::API_ENV, format!("http://{addr}"));
        let (f, b) = (fake.clone(), base.clone());
        std::thread::spawn(move || {
            for sock in listener.incoming().flatten() {
                serve(sock, &f, &b, &addr.to_string());
            }
        });
        World { base, fake }
    })
}

/// One request to the fake GitHub.
fn serve(mut sock: std::net::TcpStream, fake: &Mutex<Fake>, base: &Path, addr: &str) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let (head, body) = loop {
        let n = sock.read(&mut chunk).unwrap_or(0);
        if n == 0 {
            return;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..i]).into_owned();
            let len: usize = head
                .lines()
                .find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|v| v.trim().parse().unwrap_or(0))
                })
                .unwrap_or(0);
            while buf.len() < i + 4 + len {
                let n = sock.read(&mut chunk).unwrap_or(0);
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            break (head, buf[i + 4..].to_vec());
        }
    };
    let line = head.lines().next().unwrap_or("").to_string();
    let mut parts = line.split(' ');
    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let path = target.split('?').next().unwrap_or("");
    let query = target.split_once('?').map(|(_, q)| q).unwrap_or("");
    let json = |status: &str, v: Value| {
        let body = v.to_string();
        format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
    };
    let seg: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    let mut f = fake.lock().unwrap();
    let reply = match (method, seg.as_slice()) {
        ("GET", ["app", "installations"]) => json("200 OK", json!([{ "id": 42, "account": { "login": "Acme" } }])),
        ("POST", ["app", "installations", "42", "access_tokens"]) => json("201 Created", json!({ "token": "ghs_listing", "expires_at": "2099-01-01T00:00:00Z" })),
        ("GET", ["installation", "repositories"]) => {
            let mut repos: Vec<Value> = std::fs::read_dir(base.join("remote/Acme"))
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|e| e.file_name().to_str()?.strip_suffix(".git").map(|n| json!({ "full_name": format!("Acme/{n}") })))
                .collect();
            repos.push(json!({ "full_name": "Other/elsewhere" }));
            json("200 OK", json!({ "repositories": repos }))
        }
        ("DELETE", ["installation", "token"]) => {
            let bearer = head.lines().find(|l| l.to_ascii_lowercase().starts_with("authorization:")).and_then(|l| l.rsplit(' ').next()).unwrap_or_default();
            f.revoked.push(bearer.to_string());
            "HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n".to_string()
        }
        ("GET", ["repos", "Acme", r, "pulls"]) => {
            let head_ref = query.split('&').find_map(|p| p.strip_prefix("head=Acme:")).unwrap_or("");
            let open: Vec<Value> = f.pulls.get(*r).into_iter().flatten().filter(|p| p["state"] == "open" && p["head"]["ref"] == head_ref).cloned().collect();
            json("200 OK", json!(open))
        }
        ("POST", ["repos", "Acme", r, "pulls"]) => {
            let req: Value = serde_json::from_slice(&body).unwrap_or_default();
            let head_ref = req["head"].as_str().unwrap_or("").to_string();
            let sha = Command::new("git").arg("--git-dir").arg(remote_dir(base, r)).args(["rev-parse", &format!("refs/heads/{head_ref}")]).output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
            let list = f.pulls.entry(r.to_string()).or_default();
            let number = list.len() as u64 + 1;
            let pull = json!({ "number": number, "html_url": format!("https://github.test/Acme/{r}/pull/{number}"), "state": "open", "merged": false, "title": req["title"], "body": req["body"], "base": req["base"], "head": { "ref": head_ref, "sha": sha } });
            list.push(pull.clone());
            json("201 Created", pull)
        }
        ("GET", ["repos", "Acme", r, "pulls", n]) => match f.pulls.get(*r).and_then(|l| l.iter().find(|p| p["number"].as_u64().map(|x| x.to_string()).as_deref() == Some(*n))) {
            Some(p) => json("200 OK", p.clone()),
            None => json("404 Not Found", json!({ "message": "Not Found" })),
        },
        ("GET", ["repos", "Acme", _, "commits", sha, "check-runs"]) => json("200 OK", f.checks.get(*sha).cloned().unwrap_or(json!({ "check_runs": [] }))),
        ("GET", ["repos", "Acme", r, "pulls", n, "reviews"]) => json("200 OK", f.reviews.get(&(r.to_string(), n.parse().unwrap_or(0))).cloned().unwrap_or(json!([]))),
        ("GET", ["repos", "Acme", _, "issues", _, "comments"]) | ("GET", ["repos", "Acme", _, "pulls", _, "comments"]) => json("200 OK", json!([])),
        ("GET", ["repos", "Acme", _, "actions", "jobs", id, "logs"]) => format!("HTTP/1.1 302 Found\r\nLocation: http://{addr}/logs/{id}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"),
        ("GET", ["logs", id]) => {
            let text = format!("##[group]Run npm test\nok 1\nnot ok 2 - the list keeps its order (job {id})\n##[error]Process completed with exit code 1.\n");
            format!("HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}", text.len())
        }
        _ => json("404 Not Found", json!({ "message": "Not Found" })),
    };
    drop(f);
    let _ = sock.write_all(reply.as_bytes());
}

/// A repository on the remote with one commit on `main`, `README.md`.
fn remote_repo(base: &Path, name: &str) {
    let dir = remote_dir(base, name);
    std::fs::create_dir_all(&dir).unwrap();
    sh(&dir, &["init", "-q", "--bare", "-b", "main"]);
    let work = base.join("seed").join(name);
    std::fs::create_dir_all(&work).unwrap();
    sh(&work, &["init", "-q", "-b", "main"]);
    std::fs::write(work.join("README.md"), format!("# {name}\n")).unwrap();
    sh(&work, &["add", "."]);
    sh(&work, &["commit", "-q", "-m", "start"]);
    sh(&work, &["push", "-q", dir.to_str().unwrap(), "main"]);
}

/// A commit on the remote's `main`, as the board's merge would make one.
fn commit_on_remote(base: &Path, name: &str, file: &str) -> String {
    let work = base.join("seed").join(name);
    // From the remote's main as it is now, which the daemon may have moved.
    sh(
        &work,
        &[
            "fetch",
            "-q",
            remote_dir(base, name).to_str().unwrap(),
            "main",
        ],
    );
    sh(&work, &["reset", "-q", "--hard", "FETCH_HEAD"]);
    std::fs::write(work.join(file), "x\n").unwrap();
    sh(&work, &["add", "."]);
    sh(&work, &["commit", "-q", "-m", &format!("add {file}")]);
    sh(
        &work,
        &[
            "push",
            "-q",
            remote_dir(base, name).to_str().unwrap(),
            "main",
        ],
    );
    sh(&work, &["rev-parse", "HEAD"])
}

/// A firm `acme` whose remote has repositories `code-<t>` and `kb-<t>` (its
/// main the firm's), a CEO and an engineer `pat`.
async fn firm(t: &str) -> (TestCore, Arc<FirmRun>, String, String) {
    let w = world();
    let (code, kb) = (format!("code-{t}"), format!("kb-{t}"));
    remote_repo(&w.base, &code);
    remote_repo(&w.base, &kb);
    let dir = w.base.join("repos").join(t);
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
    git:
      remote:
        github:
          config: acme.yaml
          mint: [sh, -c, \"printf '{{\\\"value\\\":\\\"ghs_daemon\\\",\\\"expires_at\\\":\\\"2099-01-01T00:00:00Z\\\"}}'\", mint]
      dir: {}
      local: [{kb}]
",
        dir.display()
    );
    let tc = crate::test_core(&yaml);
    let run = test_run_remote(&tc.core, "acme", &[&code, &kb]).await;
    (tc, run, code, kb)
}

/// What a worker's git does: clone from the firm's server (here, its
/// repository on disk, through the same hooks), commit and push a branch.
fn push_branch(
    run: &FirmRun,
    repo: &str,
    branch: &str,
    file: &str,
    level: &str,
) -> Result<String, String> {
    let work = run
        .dir
        .join("work")
        .join(format!("{repo}-{branch}-{file}").replace('/', "_"));
    let src = remote::root(&run.config, &run.dir).join(format!("{repo}.git"));
    let _ = std::fs::remove_dir_all(&work);
    let out = Command::new("git")
        .args(["clone", "-q"])
        .arg(&src)
        .arg(&work)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    if branch != "main" {
        sh(&work, &["checkout", "-q", "-b", branch]);
    }
    if let Some(dir) = Path::new(file)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
    {
        std::fs::create_dir_all(work.join(dir)).unwrap();
    }
    std::fs::write(work.join(file), "work\n").unwrap();
    sh(&work, &["add", "."]);
    sh(&work, &["commit", "-q", "-m", &format!("add {file}")]);
    let out = Command::new("git")
        .args(["push", "-q", "origin", branch])
        .current_dir(&work)
        .env("ZAPPER_GIT_LEVEL", level)
        .env("REMOTE_USER", "pat")
        .output()
        .unwrap();
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).into_owned());
    }
    Ok(sh(&work, &["rev-parse", "HEAD"]))
}

fn rev(dir: &Path, r: &str) -> String {
    let out = Command::new("git")
        .arg("--git-dir")
        .arg(dir)
        .args(["rev-parse", r])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn inbox(run: &FirmRun, who: &str) -> Vec<Value> {
    run.post.with(|po| po.inbox(who, false, 50))
}

fn subjects(run: &FirmRun, who: &str) -> Vec<String> {
    inbox(run, who)
        .iter()
        .map(|m| m["subject"].as_str().unwrap_or("").to_string())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_repositories_are_the_apps_kept_across_runs_and_unknown_ones_archived() {
    let w = world();
    let dir = w.base.join("repos").join("prep");
    std::fs::create_dir_all(dir.join("gone.git")).unwrap();
    let (_t, run, code, kb) = firm("prep").await;
    // The account and the repositories (only the run's, here) from the App.
    let g = run.config.git.as_ref().unwrap();
    assert_eq!(g.owner, "Acme");
    assert_eq!(run.config.git_repos(), [code.clone(), kb.clone()]);
    remote::prepare(&run).await.unwrap();
    for r in [&code, &kb] {
        let local = dir.join(format!("{r}.git"));
        assert_eq!(
            rev(&local, "main"),
            rev(&remote_dir(&w.base, r), "main"),
            "{r} cloned"
        );
    }
    // A repository the App doesn't reach is moved aside, not deleted.
    assert!(!dir.join("gone.git").exists());
    assert!(dir.join("archive/gone.git").exists());
    // The run's starting point is recorded, for replay.
    let rec: Value =
        serde_json::from_slice(&std::fs::read(run.dir.join("git.json")).unwrap()).unwrap();
    assert_eq!(
        rec["repos"][&code]["start"]["main"],
        rev(&dir.join(format!("{code}.git")), "main")
    );
    // The next run fetches what is there instead of cloning again.
    let moved = commit_on_remote(&w.base, &code, "second.txt");
    remote::prepare(&run).await.unwrap();
    assert_eq!(rev(&dir.join(format!("{code}.git")), "main"), moved);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn main_comes_from_the_remote_and_nobody_pushes_it() {
    let w = world();
    let (_t, run, code, _kb) = firm("main").await;
    remote::prepare(&run).await.unwrap();
    // Not even admin pushes main of a repository whose main is the remote's.
    let e = push_branch(&run, &code, "main", "x.txt", "admin").unwrap_err();
    assert!(
        e.contains("comes from the remote") && e.contains("publish_branch"),
        "{e}"
    );
    assert!(push_branch(&run, &code, "pat-work", "x.txt", "write").is_ok());
    // The board merges on the remote; the daemon fetches main back.
    let merged = commit_on_remote(&w.base, &code, "merged.txt");
    remote::poll(&run).await;
    assert_eq!(
        rev(
            &remote::root(&run.config, &run.dir).join(format!("{code}.git")),
            "main"
        ),
        merged
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_published_branch_is_checked_pushed_and_opened_as_a_pull_request() {
    let w = world();
    let (t, run, code, kb) = firm("pub").await;
    remote::prepare(&run).await.unwrap();
    let head = push_branch(&run, &code, "feature", "src.txt", "write").unwrap();
    // Publishing needs write.
    let e = remote::publish_branch(&run, "pat", &code, "feature", "Add src", "")
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("needs write"), "{e}");
    access::board_grant(
        &t.core,
        &run,
        "pat",
        &format!("git:{code}"),
        "write",
        None,
        "x",
    )
    .unwrap();
    let v = remote::publish_branch(&run, "pat", &code, "feature", "Add src", "What it does.")
        .await
        .unwrap();
    assert_eq!(v["branch"], "pat/feature");
    assert_eq!(v["pull"], 1);
    // On the remote: the branch as `<worker>/<branch>`, and its pull request.
    assert_eq!(
        rev(&remote_dir(&w.base, &code), "refs/heads/pat/feature"),
        head
    );
    let pull = w.fake.lock().unwrap().pulls[&code][0].clone();
    assert_eq!(
        (pull["base"].as_str(), pull["head"]["ref"].as_str()),
        (Some("main"), Some("pat/feature"))
    );
    assert!(
        pull["body"].as_str().unwrap().contains("Published by pat"),
        "{pull}"
    );
    // Publishing again updates the same pull request.
    let v = remote::publish_branch(&run, "pat", &code, "feature", "Add src", "")
        .await
        .unwrap();
    assert_eq!(v["pull"], 1);
    assert_eq!(w.fake.lock().unwrap().pulls[&code].len(), 1);

    // What is refused, before anything leaves.
    let refused = |e: anyhow::Error| e.to_string();
    let e = refused(
        remote::publish_branch(&run, "pat", &code, "main", "x", "")
            .await
            .unwrap_err(),
    );
    assert!(e.contains("not main"), "{e}");
    let e = refused(
        remote::publish_branch(&run, "pat", &code, "nothing", "x", "")
            .await
            .unwrap_err(),
    );
    assert!(e.contains("push it first"), "{e}");
    push_branch(&run, &code, "ci", ".github/workflows/test.yml", "write").unwrap();
    let e = refused(
        remote::publish_branch(&run, "pat", &code, "ci", "x", "")
            .await
            .unwrap_err(),
    );
    assert!(
        e.contains("the board's") && e.contains(".github/workflows/test.yml"),
        "{e}"
    );
    commit_on_remote(&w.base, &code, "later.txt");
    remote::poll(&run).await;
    let e = refused(
        remote::publish_branch(&run, "pat", &code, "feature", "x", "")
            .await
            .unwrap_err(),
    );
    assert!(e.contains("tip of main"), "{e}");
    access::board_grant(
        &t.core,
        &run,
        "pat",
        &format!("git:{kb}"),
        "write",
        None,
        "x",
    )
    .unwrap();
    let e = refused(
        remote::publish_branch(&run, "pat", &kb, "feature", "x", "")
            .await
            .unwrap_err(),
    );
    assert!(e.contains("main is the firm's"), "{e}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_author_hears_of_checks_reviews_and_the_merge() {
    let w = world();
    let (t, run, code, _kb) = firm("tell").await;
    remote::prepare(&run).await.unwrap();
    access::board_grant(
        &t.core,
        &run,
        "pat",
        &format!("git:{code}"),
        "write",
        None,
        "x",
    )
    .unwrap();
    let head = push_branch(&run, &code, "fix", "fix.txt", "write").unwrap();
    remote::publish_branch(&run, "pat", &code, "fix", "Fix it", "")
        .await
        .unwrap();
    // Checks still running: nothing to tell.
    w.fake.lock().unwrap().checks.insert(head.clone(), json!({ "check_runs": [{ "id": 7, "name": "test", "status": "in_progress", "conclusion": null, "html_url": "" }] }));
    remote::poll(&run).await;
    assert!(!subjects(&run, "pat")
        .iter()
        .any(|s| s.contains("Pull request")));
    // They finish, one failing, and the board reviews.
    w.fake.lock().unwrap().checks.insert(
        head.clone(),
        json!({ "check_runs": [
        { "id": 7, "name": "test", "status": "completed", "conclusion": "failure", "html_url": "" },
        { "id": 8, "name": "lint", "status": "completed", "conclusion": "success", "html_url": "" },
    ] }),
    );
    w.fake.lock().unwrap().reviews.insert((code.clone(), 1), json!([{ "id": 3, "user": { "login": "the-board" }, "state": "CHANGES_REQUESTED", "body": "Keep the order.", "submitted_at": "2026-10-29T10:00:00Z" }]));
    remote::poll(&run).await;
    let told: Vec<Value> = inbox(&run, "pat")
        .into_iter()
        .filter(|m| m["subject"].as_str().unwrap_or("").contains("Pull request"))
        .collect();
    assert_eq!(told.len(), 1, "{told:?}");
    let id = told[0]["id"].as_str().unwrap().to_string();
    let msg = run
        .post
        .with(|po| po.message(&run.journal, &id, "pat"))
        .unwrap();
    let body = msg["body"].as_str().unwrap_or_default().to_string();
    assert!(
        body.contains("1 failed: test")
            && body.contains("CHANGES_REQUESTED")
            && body.contains("Keep the order."),
        "{body}"
    );
    // Told once, not on every look.
    remote::poll(&run).await;
    assert_eq!(
        subjects(&run, "pat")
            .iter()
            .filter(|s| s.contains("Pull request"))
            .count(),
        1
    );
    // pr_status has the details: the failing job's log, the review.
    let st = remote::pr_status(&run, "pat", &code, "fix").await.unwrap();
    let failing = st["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "test")
        .unwrap()
        .clone();
    assert!(
        failing["log"].as_str().unwrap_or("").contains("not ok 2"),
        "{failing}"
    );
    assert_eq!(st["remarks"][0]["verdict"], "CHANGES_REQUESTED");
    // The board merges: the author hears, and main moves on the firm's
    // server.
    let merged = commit_on_remote(&w.base, &code, "merge.txt");
    w.fake.lock().unwrap().pulls.get_mut(&code).unwrap()[0]["merged"] = json!(true);
    w.fake.lock().unwrap().pulls.get_mut(&code).unwrap()[0]["state"] = json!("closed");
    remote::poll(&run).await;
    assert!(
        subjects(&run, "pat")
            .iter()
            .any(|s| s.starts_with("Merged:")),
        "{:?}",
        subjects(&run, "pat")
    );
    assert_eq!(
        rev(
            &remote::root(&run.config, &run.dir).join(format!("{code}.git")),
            "main"
        ),
        merged
    );
    remote::poll(&run).await;
    assert_eq!(
        subjects(&run, "pat")
            .iter()
            .filter(|s| s.starts_with("Merged:"))
            .count(),
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_local_main_is_pushed_out_and_stops_when_the_remote_moved() {
    let w = world();
    let (_t, run, _code, kb) = firm("local").await;
    remote::prepare(&run).await.unwrap();
    // Write can't merge to the firm's main; admin can, and it goes out.
    assert!(push_branch(&run, &kb, "main", "page.md", "write")
        .unwrap_err()
        .contains("needs admin"));
    let merged = push_branch(&run, &kb, "main", "page.md", "admin").unwrap();
    remote::push_main(&run, &kb).await.unwrap();
    assert_eq!(rev(&remote_dir(&w.base, &kb), "main"), merged);
    // Someone changes the remote's main by another route: nothing more is
    // pushed over it, and the board is told, once.
    let other = commit_on_remote(&w.base, &kb, "elsewhere.md");
    push_branch(&run, &kb, "main", "next.md", "admin").unwrap();
    remote::push_main(&run, &kb).await.unwrap();
    remote::push_main(&run, &kb).await.unwrap();
    assert_eq!(
        rev(&remote_dir(&w.base, &kb), "main"),
        other,
        "not force-pushed"
    );
    let told: Vec<String> = subjects(&run, crate::firm::BOARD)
        .into_iter()
        .filter(|s| s.contains("main changed on the remote"))
        .collect();
    assert_eq!(told.len(), 1, "{told:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_keeps_what_it_had_and_its_tokens_are_revoked() {
    let w = world();
    let (t, run, code, _kb) = firm("end").await;
    remote::prepare(&run).await.unwrap();
    access::board_grant(
        &t.core,
        &run,
        "pat",
        &format!("git:{code}"),
        "write",
        None,
        "x",
    )
    .unwrap();
    let during = push_branch(&run, &code, "during", "d.txt", "write").unwrap();
    remote::publish_branch(&run, "pat", &code, "during", "During", "")
        .await
        .unwrap();
    remote::finish(&run).await;
    assert!(
        w.fake
            .lock()
            .unwrap()
            .revoked
            .iter()
            .any(|t| t == "ghs_daemon"),
        "the daemon's token is revoked"
    );
    // After the run, its branches are as they were when it ended, whatever
    // a later run does; and its commits are only the ones it added.
    push_branch(&run, &code, "later", "later.txt", "write").unwrap();
    let b = crate::git::branches(&run.dir, Some(&code)).await.unwrap();
    let names: Vec<&str> = b
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"during") && !names.contains(&"later"),
        "{names:?}"
    );
    let during_then = b
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["name"] == "during")
        .unwrap()
        .clone();
    assert_eq!(during_then["commit"], during);
    let log = crate::facility::git_log(&run.dir).await;
    let commits: Vec<&str> = log
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["subject"].as_str().unwrap())
        .collect();
    assert_eq!(commits, ["add d.txt"], "{commits:?}");
}
