//! Applying the repository to zapper: a commit checked out, rendered as
//! zapper's whole config (`render`), checked by zapper without being
//! applied (`PUT /v1/config?dry_run=true`, which says what would change),
//! then put in place and saved; and the images whose recipe or files
//! changed since they were last built, built again, one at a time.
//!
//! Only `main` is applied, and only when asked: changes come on branches,
//! which can be checked first. The live commit is kept beside the
//! repository (`state.json`), with each image's hash as last built.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use axum::http::Method;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::gateway::{Ctx, Live};
use crate::render;

/// A commit applied.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Applied {
    pub commit: String,
    pub subject: String,
    pub applied_at: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    live: Option<Applied>,
    /// Each image's hash when it was last built (or found built).
    images: BTreeMap<String, String>,
}

/// How long a commit's id is, as sessions are labelled with it.
const SHORT: usize = 12;

pub struct Applier {
    repo: PathBuf,
    live_dir: PathBuf,
    secrets: PathBuf,
    daemon: Option<Value>,
    state_path: PathBuf,
    state: Mutex<State>,
    builds: Mutex<BTreeMap<String, Value>>,
    applying: tokio::sync::Mutex<()>,
    live: Live,
}

fn git(repo: &Path, args: &[&str]) -> Result<String> {
    let out = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(repo)
        .args(args)
        .output()
        .map_err(|e| anyhow!("git: {e} (is git installed?)"))?;
    if !out.status.success() {
        bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// A ref a user may name: a branch or a commit, nothing git would take as
/// an option or a range.
fn checked_ref(r: &str) -> Result<&str> {
    let ok = !r.is_empty()
        && r.len() <= 200
        && !r.starts_with('-')
        && !r.contains("..")
        && r.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._/-".contains(&b));
    if !ok {
        bail!("{r:?} isn't a branch or a commit");
    }
    Ok(r)
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

impl Applier {
    /// Applies `repo` (bare) with checkouts under `live`, the secrets
    /// facility's files under `secrets`, zapper's daemon section `daemon`
    /// (else the running one); its state in `state`, and the live commit
    /// told to `live`.
    pub fn new(
        repo: PathBuf,
        live_dir: PathBuf,
        secrets: PathBuf,
        daemon: Option<Value>,
        state_path: PathBuf,
        live: Live,
    ) -> Result<Applier> {
        let state: State = match std::fs::read(&state_path) {
            Ok(b) => serde_json::from_slice(&b).context("state.json")?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::default(),
            Err(e) => return Err(e.into()),
        };
        live.set(
            state
                .live
                .as_ref()
                .map(|a| a.commit[..SHORT.min(a.commit.len())].to_string()),
        );
        Ok(Applier {
            repo,
            live_dir,
            secrets,
            daemon,
            state_path,
            state: Mutex::new(state),
            builds: Mutex::new(BTreeMap::new()),
            applying: tokio::sync::Mutex::new(()),
            live,
        })
    }

    fn save_state(&self) -> Result<()> {
        let s = serde_json::to_vec_pretty(&*self.state.lock().unwrap())?;
        let tmp = self.state_path.with_extension("tmp");
        std::fs::write(&tmp, s)?;
        std::fs::rename(tmp, &self.state_path)?;
        Ok(())
    }

    /// The commit `r` names, and its subject.
    fn resolve(&self, r: &str) -> Result<(String, String)> {
        let r = checked_ref(r)?;
        let commit = git(
            &self.repo,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{r}^{{commit}}"),
            ],
        )
        .map_err(|_| anyhow!("no commit {r} in the repository"))?;
        let subject = git(&self.repo, &["log", "-1", "--format=%s", &commit])?;
        Ok((commit, subject))
    }

    /// `commit`'s files, checked out once under `dir` (`dir/repo`).
    fn checkout(&self, commit: &str, dir: &Path) -> Result<PathBuf> {
        let repo = dir.join("repo");
        if repo.exists() {
            return Ok(repo);
        }
        let partial = dir.join(".repo");
        let _ = std::fs::remove_dir_all(&partial);
        std::fs::create_dir_all(&partial)?;
        let archive = std::process::Command::new("git")
            .arg("--git-dir")
            .arg(&self.repo)
            .args(["archive", "--format=tar", commit])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .context("git archive")?;
        let ok = std::process::Command::new("tar")
            .args(["-x", "-C"])
            .arg(&partial)
            .stdin(archive.stdout.unwrap())
            .status()
            .context("tar")?;
        if !ok.success() {
            bail!("checking out {commit} failed");
        }
        std::fs::rename(&partial, &repo)?;
        Ok(repo)
    }

    /// Renders `commit` checked out in `dir`, as zapper's whole config: its
    /// daemon section the workbench's, else the running one as it is; and
    /// the office's `firms`, which aren't the repository's, kept as they run.
    async fn render(&self, ctx: &Ctx, commit: &str, dir: &Path) -> Result<render::Rendered> {
        let repo = self.checkout(commit, dir)?;
        let running = ctx
            .zapper
            .call(Method::GET, "/v1/config?format=json", None)
            .await?
            .ok()
            .map_err(|(_, why)| anyhow!("zapper's config: {why}"))?;
        let daemon = self
            .daemon
            .clone()
            .unwrap_or_else(|| running["daemon"].clone());
        let mut rendered = render::render(&repo, dir, &self.secrets, daemon)?;
        if let Some(firms) = running
            .get("firms")
            .filter(|f| f.as_object().is_some_and(|m| !m.is_empty()))
        {
            rendered.config["firms"] = firms.clone();
        }
        Ok(rendered)
    }

    /// What applying `r` (a branch or a commit) would change, checked by
    /// zapper as a replacement and not applied: its diff, or why it can't be.
    pub async fn check(&self, ctx: &Ctx, r: &str) -> Result<Value> {
        let (commit, subject) = self.resolve(r)?;
        let dir = self.live_dir.join(format!(".check-{commit}"));
        let rendered = self.render(ctx, &commit, &dir).await;
        let _ = std::fs::remove_dir_all(&dir);
        let rendered = rendered?;
        let diff = dry_run(ctx, &rendered.config).await?;
        Ok(
            json!({ "commit": commit, "subject": subject, "diff": diff, "images": self.changed(ctx, &rendered).await? }),
        )
    }

    /// Applies `main`: rendered, checked, put in place and saved; then the
    /// images it changed built again, in the background.
    pub async fn apply(self: &Arc<Self>, ctx: &Ctx) -> Result<Value> {
        let _one = self.applying.lock().await;
        let (commit, subject) = self.resolve("main")?;
        let dir = self.live_dir.join(&commit);
        let rendered = self.render(ctx, &commit, &dir).await?;
        let diff = dry_run(ctx, &rendered.config).await?;
        let put = ctx
            .zapper
            .call(Method::PUT, "/v1/config", Some(&rendered.config))
            .await?;
        put.ok()
            .map_err(|(_, why)| anyhow!("zapper refused the config: {why}"))?;
        ctx.zapper
            .call(Method::POST, "/v1/config/save", None)
            .await?
            .ok()
            .map_err(|(_, why)| anyhow!("applied, but not saved: {why}"))?;
        let changed = self.changed(ctx, &rendered).await?;
        {
            let mut st = self.state.lock().unwrap();
            st.live = Some(Applied {
                commit: commit.clone(),
                subject: subject.clone(),
                applied_at: now(),
            });
        }
        self.save_state()?;
        self.live.set(Some(commit[..SHORT].to_string()));
        self.prune(&commit);
        if !changed.is_empty() {
            let (me, ctx, hashes) = (self.clone(), ctx.clone(), rendered.images.clone());
            let names = changed.clone();
            tokio::spawn(async move { me.build(&ctx, names, hashes).await });
        }
        Ok(json!({ "commit": commit, "subject": subject, "diff": diff, "building": changed }))
    }

    /// The images to build again: those whose hash isn't the one last
    /// built. One never recorded is built only if zapper doesn't have it.
    async fn changed(&self, ctx: &Ctx, rendered: &render::Rendered) -> Result<Vec<String>> {
        let have = ctx.zapper.call(Method::GET, "/v1/images", None).await?.body;
        let built: Vec<String> = have["images"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|i| i["name"].as_str().map(str::to_string))
            .collect();
        let mut st = self.state.lock().unwrap();
        let mut out = vec![];
        for (name, hash) in &rendered.images {
            match st.images.get(name) {
                Some(h) if h == hash => {}
                Some(_) => out.push(name.clone()),
                None if built.contains(name) => {
                    st.images.insert(name.clone(), hash.clone());
                }
                None => out.push(name.clone()),
            }
        }
        Ok(out)
    }

    /// Builds `names` one at a time through zapper, each recorded as built
    /// at its hash once it is.
    async fn build(&self, ctx: &Ctx, names: Vec<String>, hashes: BTreeMap<String, String>) {
        for n in &names {
            self.builds
                .lock()
                .unwrap()
                .insert(n.clone(), json!({ "state": "queued" }));
        }
        for name in names {
            let say = |v: Value| {
                self.builds.lock().unwrap().insert(name.clone(), v);
            };
            // One build at a time in zapper: wait for another to end.
            let id = loop {
                match ctx
                    .zapper
                    .call(
                        Method::POST,
                        &format!("/v1/images/{name}/build"),
                        Some(&json!({})),
                    )
                    .await
                {
                    Ok(a) if a.status.as_u16() == 409 => {
                        tokio::time::sleep(Duration::from_secs(3)).await
                    }
                    Ok(a) if a.status.is_success() => break a.body["id"].clone(),
                    Ok(a) => {
                        say(json!({ "state": "failed", "detail": a.body["error"] }));
                        break Value::Null;
                    }
                    Err(e) => {
                        say(json!({ "state": "failed", "detail": format!("{e:#}") }));
                        break Value::Null;
                    }
                }
            };
            if id.is_null() {
                continue;
            }
            say(json!({ "state": "building", "build": id }));
            let outcome = loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                let Ok(a) = ctx.zapper.call(Method::GET, "/v1/images", None).await else {
                    continue;
                };
                let b = a.body["builds"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|b| b["id"] == id)
                    .cloned();
                match b {
                    Some(b) if !b["outcome"].is_null() => break b["outcome"].clone(),
                    Some(_) => {}
                    None => break json!({ "failed": "zapper no longer lists the build" }),
                }
            };
            if outcome.get("failed").is_some() {
                say(json!({ "state": "failed", "build": id, "detail": outcome["failed"] }));
            } else {
                say(json!({ "state": "built", "build": id }));
                if let Some(h) = hashes.get(&name) {
                    self.state
                        .lock()
                        .unwrap()
                        .images
                        .insert(name.clone(), h.clone());
                    let _ = self.save_state();
                }
            }
        }
    }

    /// Keeps the live commit's checkout and the one before it: sessions copy
    /// what they mount when they boot, so no running one needs another.
    fn prune(&self, live: &str) {
        let Ok(entries) = std::fs::read_dir(&self.live_dir) else {
            return;
        };
        let mut dirs: Vec<(std::time::SystemTime, PathBuf)> = entries
            .flatten()
            .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
            .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
            .collect();
        dirs.sort();
        let keep = 2;
        let n = dirs.len();
        for (_, d) in dirs.into_iter().take(n.saturating_sub(keep)) {
            if !d.ends_with(live) {
                let _ = std::fs::remove_dir_all(d);
            }
        }
    }

    /// What is applied, and the sessions an apply has left behind: made
    /// from another commit (their `workbench-config` label), not restarted.
    pub async fn status(&self, ctx: &Ctx) -> Value {
        let live = self.state.lock().unwrap().live.clone();
        let main = self.resolve("main").ok();
        let short = live
            .as_ref()
            .map(|a| a.commit[..SHORT.min(a.commit.len())].to_string());
        let mut behind = vec![];
        if let (Some(short), Ok(a)) = (
            &short,
            ctx.zapper.call(Method::GET, "/v1/sessions", None).await,
        ) {
            for s in a.body.as_array().into_iter().flatten() {
                if let Some(c) = s["labels"][crate::gateway::CONFIG_LABEL].as_str() {
                    if c != short {
                        behind.push(json!({ "id": s["id"], "profile": s["profile"], "commit": c }));
                    }
                }
            }
        }
        json!({
            "live": live,
            "main": main.map(|(c, s)| json!({ "commit": c, "subject": s })),
            "builds": *self.builds.lock().unwrap(),
            "out_of_date": behind,
        })
    }
}

/// zapper's check of `config` as a replacement: what it would change.
async fn dry_run(ctx: &Ctx, config: &Value) -> Result<String> {
    let a = ctx
        .zapper
        .call(Method::PUT, "/v1/config?dry_run=true", Some(config))
        .await?;
    let v = a
        .ok()
        .map_err(|(_, why)| anyhow!("zapper refuses it: {why}"))?;
    Ok(v["diff"].as_str().unwrap_or_default().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ref_is_a_branch_or_a_commit_not_an_option() {
        for ok in ["main", "proposal/one", "3f1a2b", "v1.2"] {
            assert!(checked_ref(ok).is_ok(), "{ok}");
        }
        for bad in ["", "--upload-pack=x", "main..other", "a b", "x;y"] {
            assert!(checked_ref(bad).is_err(), "{bad}");
        }
    }
}
