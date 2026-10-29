//! The git server over HTTP, as a worker's git makes its requests: its
//! checks (`authorize`, tested on its own in `run_tests`) refuse, and what
//! they let through reaches git.

use std::convert::Infallible;
use std::sync::Arc;

use base64::Engine as _;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request};
use hyper_util::rt::TokioIo;

use super::super::access;
use super::super::{test_run, FirmRun};

const FIRM: &str = "
profiles:
  w: { agent: { kind: claude-code } }
firms:
  acme:
    roles:
      ceo: { profile: w, agent: ceo }
      eng: { profile: w, agent: software-engineer }
    workers:
      - { name: ceo, role: ceo }
      - { name: pat, role: eng, reports_to: ceo }
    git: { repos: [api, web] }
";

/// The git server's answer to `member` asking for `path`, with `auth` as
/// their Basic credentials: its status and text.
async fn git(run: &Arc<FirmRun>, member: &str, auth: Option<&str>, path: &str) -> (u16, String) {
    let (client, server) = tokio::io::duplex(1 << 16);
    let (run, who) = (run.clone(), member.to_string());
    tokio::spawn(async move {
        let svc = hyper::service::service_fn(move |req| {
            let (run, who) = (run.clone(), who.clone());
            async move { Ok::<_, Infallible>(super::git_request(&run, &who, req).await) }
        });
        let _ = hyper::server::conn::http1::Builder::new()
            .serve_connection(TokioIo::new(server), svc)
            .await;
    });
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(client))
        .await
        .unwrap();
    tokio::spawn(conn);
    let method = if path.ends_with("-pack") {
        Method::POST
    } else {
        Method::GET
    };
    let mut req = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "git.acme");
    if let Some(a) = auth {
        let v = base64::engine::general_purpose::STANDARD.encode(a);
        req = req.header(hyper::header::AUTHORIZATION, format!("Basic {v}"));
    }
    let r = sender
        .send_request(req.body(Full::new(Bytes::new())).unwrap())
        .await
        .unwrap();
    let status = r.status().as_u16();
    let body = r.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&body).into_owned())
}

/// Past the checks: whatever git itself answers (an empty push is a bad
/// request to it), not one of their refusals.
fn allowed((status, text): &(u16, String)) -> bool {
    ![401, 403, 404].contains(status) && !text.contains("bad path")
}

#[tokio::test]
async fn the_git_server_checks_who_asks_and_their_grant_on_the_repository() {
    let t = crate::test_core(FIRM);
    let run = test_run(&t.core, "acme");
    // The repositories as the firm's git server keeps them.
    for r in ["api", "web"] {
        let bare = run.dir.join("git").join(format!("{r}.git"));
        std::fs::create_dir_all(&bare).unwrap();
        let ok = std::process::Command::new("git")
            .args(["init", "-q", "--bare"])
            .arg(&bare)
            .status()
            .unwrap();
        assert!(ok.success());
    }
    run.facility
        .git_passwords
        .lock()
        .unwrap()
        .insert("pat".into(), "pw".into());
    let refs = "/api.git/info/refs?service=git-upload-pack";
    let push = "/api.git/git-receive-pack";

    // Who: the password, for the worker the connection came from.
    assert_eq!(git(&run, "pat", None, refs).await.0, 401);
    assert_eq!(git(&run, "pat", Some("pat:wrong"), refs).await.0, 401);
    assert_eq!(git(&run, "pat", Some("ceo:pw"), refs).await.0, 401);

    // What: a grant on this repository.
    let (status, text) = git(&run, "pat", Some("pat:pw"), refs).await;
    assert_eq!(status, 403);
    assert!(text.contains("no access to api"), "{text}");

    access::board_grant(&t.core, &run, "pat", "git:api", "read", None, "test").unwrap();
    let r = git(&run, "pat", Some("pat:pw"), refs).await;
    assert!(allowed(&r), "{r:?}");
    let (status, text) = git(&run, "pat", Some("pat:pw"), push).await;
    assert_eq!(status, 403);
    assert!(text.contains("you can read api"), "{text}");
    assert_eq!(
        git(
            &run,
            "pat",
            Some("pat:pw"),
            "/api.git/info/refs?service=git-receive-pack"
        )
        .await
        .0,
        403,
        "asking to push is a push"
    );

    access::board_grant(&t.core, &run, "pat", "git:api", "write", None, "test").unwrap();
    let r = git(&run, "pat", Some("pat:pw"), push).await;
    assert!(allowed(&r), "{r:?}");
    // Merge pushes too: the repository's hook says where (into main only).
    access::board_grant(&t.core, &run, "pat", "git:api", "merge", None, "test").unwrap();
    let r = git(&run, "pat", Some("pat:pw"), push).await;
    assert!(allowed(&r), "{r:?}");
    // Merge is git's: not a level of the cluster or a drive space.
    assert!(
        access::board_grant(&t.core, &run, "pat", "drive:general", "merge", None, "test").is_err()
    );

    // Other repositories: their own grant; unknown ones aren't there.
    assert_eq!(
        git(&run, "pat", Some("pat:pw"), "/web.git/info/refs")
            .await
            .0,
        403
    );
    let (status, text) = git(&run, "pat", Some("pat:pw"), "/other.git/info/refs").await;
    assert_eq!(status, 404);
    assert!(
        text.contains("api.git") && text.contains("web.git"),
        "{text}"
    );
    assert_eq!(
        git(&run, "pat", Some("pat:pw"), "/api.git/../web.git/HEAD")
            .await
            .0,
        400
    );
}

/// The `pre-receive` hook as git runs it: a bare repository with it
/// installed, pushed to from a clone as `who` at a level, the way the git
/// handler sets them.
struct Hooked {
    dir: std::path::PathBuf,
}

impl Hooked {
    fn new(name: &str) -> Hooked {
        let dir = std::env::temp_dir().join(format!("zapper-hook-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let h = Hooked { dir };
        h.git(&["init", "-q", "--bare", "-b", "main", "repo.git"], ".");
        let hook = h.dir.join("repo.git/hooks/pre-receive");
        std::fs::write(&hook, super::PRE_RECEIVE).unwrap();
        std::fs::set_permissions(&hook, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        h.git(&["clone", "-q", "repo.git", "work"], ".");
        h.commit("ceo", "README.md", "hello\n");
        assert!(h.push("ceo", "admin", "main").0, "admin starts main");
        h
    }

    fn git(&self, args: &[&str], at: &str) -> String {
        let out = std::process::Command::new("git")
            .current_dir(self.dir.join(at))
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A commit in the clone by `author`, on whatever is checked out.
    fn commit(&self, author: &str, file: &str, text: &str) {
        std::fs::write(self.dir.join("work").join(file), text).unwrap();
        self.git(&["add", file], "work");
        let who = format!("user.name={author}");
        let mail = format!("user.email={author}@acme.internal");
        self.git(
            &["-c", &who, "-c", &mail, "commit", "-q", "-m", file],
            "work",
        );
    }

    fn branch(&self, name: &str, from: &str) {
        self.git(&["checkout", "-q", "-B", name, from], "work");
    }

    /// Pushes `refspec` as `who` at `level`: whether it was taken, and what
    /// the hook said.
    fn push(&self, who: &str, level: &str, refspec: &str) -> (bool, String) {
        let out = std::process::Command::new("git")
            .current_dir(self.dir.join("work"))
            .args(["push", "-q", "origin", refspec])
            .env("REMOTE_USER", who)
            .env("ZAPPER_GIT_LEVEL", level)
            .output()
            .unwrap();
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    /// Merges `branch` into the clone's main as git does, by `who`.
    fn merge(&self, who: &str, branch: &str) {
        self.git(&["checkout", "-q", "main"], "work");
        self.git(&["reset", "-q", "--hard", "origin/main"], "work");
        let name = format!("user.name={who}");
        let mail = format!("user.email={who}@acme.internal");
        self.git(
            &[
                "-c",
                &name,
                "-c",
                &mail,
                "merge",
                "-q",
                "--no-ff",
                "-m",
                "merge",
                &format!("origin/{branch}"),
            ],
            "work",
        );
    }
}

#[test]
fn merge_merges_someone_elses_branch_into_main_and_nothing_else() {
    let h = Hooked::new("merge");
    // Write pushes branches, not main.
    h.branch("pat/a", "main");
    h.commit("pat", "a.md", "a\n");
    assert!(h.push("pat", "write", "pat/a").0);
    let (ok, why) = h.push("pat", "write", "pat/a:main");
    assert!(!ok && why.contains("needs merge"), "{why}");
    // Merge pushes no branches of its own.
    h.branch("lin/x", "main");
    h.commit("lin", "x.md", "x\n");
    let (ok, why) = h.push("lin", "merge", "lin/x");
    assert!(!ok && why.contains("push nothing else"), "{why}");
    // It merges pat's branch, as git makes the merge.
    h.merge("lin", "pat/a");
    let (ok, why) = h.push("lin", "merge", "main");
    assert!(ok, "{why}");

    // A merge that adds something of its own is refused.
    h.branch("pat/b", "origin/main");
    h.commit("pat", "b.md", "b\n");
    assert!(h.push("pat", "write", "pat/b").0);
    h.merge("lin", "pat/b");
    std::fs::write(h.dir.join("work/extra.md"), "smuggled\n").unwrap();
    h.git(&["add", "extra.md"], "work");
    h.git(
        &[
            "-c",
            "user.name=lin",
            "-c",
            "user.email=lin@acme.internal",
            "commit",
            "-q",
            "--amend",
            "--no-edit",
        ],
        "work",
    );
    let (ok, why) = h.push("lin", "merge", "main");
    assert!(!ok && why.contains("add nothing of its own"), "{why}");
    // Nor can main move to a commit that is no branch's.
    let (ok, why) = h.push("lin", "merge", "HEAD:main");
    assert!(!ok, "{why}");

    // A fast-forward to someone else's branch is a merge too.
    h.branch("pat/ff", "origin/main");
    h.commit("pat", "ff.md", "ff\n");
    assert!(h.push("pat", "write", "pat/ff").0);
    let (ok, why) = h.push("lin", "merge", "pat/ff:main");
    assert!(ok, "{why}");
}

#[test]
fn nobody_merges_their_own_work() {
    let h = Hooked::new("own");
    // What lin pushed, while she held write, she can't merge with merge.
    h.branch("lin/c", "main");
    h.commit("lin", "c.md", "c\n");
    assert!(h.push("lin", "write", "lin/c").0);
    h.merge("lin", "lin/c");
    let (ok, why) = h.push("lin", "merge", "main");
    assert!(!ok && why.contains("lin pushed"), "{why}");
    // Nor what she wrote, though pat pushed it.
    h.branch("pat/d", "origin/main");
    h.commit("lin", "d.md", "d\n");
    assert!(h.push("pat", "write", "pat/d").0);
    h.merge("lin", "pat/d");
    let (ok, why) = h.push("lin", "merge", "main");
    assert!(!ok && why.contains("lin wrote"), "{why}");
    // Someone else may merge it: the review the rule asks for.
    h.merge("ken", "pat/d");
    let (ok, why) = h.push("ken", "merge", "main");
    assert!(ok, "{why}");
}
