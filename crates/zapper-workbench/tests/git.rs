//! The workbench's repository, end to end over a `TestCore` (no VMs), with
//! the real git client: a session's git through zapper's proxy (TLS with
//! the session's CA, net rules with paths), the caller line, `git
//! http-backend` and the office's pre-receive hook; the user's pushes on the
//! host.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use zapper_core::testing::{dial, guest_tls, TestCore};
use zapper_core::{NewSession, Session};
use zapper_workbench::facility::{self, Facilities};
use zapper_workbench::gateway::{self, Ctx};
use zapper_workbench::git::{Git, NAME, REPO};
use zapper_workbench::tokens::Tokens;
use zapper_workbench::zapper::Zapper;

const CONFIG: &str = "profiles:\n  web: {}\n";

/// `git ARGS` in `dir`, with a home of its own and a name to commit as:
/// whether it worked, and what it said.
fn git(dir: &Path, home: &Path, args: &[&str]) -> (bool, String) {
    let o = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "someone")
        .env("GIT_AUTHOR_EMAIL", "someone@example.com")
        .env("GIT_COMMITTER_NAME", "someone")
        .env("GIT_COMMITTER_EMAIL", "someone@example.com")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap();
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
    (o.status.success(), said)
}

/// A local port whose connections are the session's to git.zapper:443:
/// through zapper's proxy, TLS with the session's CA, as git in the guest
/// would reach it.
async fn bridge(t: &Arc<TestCore>, s: &Arc<Session>) -> u16 {
    let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = tcp.local_addr().unwrap().port();
    let (t, s) = (t.clone(), s.clone());
    tokio::spawn(async move {
        while let Ok((mut plain, _)) = tcp.accept().await {
            let io = dial(&t, &s, NAME, 443);
            let s = s.clone();
            tokio::spawn(async move {
                let name = rustls::pki_types::ServerName::try_from(NAME).unwrap();
                let Ok(mut tls) = guest_tls(&s).connect(name, io).await else {
                    return;
                };
                let _ = tokio::io::copy_bidirectional(&mut plain, &mut tls).await;
            });
        }
    });
    port
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_proposes_a_branch_and_main_stays_the_users() {
    let t = Arc::new(TestCore::new(CONFIG));
    let socket = t.dir.join("api.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let api = zapper_api::api(t.core.clone(), &zapper_api::Mount::default());
    tokio::spawn(async move { axum::serve(listener, api).await });
    let git_root = t.dir.join("git");
    let facility = Arc::new(Git::new(git_root.clone()).unwrap());
    let repo = facility.repo();
    let ctx = Ctx {
        zapper: Zapper::new(socket),
        tokens: Tokens::open(t.dir.join("wb")).unwrap(),
        facilities: Arc::new(Facilities::new(vec![facility]).unwrap()),
        work: t.dir.join("work"),
        port: 0,
        remote: false,
    };
    for f in ctx.facilities.iter() {
        f.start(&ctx);
    }
    facility::register(&ctx).await.unwrap();
    let sock = t.core.socket_dir().join(zapper_workbench::git::SOCKET);
    for _ in 0..200 {
        if sock.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let _app = gateway::app(ctx.clone());
    let home = t.dir.join("home");
    std::fs::create_dir_all(&home).unwrap();

    // The user starts main on the host.
    let mine = t.dir.join("mine");
    let (ok, said) = git(
        &t.dir,
        &home,
        &["clone", "--quiet", repo.to_str().unwrap(), "mine"],
    );
    assert!(ok, "{said}");
    std::fs::write(mine.join("README.md"), "config\n").unwrap();
    git(&mine, &home, &["add", "."]);
    git(&mine, &home, &["commit", "--quiet", "-m", "start"]);
    let (ok, said) = git(&mine, &home, &["push", "--quiet", "origin", "HEAD:main"]);
    assert!(ok, "the user's push to main: {said}");

    // A workbench session, through the proxy.
    let mut new = NewSession::plain("web".into(), t.workdir(), Default::default());
    new.labels = [("workbench".to_string(), "1".to_string())].into();
    let s = t.core.test_session(new, true).unwrap();
    let port = bridge(&t, &s).await;
    let url = format!("http://127.0.0.1:{port}/{REPO}");
    let theirs = t.dir.join("theirs");

    let (ok, said) = git(&t.dir, &home, &["clone", "--quiet", &url, "theirs"]);
    assert!(!ok, "no rule yet: {said}");
    zapper_workbench::facility::add_global_rules(&ctx, &zapper_workbench::git::usual_rules())
        .await
        .unwrap();
    let (ok, said) = git(&t.dir, &home, &["clone", "--quiet", &url, "theirs"]);
    assert!(ok, "{said}");
    assert_eq!(
        std::fs::read_to_string(theirs.join("README.md")).unwrap(),
        "config\n"
    );

    std::fs::write(theirs.join("profile.yaml"), "idea\n").unwrap();
    git(&theirs, &home, &["add", "."]);
    git(&theirs, &home, &["commit", "--quiet", "-m", "a proposal"]);
    let (ok, said) = git(
        &theirs,
        &home,
        &["push", "--quiet", "origin", "HEAD:proposal"],
    );
    assert!(ok, "a branch: {said}");
    let (ok, said) = git(&theirs, &home, &["push", "--quiet", "origin", "HEAD:main"]);
    assert!(!ok, "main: {said}");
    assert!(
        said.contains("push a branch"),
        "the office's hook says why: {said}"
    );

    // The user sees it, pushed as the session.
    let refs = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(&repo)
        .args([
            "for-each-ref",
            "--format=%(refname:short) %(committername)",
            "refs/heads/",
        ])
        .output()
        .unwrap();
    let refs = String::from_utf8_lossy(&refs.stdout).into_owned();
    assert!(refs.contains("main someone"), "{refs}");
    assert!(refs.contains("proposal"), "{refs}");
    let pushed = std::fs::read_to_string(repo.join("zapper-pushed")).unwrap();
    assert!(
        pushed.contains(&s.id),
        "the hook recorded who pushed it: {pushed}"
    );
}
