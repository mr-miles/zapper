//! Applying the repository, against zapper's API on a `TestCore` (no
//! VMs), with the shop's layout (`tests/shop`) as its first commit: checked
//! without being applied, then applied, saved, and what it left behind.

use std::path::Path;
use std::sync::Arc;

use zapper_core::prelude::*;
use zapper_core::testing::TestCore;
use zapper_core::NewSession;
use zapper_workbench::facility::Facilities;
use zapper_workbench::gateway::{self, Ctx, Live, CONFIG_LABEL};
use zapper_workbench::git::{ApplySettings, Git};
use zapper_workbench::tokens::Tokens;
use zapper_workbench::zapper::Zapper;

const CONFIG: &str = "profiles:\n  old: {}\n";

fn git(dir: &Path, args: &[&str]) -> String {
    let o = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", dir)
        .env("GIT_AUTHOR_NAME", "me")
        .env("GIT_AUTHOR_EMAIL", "me@example.com")
        .env("GIT_COMMITTER_NAME", "me")
        .env("GIT_COMMITTER_EMAIL", "me@example.com")
        .output()
        .unwrap();
    assert!(
        o.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

fn copy(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap().flatten() {
        let (s, d) = (e.path(), to.join(e.file_name()));
        if s.is_dir() {
            copy(&s, &d)
        } else {
            std::fs::copy(&s, &d).unwrap();
        }
    }
}

struct Bench {
    t: TestCore,
    ctx: Ctx,
    clone: std::path::PathBuf,
    live: Live,
}

async fn bench() -> Bench {
    let t = TestCore::new(CONFIG);
    let socket = t.dir.join("api.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let api = zapper_api::api(t.core.clone(), &zapper_api::Mount::default());
    tokio::spawn(async move { axum::serve(listener, api).await });
    // The office image, as if built: an apply doesn't build what zapper has.
    let images = t.dir.join("images");
    std::fs::create_dir_all(&images).unwrap();
    std::fs::write(images.join("office.ext4"), b"").unwrap();
    let live = Live::default();
    let facility = Git::new(
        t.dir.join("git"),
        ApplySettings {
            secrets: t.dir.join("secrets"),
            daemon: None,
            live: live.clone(),
        },
    )
    .unwrap();
    let repo = t.dir.join("git").join("config.git");
    let ctx = Ctx {
        zapper: Zapper::new(socket),
        tokens: Tokens::open(t.dir.join("wb")).unwrap(),
        facilities: Arc::new(Facilities::new(vec![Arc::new(facility)]).unwrap()),
        work: t.dir.join("work"),
        port: 0,
        remote: false,
        live: live.clone(),
    };
    // The user's clone, with the shop's layout as main's first commit.
    let clone = t.dir.join("mine");
    git(
        &t.dir,
        &["clone", "--quiet", repo.to_str().unwrap(), "mine"],
    );
    copy(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/shop"),
        &clone,
    );
    git(&clone, &["add", "."]);
    git(&clone, &["commit", "--quiet", "-m", "the shop"]);
    git(&clone, &["push", "--quiet", "origin", "HEAD:main"]);
    Bench {
        t,
        ctx,
        clone,
        live,
    }
}

/// The git facility's `path`, as the owner.
async fn call(b: &Bench, method: &str, path: &str, body: &str) -> (u16, serde_json::Value) {
    use tower::ServiceExt as _;
    let token = b.ctx.tokens.owner().unwrap();
    let req = axum::http::Request::builder()
        .method(method)
        .uri(format!("/w/v1/git{path}"))
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(axum::body::Body::from(body.to_string()))
        .unwrap();
    let r = gateway::app(b.ctx.clone()).oneshot(req).await.unwrap();
    let status = r.status().as_u16();
    let bytes = http_body_util::BodyExt::collect(r.into_body())
        .await
        .unwrap()
        .to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn a_commit_is_checked_then_applied_as_zappers_whole_config_and_saved() {
    let b = bench().await;
    let (s, v) = call(&b, "POST", "/check", "{}").await;
    assert_eq!(s, 200, "{v}");
    let diff = v["diff"].as_str().unwrap();
    assert!(
        diff.contains("+  engineer:") && diff.contains("-  old:"),
        "{diff}"
    );
    assert!(
        b.t.core.config().profiles.contains_key("old"),
        "a check applies nothing"
    );

    let (s, v) = call(&b, "POST", "/apply", "").await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(
        v["building"],
        serde_json::json!([]),
        "the office image is there already"
    );
    let c = b.t.core.config();
    assert!(
        !c.profiles.contains_key("old"),
        "the repository's config, wholesale"
    );
    let engineer = c.resolve("engineer").unwrap();
    assert!(engineer
        .agent
        .instructions
        .as_deref()
        .unwrap()
        .starts_with("You work with the user"));
    let skills = Path::new(engineer.agent.skills.as_deref().unwrap());
    assert!(skills.join("reviewing-work/SKILL.md").exists());
    let pm = c.resolve("product-manager").unwrap();
    assert!(Path::new(pm.agent.skills.as_deref().unwrap())
        .join("product-brief/SKILL.md")
        .exists());
    assert_eq!(
        serde_json::to_value(&c.secrets["claude"]).unwrap()["file"],
        b.t.dir.join("secrets/claude").display().to_string(),
        "the secrets facility's file"
    );
    let saved = std::fs::read_to_string(b.t.startup()).unwrap();
    assert!(saved.contains("product-manager:"), "saved: {saved}");
    let commit = v["commit"].as_str().unwrap().to_string();
    assert_eq!(b.live.get().unwrap(), commit[..12]);

    // A session made from it, and one from before: the second is behind.
    let mut new = NewSession::plain("engineer".into(), b.t.workdir(), Default::default());
    new.labels = [(CONFIG_LABEL.to_string(), "000000000000".to_string())].into();
    let old = b.t.core.test_session(new, true).unwrap();
    let (_, o) = call(&b, "GET", "", "").await;
    assert_eq!(o["config"]["live"]["commit"], commit.as_str());
    let behind = o["config"]["out_of_date"].as_array().unwrap();
    assert_eq!(behind.len(), 1);
    assert_eq!(behind[0]["id"], old.id.as_str());
    assert!(old.is_running(), "flagged, not restarted");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_branch_is_checked_and_what_zapper_refuses_isnt_applied() {
    let b = bench().await;
    call(&b, "POST", "/apply", "").await;
    let applied = b.t.core.config().to_yaml();

    // A branch that changes the image's recipe: checked, it says so.
    git(&b.clone, &["checkout", "--quiet", "-b", "newer-node"]);
    let recipe = b.clone.join("images/office.yaml");
    let text = std::fs::read_to_string(&recipe).unwrap();
    std::fs::write(&recipe, format!("{text}\n# a change\nafter: [\"true\"]\n")).unwrap();
    git(&b.clone, &["commit", "--quiet", "-am", "newer"]);
    git(&b.clone, &["push", "--quiet", "origin", "newer-node"]);
    let (s, v) = call(&b, "POST", "/check", r#"{"ref":"newer-node"}"#).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(
        v["images"],
        serde_json::json!(["office"]),
        "it would build the image again"
    );

    // A branch zapper refuses: a proxy secret with no hosts.
    git(&b.clone, &["checkout", "--quiet", "-b", "broken", "main"]);
    std::fs::write(b.clone.join("secrets.yaml"), "claude: {}\n").unwrap();
    git(&b.clone, &["commit", "--quiet", "-am", "broken"]);
    git(&b.clone, &["push", "--quiet", "origin", "broken"]);
    let (s, v) = call(&b, "POST", "/check", r#"{"ref":"broken"}"#).await;
    assert_eq!(s, 400);
    assert!(v["error"].as_str().unwrap().contains("needs hosts"), "{v}");
    let (s, _) = call(&b, "POST", "/check", r#"{"ref":"--upload-pack=x"}"#).await;
    assert_eq!(s, 400);
    // Merged to main, an apply is refused too, and zapper's config stays.
    git(&b.clone, &["push", "--quiet", "origin", "broken:main"]);
    let (s, _) = call(&b, "POST", "/apply", "").await;
    assert_eq!(s, 400);
    assert_eq!(b.t.core.config().to_yaml(), applied);
}
