//! The office's routes over runs on disk, made here: the list of runs,
//! notes, deleting runs, journals and transcripts, a run's config, and the
//! git browser over real bare repositories.

use std::path::{Path, PathBuf};
use std::process::Command;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

use crate::testing::TestCore;

const FIRM: &str = "
profiles:
  w: { agent: { kind: claude-code } }
firms:
  acme:
    roles:
      ceo: { profile: w, skills: { work: ceo } }
    workers:
      - { name: ceo, role: ceo }
";

const OLD: &str = "20261020-090000-abc";
const NEW: &str = "20261021-090000-def";

/// A request to the office's routes: its status and body (JSON, or the
/// text as a string).
async fn call(t: &TestCore, method: Method, uri: &str, body: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::from(body.to_string()))
        .unwrap();
    let r = t.core.office_routes().oneshot(req).await.unwrap();
    let status = r.status();
    let bytes = http_body_util::BodyExt::collect(r.into_body())
        .await
        .unwrap()
        .to_bytes();
    let text = String::from_utf8_lossy(&bytes).into_owned();
    (
        status,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

async fn get(t: &TestCore, uri: &str) -> (StatusCode, Value) {
    call(t, Method::GET, uri, "").await
}

fn run_dir(t: &TestCore, run: &str) -> PathBuf {
    let dir = t.core.state_dir().join("firms/acme/runs").join(run);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn journal(lines: &[Value]) -> String {
    lines.iter().map(|l| format!("{l}\n")).collect()
}

/// Two past runs: the old one kicked off by the board and ended, the new
/// one never kicked off.
fn two_runs(t: &TestCore) {
    let old = run_dir(t, OLD);
    std::fs::write(old.join("run.json"), r#"{"started": 100.0}"#).unwrap();
    std::fs::write(
        old.join("journal.jsonl"),
        journal(&[
            json!({ "ts": 100.0, "type": "run", "state": "running" }),
            json!({ "ts": 101.0, "type": "mail", "from": "board", "to": ["ceo"], "subject": "Build it", "body": "A todo\n  app,  please" }),
            json!({ "ts": 102.0, "type": "mail", "from": "ceo", "to": ["board"], "subject": "Re: Build it", "body": "On it" }),
            json!({ "ts": 103.0, "type": "turn", "worker": "ceo", "padding": "x".repeat(80) }),
            json!({ "ts": 104.0, "type": "run", "state": "ended" }),
        ]),
    )
    .unwrap();
    let new = run_dir(t, NEW);
    std::fs::write(new.join("run.json"), r#"{"started": 200.0}"#).unwrap();
    // Not runs: other names in the directory.
    std::fs::create_dir_all(old.parent().unwrap().join("not-a-run")).unwrap();
}

#[tokio::test]
async fn past_runs_newest_first_with_what_their_journal_says() {
    let t = TestCore::new(FIRM);
    two_runs(&t);
    let (status, runs) = get(&t, "/firms/acme/runs").await;
    assert_eq!(status, StatusCode::OK);
    let runs = runs.as_array().unwrap();
    assert_eq!(runs.len(), 2, "{runs:?}");
    assert_eq!(runs[0]["run"], NEW);
    assert_eq!(runs[0]["started"], 200.0);
    assert!(runs[0]["kickoff"].is_null());
    assert_eq!(runs[0]["messages"], 0);
    assert_eq!(runs[0]["ended"], false);

    let old = &runs[1];
    assert_eq!(old["run"], OLD);
    assert_eq!(old["messages"], 2);
    assert_eq!(old["last"], 104.0);
    assert_eq!(old["ended"], true);
    assert_eq!(old["kickoff"]["subject"], "Build it");
    assert_eq!(old["kickoff"]["preview"], "A todo app, please");
    assert_eq!(old["kickoff"]["body"], "A todo\n  app,  please");
    assert!(old["note"].is_null());
    assert!(old["journal_bytes"].as_u64().unwrap() > 0);

    // A firm neither configured nor on disk.
    let (status, e) = get(&t, "/firms/nosuch/runs").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{e}");
}

#[tokio::test]
async fn notes_on_runs_and_deleting_runs() {
    let t = TestCore::new(FIRM);
    two_runs(&t);
    let note = |run: &str| format!("/firms/acme/runs/{run}/note");

    let (status, v) = call(
        &t,
        Method::PUT,
        &note(OLD),
        r#"{"note": "  the good one "}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["note"], "the good one");
    let (_, runs) = get(&t, "/firms/acme/runs").await;
    assert_eq!(runs[1]["note"], "the good one");

    let long = json!({ "note": "x".repeat(5000) }).to_string();
    let (status, v) = call(&t, Method::PUT, &note(OLD), &long).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{v}");
    let (status, _) = call(&t, Method::PUT, &note(OLD), "{}").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "a note is required");

    // Empty removes it, and removing none is fine.
    for _ in 0..2 {
        let (status, _) = call(&t, Method::PUT, &note(OLD), r#"{"note": ""}"#).await;
        assert_eq!(status, StatusCode::OK);
    }
    let (_, runs) = get(&t, "/firms/acme/runs").await;
    assert!(runs[1]["note"].is_null());

    // Run ids only, of runs that exist.
    let (status, _) = call(
        &t,
        Method::PUT,
        &note("20261019-090000-xyz"),
        r#"{"note":"x"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(&t, Method::PUT, &note("not-a-run"), r#"{"note":"x"}"#).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = call(&t, Method::DELETE, "/firms/acme/runs/current", "").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "no current run");

    let (status, v) = call(&t, Method::DELETE, &format!("/firms/acme/runs/{OLD}"), "").await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["removed"], OLD);
    let (_, runs) = get(&t, "/firms/acme/runs").await;
    assert_eq!(runs.as_array().unwrap().len(), 1);
    let (status, _) = call(&t, Method::DELETE, &format!("/firms/acme/runs/{OLD}"), "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn deleting_the_running_run_is_refused() {
    let t = TestCore::new(FIRM);
    let run = super::test_run(&t.core, "acme");
    let (status, v) = call(
        &t,
        Method::DELETE,
        &format!("/firms/acme/runs/{}", run.id),
        "",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{v}");
    assert!(v["error"].as_str().unwrap().contains("is running"), "{v}");
    assert!(run.dir.is_dir());
}

#[tokio::test]
async fn journals_transcripts_and_a_runs_config() {
    let t = TestCore::new(FIRM);
    two_runs(&t);
    let old = run_dir(&t, OLD);
    std::fs::create_dir_all(old.join("transcripts")).unwrap();
    std::fs::write(old.join("transcripts/ceo.jsonl"), "{\"n\":1}\n").unwrap();
    std::fs::write(old.join("config.yaml"), "firms: {}\n").unwrap();
    let skill = old.join("skills/ceo/work");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(skill.join("SKILL.md"), "# Work").unwrap();
    std::fs::write(skill.join("notes.txt"), "").unwrap();

    let base = format!("/firms/acme/runs/{OLD}");
    let (status, j) = get(&t, &format!("{base}/journal")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(j.as_str().unwrap().lines().count(), 5);
    let (status, tr) = get(&t, &format!("{base}/transcripts/ceo")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(tr, json!({ "n": 1 }));
    let (status, _) = get(&t, &format!("{base}/transcripts/pat")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = get(&t, &format!("{base}/transcripts/..%2Fjournal")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = get(&t, &format!("/firms/acme/runs/{NEW}/journal")).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "no journal yet");

    let (status, c) = get(&t, &format!("{base}/config")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(c["config"], "firms: {}\n");
    assert_eq!(
        c["skills"],
        json!([{ "role": "ceo", "skill": "work", "text": "# Work", "files": ["notes.txt"] }])
    );

    let (status, _) = get(&t, "/firms/a..b/runs/x/journal").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// The git browser, over bare repositories as a run's git server keeps them.

fn git(dir: &Path, args: &[&str], ts: u64) {
    let date = format!("@{ts} +0000");
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "init.defaultBranch=main"])
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        // Over any identity in the environment running the tests.
        .env("GIT_AUTHOR_NAME", "Ada")
        .env("GIT_AUTHOR_EMAIL", "ada@acme")
        .env("GIT_COMMITTER_NAME", "Ada")
        .env("GIT_COMMITTER_EMAIL", "ada@acme")
        .env("GIT_AUTHOR_DATE", &date)
        .env("GIT_COMMITTER_DATE", &date)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `name.git` in the run: commits at `ts`, each `(file, content, message)`,
/// on main, then a branch `feature/x` with one more.
fn repo(run: &Path, name: &str, commits: &[(&str, &[u8], &str, u64)]) {
    let bare = run.join("git").join(format!("{name}.git"));
    std::fs::create_dir_all(&bare).unwrap();
    git(&bare, &["init", "-q", "--bare"], 0);
    let work = run.join(format!("work-{name}"));
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q"], 0);
    for (file, content, msg, ts) in commits {
        let p = work.join(file);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, content).unwrap();
        git(&work, &["add", "-A"], *ts);
        git(&work, &["commit", "-q", "-m", msg], *ts);
    }
    git(&work, &["push", "-q", bare.to_str().unwrap(), "main"], 0);
    git(&work, &["checkout", "-q", "-b", "feature/x"], 0);
    std::fs::write(work.join("feature.txt"), "f").unwrap();
    git(&work, &["add", "-A"], 5000);
    git(&work, &["commit", "-q", "-m", "Feature"], 5000);
    git(
        &work,
        &["push", "-q", bare.to_str().unwrap(), "feature/x"],
        0,
    );
    std::fs::remove_dir_all(&work).unwrap();
}

fn repos(t: &TestCore) -> String {
    let run = run_dir(t, OLD);
    repo(
        &run,
        "product",
        &[
            (
                "README.md",
                b"# Todo\n",
                "Add the README\n\nWhat it is.\n\nCo-Authored-By: Claude <noreply@anthropic.com>",
                1000,
            ),
            ("src/app.py", b"print('hi')\n", "Add the app", 3000),
            ("logo.bin", &[0, 159, 146, 150], "Add the logo", 4000),
        ],
    );
    repo(
        &run,
        "api",
        &[("api.py", b"x = 1\n", "Start the API", 2000)],
    );
    // Not a repository's name.
    std::fs::create_dir_all(run.join("git/Bad Name.git")).unwrap();
    format!("/firms/acme/runs/{OLD}/git")
}

#[tokio::test]
async fn the_git_browser_reads_a_runs_repositories() {
    let t = TestCore::new(FIRM);
    let base = repos(&t);

    let (_, r) = get(&t, &format!("{base}/repos")).await;
    assert_eq!(r, json!(["product", "api"]), "product first, then by name");

    let (status, b) = get(&t, &format!("{base}/branches")).await;
    assert_eq!(status, StatusCode::OK, "{b}");
    let names: Vec<&str> = b
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["main", "feature/x"], "main first though older");
    assert_eq!(b[0]["subject"], "Add the logo");
    assert_eq!(b[0]["author"], "Ada");

    let (_, log) = get(&t, &format!("{base}/log")).await;
    let subjects: Vec<&str> = log
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["subject"].as_str().unwrap())
        .collect();
    assert_eq!(subjects, ["Add the logo", "Add the app", "Add the README"]);
    assert_eq!(
        log[2]["body"], "What it is.",
        "the description, not the trailer"
    );
    assert_eq!(log[0]["ts"], 4000.0);
    let (_, one) = get(&t, &format!("{base}/log?limit=1&ref=feature/x")).await;
    assert_eq!(one.as_array().unwrap().len(), 1);
    assert_eq!(one[0]["subject"], "Feature");
    let (_, api) = get(&t, &format!("{base}/log?repo=api")).await;
    assert_eq!(api[0]["subject"], "Start the API");

    let (_, tree) = get(&t, &format!("{base}/tree")).await;
    let entries: Vec<(&str, &str)> = tree
        .as_array()
        .unwrap()
        .iter()
        .map(|e| (e["name"].as_str().unwrap(), e["type"].as_str().unwrap()))
        .collect();
    assert_eq!(
        entries,
        [("src", "tree"), ("logo.bin", "blob"), ("README.md", "blob")]
    );
    assert_eq!(tree[2]["size"], 7);
    let (_, sub) = get(&t, &format!("{base}/tree?path=src")).await;
    assert_eq!(sub[0]["name"], "app.py");

    let (_, f) = get(&t, &format!("{base}/blob?path=src/app.py")).await;
    assert_eq!(f["text"], "print('hi')\n");
    assert_eq!(f["size"], 12);
    let (_, bin) = get(&t, &format!("{base}/blob?path=logo.bin")).await;
    assert!(bin["text"].is_null());
    assert_eq!(bin["note"], "binary file");

    let id = log[1]["commit"].as_str().unwrap();
    let (status, c) = get(&t, &format!("{base}/commits/{id}")).await;
    assert_eq!(status, StatusCode::OK, "{c}");
    assert_eq!(c["message"], "Add the app");
    assert_eq!(c["parents"].as_array().unwrap().len(), 1);
    assert_eq!(
        c["files"],
        json!([{ "added": "1", "removed": "0", "path": "src/app.py" }])
    );
    assert!(c["diff"].as_str().unwrap().contains("+print('hi')"));
    assert_eq!(c["truncated"], false);
    let (_, short) = get(&t, &format!("{base}/commits/{}", &id[..7])).await;
    assert_eq!(short["commit"], id);
}

#[tokio::test]
async fn the_git_browser_refuses_what_isnt_a_ref_path_or_repository() {
    let t = TestCore::new(FIRM);
    let base = repos(&t);
    let bad = [
        "log?ref=--output=/tmp/x",
        "log?ref=main..feature/x",
        "tree?path=../x",
        "tree?path=/etc",
        "blob?path=a//b",
        "blob",
        "branches?repo=Bad%20Name",
        "branches?repo=other",
        "commits/main",
        "commits/abc",
    ];
    for q in bad {
        let (status, v) = get(&t, &format!("{base}/{q}")).await;
        assert!(status.is_client_error(), "{q}: {status} {v}");
    }
    let (status, v) = get(&t, &format!("{base}/branches?repo=other")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        v["error"].as_str().unwrap().contains("no repository"),
        "{v}"
    );
    // Well-formed, but not there.
    let (status, _) = get(&t, &format!("{base}/log?ref=nosuch")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = get(&t, &format!("{base}/blob?path=nosuch.txt")).await;
    assert!(status.is_client_error());
    let (status, _) = get(&t, &format!("{base}/commits/{}", "0".repeat(40))).await;
    assert!(status.is_client_error());

    // No such run; then a run without repositories.
    let (status, v) = get(&t, &format!("/firms/acme/runs/{NEW}/git/branches")).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{v}");
    run_dir(&t, NEW);
    let (status, v) = get(&t, &format!("/firms/acme/runs/{NEW}/git/branches")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{v}");
    assert!(
        v["error"].as_str().unwrap().contains("no git repository"),
        "{v}"
    );
}

#[tokio::test]
async fn the_runs_git_log_is_every_repository_newest_first() {
    let t = TestCore::new(FIRM);
    let base = repos(&t);
    let (_, all) = get(&t, &base).await;
    let seen: Vec<(&str, &str)> = all
        .as_array()
        .unwrap()
        .iter()
        .map(|c| (c["repo"].as_str().unwrap(), c["subject"].as_str().unwrap()))
        .collect();
    // Both repositories' branches: their features at the same time.
    assert_eq!(seen.len(), 6, "{seen:?}");
    assert!(seen[..2].iter().all(|(_, s)| *s == "Feature"));
    assert_eq!(
        &seen[2..],
        [
            ("product", "Add the logo"),
            ("product", "Add the app"),
            ("api", "Start the API"),
            ("product", "Add the README"),
        ]
    );
    let main = all
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["subject"] == "Add the logo")
        .unwrap();
    assert!(main["refs"].as_str().unwrap().contains("main"), "{main}");
}
