//! The workbench's repository: `config.git`, bare, under the workbench's
//! data. The user clones it and pushes to it on the host, `main` included;
//! sandboxes reach it at `/git/config.git/` on the workbench's edge, where
//! `git http-backend` (`zapper-git`, as the office's git server) answers
//! with the sandbox as the pusher at the `write` level, so its `pre-receive`
//! hook lets them push branches and never `main`, `main/…` or tags. An
//! agent proposes a change on a branch; the user merges it, and
//! `zapper-workbench apply` makes `main` the gateway's templates.

use std::path::{Path, PathBuf};

use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use zapper_openshell::edge::{plain, Body};

/// The repository's name, under the root.
pub const REPO: &str = "config.git";
/// Largest push taken.
const MAX_BODY: usize = 512 << 20;

/// Runs `git` on the bare repository `dir`.
pub fn git(dir: &Path, args: &[&str]) -> anyhow::Result<String> {
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
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[derive(Clone)]
pub struct Repo {
    root: PathBuf,
}

impl Repo {
    /// The repository under `root`, made (bare, on `main`, with its hook)
    /// if it isn't there.
    pub fn new(root: PathBuf) -> anyhow::Result<Repo> {
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
        git(&repo, &["config", "http.receivepack", "true"])?;
        let hook = repo.join("hooks").join("pre-receive");
        std::fs::create_dir_all(repo.join("hooks"))?;
        std::fs::write(&hook, zapper_git::pre_receive_with_local_admin())?;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755))?;
        Ok(Repo { root })
    }

    /// The bare repository.
    pub fn path(&self) -> PathBuf {
        self.root.join(REPO)
    }

    /// Answers sandbox `who`'s git request (its path without `/git`).
    pub async fn serve(&self, who: &str, req: Request<Incoming>) -> Response<Body> {
        let path = req.uri().path().to_string();
        if !path.starts_with(&format!("/{REPO}/")) || path.contains("/../") {
            return plain(StatusCode::NOT_FOUND, &format!("only /git/{REPO}"));
        }
        let query = req.uri().query().unwrap_or("").to_string();
        let (method, headers) = (req.method().clone(), req.headers().clone());
        let body: Bytes = match http_body_util::Limited::new(req.into_body(), MAX_BODY)
            .collect()
            .await
        {
            Ok(b) => b.to_bytes(),
            Err(_) => return plain(StatusCode::PAYLOAD_TOO_LARGE, "push too large"),
        };
        let (resp, ok) = zapper_git::http_backend(zapper_git::Request {
            root: &self.root,
            user: who,
            email: &format!("{who}@workbench.sandbox"),
            level: "write",
            method: method.as_str(),
            path: &path,
            query: &query,
            headers: &headers,
            body,
        })
        .await;
        if path.ends_with("/git-receive-pack") {
            tracing::info!(sandbox = %who, pushed = ok, "git");
        }
        resp.map(|b| b.map_err(|n| match n {}).boxed())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_repository_is_made_bare_on_main_with_its_hook() {
        let root = std::env::temp_dir().join(format!("zapper-wb-repo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let r = Repo::new(root.clone()).unwrap();
        assert!(r.path().join("HEAD").is_file());
        assert_eq!(
            git(&r.path(), &["symbolic-ref", "HEAD"]).unwrap(),
            "refs/heads/main"
        );
        assert!(r.path().join("hooks/pre-receive").is_file());
        // Made again, it is left as it is.
        Repo::new(root.clone()).unwrap();
        let _ = std::fs::remove_dir_all(root);
    }
}
