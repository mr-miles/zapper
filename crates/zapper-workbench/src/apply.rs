//! Applying the repository to the gateway: a commit of `main` checked out
//! and rendered as zapper's profiles and image recipes (`render`); each
//! image whose recipe or files changed built again (`zapper-openshell`'s
//! image builder); and each profile made a sandbox template, `wb-<profile>`:
//! its image, CPU and memory, its environment (the profile's and its
//! harness's, and `ZAPPER_WORKBENCH_PROFILE`), its disk size and files
//! (the harness's files, the skills and directories it mounted: staged under
//! the driver's files root for the driver to copy into each sandbox made
//! from it, before its workload starts), and labels naming the profile and
//! the commit it came from (`zapper-profile`, `zapper-commit`). Its setup
//! commands run in the sandboxes `zapper-workbench new` makes
//! (`provision`).
//!
//! Only what is asked for is applied (`zapper-workbench apply`, `main` by
//! default); `check` renders a branch without building or applying. The
//! live commit is kept beside the repository (`state.json`), with each
//! image's hash as last built.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use zapper_openshell::{Labels, OpenShell, Setup, Template};
use zapper_policy::{Config, ResolvedProfile};

use crate::render;
use crate::repo::git;

/// A commit applied.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Applied {
    pub commit: String,
    pub subject: String,
    pub applied_at: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct State {
    live: Option<Applied>,
    /// Each image's hash when it was last built.
    images: BTreeMap<String, String>,
}

/// How long a commit's id is in the labels.
const SHORT: usize = 12;
/// The prefix of the templates the workbench makes.
pub const TEMPLATE_PREFIX: &str = "wb-";
/// Where the workbench stages its templates' files, under the driver's files
/// root.
const STAGED: &str = "workbench";

/// The template a profile becomes.
pub fn template_name(profile: &str) -> String {
    format!("{TEMPLATE_PREFIX}{profile}")
}

/// The labels of profile `profile`'s template, made from `commit`, and of
/// the sandboxes the workbench makes from it: so the gateway's operator sees
/// which definition a sandbox came from.
pub fn labels(profile: &str, commit: &str) -> Labels {
    Labels::from([
        ("zapper-workbench".to_string(), "1".to_string()),
        (
            "zapper-profile".to_string(),
            zapper_openshell::label_value(profile),
        ),
        (
            "zapper-commit".to_string(),
            commit[..SHORT.min(commit.len())].to_string(),
        ),
    ])
}

/// The OCI image the workbench builds recipe `name` as, unless
/// `openshell.yaml`'s `images:` names one (then that, built by whoever set it):
/// its own tag, apart from the office's.
pub fn image_of(os: &OpenShell, name: &str) -> (String, bool) {
    match os.image_override(name) {
        Some(i) => (i, false),
        None => (format!("localhost/zapper-workbench-{name}"), true),
    }
}

/// The account the profiles on image `name` run as (its uid 1000).
fn image_user(config: &Config, name: &str) -> String {
    config
        .profiles
        .keys()
        .filter_map(|p| config.resolve(p).ok())
        .find(|p| p.engine.image == name)
        .map(|p| p.agent.user.clone())
        .unwrap_or_else(|| "agent".into())
}

/// What an image is built from: its recipe and files (`hash`) and its user.
fn image_key(config: &Config, name: &str, hash: &str) -> String {
    format!("{hash} {}", image_user(config, name))
}

/// The recipe arguments the workbench can't give (it gives none): those
/// without a default.
fn check_args(config: &Config, name: &str) -> Result<()> {
    let Some(r) = config.images.get(name) else {
        bail!("no recipe for image {name}");
    };
    let missing: Vec<&String> = r
        .args
        .iter()
        .filter(|(_, a)| a.default.is_none())
        .map(|(k, _)| k)
        .collect();
    if !missing.is_empty() {
        bail!(
            "image {name}: give its arguments defaults in the repository ({}): the workbench builds it with none",
            missing.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
        );
    }
    Ok(())
}

/// Profile `p`'s setup, as its sandboxes get it: `ZAPPER_WORKBENCH_PROFILE`
/// in its environment, so what runs in one knows which it is.
pub fn setup(p: &ResolvedProfile) -> Result<Setup> {
    let extra = BTreeMap::from([("ZAPPER_WORKBENCH_PROFILE".to_string(), p.name.clone())]);
    Setup::of(p, extra)
}

pub struct Applier {
    repo: PathBuf,
    data: PathBuf,
    /// The live commit's config, once rendered.
    live: Mutex<Option<(String, Config)>>,
}

impl Applier {
    /// Applies the bare repository `repo`, with checkouts and state under
    /// `data`.
    pub fn new(repo: PathBuf, data: PathBuf) -> Applier {
        Applier {
            repo,
            data,
            live: Mutex::default(),
        }
    }
    fn state_path(&self) -> PathBuf {
        self.data.join("state.json")
    }

    fn state(&self) -> Result<State> {
        match std::fs::read(self.state_path()) {
            Ok(b) => serde_json::from_slice(&b).context("state.json"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
            Err(e) => Err(e.into()),
        }
    }

    fn save(&self, s: &State) -> Result<()> {
        let tmp = self.state_path().with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(s)?)?;
        std::fs::rename(tmp, self.state_path())?;
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
            .stdin(archive.stdout.context("git archive's output")?)
            .status()
            .context("tar")?;
        if !ok.success() {
            bail!("checking out {commit} failed");
        }
        std::fs::rename(&partial, &repo)?;
        Ok(repo)
    }

    /// `commit` rendered under `dir`, as a zapper config.
    fn render(&self, commit: &str, dir: &Path) -> Result<(Config, BTreeMap<String, String>)> {
        let repo = self.checkout(commit, dir)?;
        let r = render::render(&repo, dir)?;
        let text = serde_yaml::to_string(&r.config)?;
        let config = Config::from_yaml(&text).context("the rendered config")?;
        Ok((config, r.images))
    }

    /// What applying `r` would do: the images it would build and the
    /// templates it would make, or why it can't be applied.
    pub fn check(&self, r: &str) -> Result<Value> {
        let (commit, subject) = self.resolve(r)?;
        let dir = self.data.join(format!(".check-{commit}"));
        let rendered = self.render(&commit, &dir);
        let _ = std::fs::remove_dir_all(&dir);
        let (config, images) = rendered?;
        let state = self.state()?;
        let changed: Vec<&String> = images
            .iter()
            .filter(|(n, h)| state.images.get(*n) != Some(&image_key(&config, n, h)))
            .map(|(n, _)| n)
            .collect();
        for n in &changed {
            check_args(&config, n)?;
        }
        let profiles: Vec<String> = config.profiles.keys().map(|p| template_name(p)).collect();
        let ignored = zapper_openshell::rules::ignored(&config);
        Ok(
            json!({ "commit": commit, "subject": subject, "images": changed, "templates": profiles, "ignored": ignored }),
        )
    }

    /// Applies `r` (`main` unless said): images built where they changed,
    /// every profile's template made again.
    pub async fn apply(&self, os: &OpenShell, r: &str, log: &dyn Fn(&str)) -> Result<Value> {
        let (commit, subject) = self.resolve(r)?;
        let dir = self.data.join("live").join(&commit);
        let (config, images) = self.render(&commit, &dir)?;
        let mut state = self.state()?;
        for w in zapper_openshell::rules::ignored(&config) {
            log(&format!("ignored: {w}"));
        }
        let mut built = Vec::new();
        for (name, hash) in &images {
            let key = image_key(&config, name, hash);
            let (tag, ours) = image_of(os, name);
            if !ours || state.images.get(name) == Some(&key) {
                continue;
            }
            check_args(&config, name)?;
            let b = zapper_openshell::image::Build {
                name: name.clone(),
                args: vec![],
                user: image_user(&config, name),
                // Its sandboxes don't reach the office.
                agent: None,
                tag: Some(tag),
            };
            zapper_openshell::image::build(&config, &b, log)?;
            state.images.insert(name.clone(), key);
            built.push(name.clone());
        }
        let mut made = Vec::new();
        for name in config.profiles.keys() {
            let p = config.resolve(name)?;
            let s = setup(&p)?;
            // Its files and directories, staged for the driver to copy into
            // each sandbox made from it before the workload starts.
            let driver = os.driver_config(&p, &s, &format!("{STAGED}/{name}"), None)?;
            let t = Template {
                name: template_name(name),
                image: image_of(os, &p.engine.image).0,
                vcpus: p.engine.vcpus,
                memory_mib: p.engine.memory_mib,
                labels: labels(name, &commit),
                env: s.env,
                driver_config: Some(driver),
            };
            os.put_template(&t).await?;
            made.push(t.name);
        }
        state.live = Some(Applied {
            commit: commit.clone(),
            subject: subject.clone(),
            applied_at: zapper_util::now() as u64,
        });
        self.save(&state)?;
        *self.live.lock().unwrap() = Some((commit.clone(), config));
        // The checkout before the live one is kept, older ones go.
        prune(&self.data.join("live"), &commit);
        Ok(json!({ "commit": commit, "subject": subject, "built": built, "templates": made }))
    }

    /// The live commit's profile `name`, resolved; an error if nothing was
    /// applied or it has no such profile.
    pub fn live_profile(&self, name: &str) -> Result<(String, ResolvedProfile)> {
        let commit = self
            .state()?
            .live
            .ok_or_else(|| anyhow!("nothing applied yet (zapper-workbench apply)"))?
            .commit;
        let mut live = self.live.lock().unwrap();
        if live.as_ref().map(|(c, _)| c) != Some(&commit) {
            let (config, _) = self.render(&commit, &self.data.join("live").join(&commit))?;
            *live = Some((commit.clone(), config));
        }
        let (_, config) = live.as_ref().expect("rendered above");
        let p = config
            .resolve(name)
            .with_context(|| format!("profile {name} at {commit}"))?;
        Ok((commit, p))
    }

    /// The live commit, if any was applied.
    pub fn status(&self) -> Result<Value> {
        let s = self.state()?;
        Ok(json!({ "live": s.live, "images": s.images }))
    }
}
/// A ref the user may name: a branch or a commit, nothing git would read
/// as an option or a range.
fn checked_ref(r: &str) -> Result<&str> {
    let ok = !r.is_empty()
        && !r.starts_with('-')
        && !r.contains("..")
        && r.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./".contains(&b));
    if !ok {
        bail!("{r:?} isn't a branch or a commit");
    }
    Ok(r)
}

/// Removes the checkouts under `live` but `keep` and the newest other.
fn prune(live: &Path, keep: &str) {
    let Ok(rd) = std::fs::read_dir(live) else {
        return;
    };
    let mut others: Vec<(std::time::SystemTime, PathBuf)> = rd
        .flatten()
        .filter(|e| e.file_name() != keep)
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    others.sort();
    others.pop();
    for (_, p) in others {
        let _ = std::fs::remove_dir_all(p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refs_are_branches_or_commits() {
        for ok in ["main", "feat/x", "a1b2c3"] {
            assert!(checked_ref(ok).is_ok(), "{ok}");
        }
        for bad in ["", "-x", "a..b", "a b", "a;b"] {
            assert!(checked_ref(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_profiles_template_names_it_and_its_sandboxes_get_the_rest() {
        let c = Config::from_yaml(
            "profiles:\n  dev:\n    agent:\n      kind: claude-code\n      instructions: be careful\n      env: { MODE: a, TOKEN: \"{{secret:x}}\" }\n    setup: [\"echo hi\"]\n",
        )
        .unwrap();
        let p = c.resolve("dev").unwrap();
        let l = labels("dev", "0123456789abcdef0123");
        assert_eq!(l["zapper-workbench"], "1");
        assert_eq!(l["zapper-profile"], "dev");
        assert_eq!(l["zapper-commit"], "0123456789ab");
        let s = setup(&p).unwrap();
        assert_eq!(s.env["MODE"], "a");
        assert_eq!(s.env["ZAPPER_WORKBENCH_PROFILE"], "dev");
        assert!(!s.env.contains_key("TOKEN"), "secrets are the gateway's");
        assert_eq!(s.left_out, ["agent.env.TOKEN"]);
        assert_eq!(s.commands, ["echo hi"]);
        let home = format!("/home/{}/", p.agent.user);
        assert!(
            s.files.iter().any(|f| f.path.starts_with(&home)),
            "the harness's files under the home"
        );
        assert_eq!(template_name("dev"), "wb-dev");
    }
}
