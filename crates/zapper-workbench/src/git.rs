//! The workbench's repository, served to sessions as `git.zapper`, so an
//! agent can propose a change on a branch, which the user merges.
//!
//! The repository is `config.git`, bare, under the workbench's data; the
//! user clones it and pushes to it on the host. Sessions reach it as a
//! socket service (the drive's way): zapper terminates TLS, names the
//! caller and decides each request against `net` rules, then `git
//! http-backend` answers it (`zapper-git`, as the office's git server does)
//! with the session as the pusher at the `write` level: its `pre-receive`
//! hook, the office's, lets it push branches and never `main`, `main/…` or
//! tags. A push on the host itself, with no level, is the user's.
//!
//! Applying `main` to zapper's config on each commit is the repository's
//! own phase (proposal 12, phase 3); this serves it.

use std::path::{Path, PathBuf};

use axum::body::Body;
use axum::extract::Extension;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use http_body_util::BodyExt;
use serde_json::{json, Value};

use crate::facility::{Facility, Panel};
use crate::gateway::{Ctx, Error};
use crate::tokens::Grant;

/// The name sessions reach it by, and its socket's in `state/services/`.
pub const NAME: &str = "git.zapper";
pub const SOCKET: &str = "workbench-git.sock";
/// The repository's name, under the root.
pub const REPO: &str = "config.git";
/// Largest push taken.
const MAX_BODY: usize = 512 << 20;

/// The rule that lets the workbench's sessions clone, fetch and push
/// branches (the hook keeps `main` the user's).
pub fn usual_rules() -> [Value; 1] {
    [
        json!({ "id": "git-config", "kind": "net", "action": "allow", "host": NAME, "ports": [443], "methods": ["GET", "POST"], "path": format!("/{REPO}/**") }),
    ]
}

pub struct Git {
    root: PathBuf,
}

fn git(args: &[&str], dir: &Path) -> anyhow::Result<String> {
    let out = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|e| anyhow::anyhow!("git: {e} (is git installed?)"))?;
    if !out.status.success() {
        anyhow::bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

impl Git {
    /// The repository under `root`, made (bare, `main`, its hook) if it
    /// isn't there.
    pub fn new(root: PathBuf) -> anyhow::Result<Git> {
        use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&root)?;
        let repo = root.join(REPO);
        if !repo.join("HEAD").exists() {
            let ok = std::process::Command::new("git")
                .args(["init", "--bare", "--quiet", "--initial-branch=main"])
                .arg(&repo)
                .status()
                .map_err(|e| anyhow::anyhow!("git init: {e} (is git installed?)"))?;
            if !ok.success() {
                anyhow::bail!("git init {} failed", repo.display());
            }
        }
        git(&["config", "http.receivepack", "true"], &repo)?;
        let hook = repo.join("hooks").join("pre-receive");
        std::fs::create_dir_all(repo.join("hooks"))?;
        std::fs::write(&hook, zapper_git::pre_receive_with_local_admin())?;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755))?;
        Ok(Git { root })
    }

    pub fn repo(&self) -> PathBuf {
        self.root.join(REPO)
    }
}

/// Answers a session's request, as the pusher `who`.
async fn serve(root: PathBuf, who: String, req: hyper::Request<hyper::body::Incoming>) -> Response {
    let text = |status: StatusCode, msg: &str| {
        Response::builder()
            .status(status)
            .header("content-type", "text/plain")
            .body(Body::from(format!("{msg}\n")))
            .unwrap()
    };
    let path = req.uri().path().to_string();
    if !path.starts_with(&format!("/{REPO}/")) || path.contains("/../") {
        return text(StatusCode::NOT_FOUND, &format!("only /{REPO}"));
    }
    let query = req.uri().query().unwrap_or("").to_string();
    let (method, headers) = (req.method().clone(), req.headers().clone());
    let body = match http_body_util::Limited::new(req.into_body(), MAX_BODY)
        .collect()
        .await
    {
        Ok(b) => b.to_bytes(),
        Err(_) => return text(StatusCode::PAYLOAD_TOO_LARGE, "push too large"),
    };
    let (resp, ok) = zapper_git::http_backend(zapper_git::Request {
        root: &root,
        user: &who,
        email: &format!("{who}@workbench.zapper"),
        level: "write",
        method: method.as_str(),
        path: &path,
        query: &query,
        headers: &headers,
        body,
    })
    .await;
    if path.ends_with("/git-receive-pack") {
        tracing::info!(session = %who, pushed = ok, "git");
    }
    resp.map(Body::new)
}

fn owner(g: &Grant) -> Result<(), Error> {
    if *g == Grant::owner() {
        Ok(())
    } else {
        Err(Error(
            StatusCode::FORBIDDEN,
            "only the owner's token reaches the repository".into(),
        ))
    }
}

#[derive(Clone)]
struct Repo(PathBuf);

/// The repository as the user sees it: where to clone it from, and its
/// branches with their last commits (a session's proposals among them).
async fn overview(
    Extension(g): Extension<Grant>,
    Extension(r): Extension<Repo>,
) -> Result<Json<Value>, Error> {
    owner(&g)?;
    let out = git(
        &["for-each-ref", "--format=%(refname:short)%09%(objectname:short)%09%(committername)%09%(committerdate:unix)%09%(subject)", "refs/heads/"],
        &r.0,
    )?;
    let branches: Vec<Value> = out
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.splitn(5, '\t').collect();
            (f.len() == 5).then(|| {
                json!({ "name": f[0], "commit": f[1], "by": f[2], "when": f[3].parse::<i64>().unwrap_or(0), "subject": f[4] })
            })
        })
        .collect();
    Ok(Json(json!({
        "clone": format!("git clone {}", r.0.display()),
        "guests": format!("https://{NAME}/{REPO}"),
        "branches": branches,
    })))
}

/// Adds the usual rules to zapper's global layer, if they aren't there.
async fn add_rules(
    axum::extract::State(ctx): axum::extract::State<Ctx>,
    Extension(g): Extension<Grant>,
) -> Result<Json<Value>, Error> {
    owner(&g)?;
    let added = crate::facility::add_global_rules(&ctx, &usual_rules()).await?;
    Ok(Json(json!({ "added": added, "saved": false })))
}

const PANEL: &str = include_str!("git.js");

impl Facility for Git {
    fn name(&self) -> &'static str {
        "git"
    }

    fn routes(&self) -> Router<Ctx> {
        Router::new()
            .route("/", get(overview))
            .route("/rules", post(add_rules))
            .route(
                "/rules/usual",
                get(|| async { Json(Value::Array(usual_rules().to_vec())) }),
            )
            .layer(Extension(Repo(self.repo())))
    }

    fn panel(&self) -> Option<Panel> {
        Some(Panel {
            title: "Repository",
            script: PANEL,
        })
    }

    fn services(&self) -> Vec<Value> {
        vec![json!({
            "name": NAME,
            "port": 443,
            "sessions": { crate::gateway::LABEL.0: crate::gateway::LABEL.1 },
            "to": { "socket": SOCKET, "tls": true },
        })]
    }

    fn start(&self, ctx: &Ctx) {
        let root = self.root.clone();
        crate::facility::serve_socket(ctx, SOCKET, move |who, req| {
            serve(root.clone(), who.session, req)
        });
    }
}
