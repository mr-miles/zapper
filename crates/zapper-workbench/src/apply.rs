//! Applying the repository to the gateway: a commit of `main` checked out
//! and rendered as zapper's profiles and image recipes (`render`); each
//! image whose recipe or files changed built again (`zapper-openshell`'s
//! image builder); and each profile made a sandbox template, `wb-<profile>`,
//! whose image is its base with the profile in it: the harness's files and
//! environment (instructions, settings, MCP servers, the turn-end hook), the
//! skills and directories it mounted, and its setup commands, as the user's
//! account. A sandbox made from the template, in OpenShell's CLI or UI, is
//! the profile's from its first command.
//!
//! Only what is asked for is applied (`zapper-workbench apply`, `main` by
//! default); `check` renders a branch without building or applying. The
//! live commit is kept beside the repository (`state.json`), with each
//! image's hash as last built and each profile's as last made.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use zapper_openshell::OpenShell;
use zapper_policy::{secret_refs, Config, ResolvedProfile};

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
struct State {
    live: Option<Applied>,
    /// Each image's hash when it was last built.
    images: BTreeMap<String, String>,
    /// Each profile's, as its template was last made.
    profiles: BTreeMap<String, String>,
}

/// How long a commit's id is in the labels.
const SHORT: usize = 12;

/// The template a profile becomes.
pub fn template_name(profile: &str) -> String {
    format!("wb-{profile}")
}

/// The image a profile's template runs.
pub fn profile_image(profile: &str) -> String {
    format!("localhost/zapper-workbench-{profile}")
}

pub struct Applier {
    repo: PathBuf,
    data: PathBuf,
    /// The static guest binary the base images install as `office`.
    agent: PathBuf,
}

impl Applier {
    /// Applies the bare repository `repo`, with checkouts and state under
    /// `data`.
    pub fn new(repo: PathBuf, data: PathBuf, agent: PathBuf) -> Applier {
        Applier { repo, data, agent }
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
            .filter(|(n, h)| state.images.get(*n) != Some(h))
            .map(|(n, _)| n)
            .collect();
        let profiles: Vec<String> = config.profiles.keys().map(|p| template_name(p)).collect();
        Ok(
            json!({ "commit": commit, "subject": subject, "images": changed, "templates": profiles }),
        )
    }

    /// Applies `r` (`main` unless said): images built where they changed,
    /// every profile's template made from its image with the profile in it.
    pub async fn apply(&self, os: &OpenShell, r: &str, log: &dyn Fn(&str)) -> Result<Value> {
        let (commit, subject) = self.resolve(r)?;
        let dir = self.data.join("live").join(&commit);
        let (config, images) = self.render(&commit, &dir)?;
        let mut state = self.state()?;
        let mut built = Vec::new();
        for (name, hash) in &images {
            if state.images.get(name) == Some(hash) {
                continue;
            }
            // The account commands run as: the profiles' on this image.
            let user = config
                .profiles
                .keys()
                .filter_map(|p| config.resolve(p).ok())
                .find(|p| &p.engine.image == name)
                .map(|p| p.agent.user.clone())
                .unwrap_or_else(|| "agent".into());
            let b = zapper_openshell::image::Build {
                name: name.clone(),
                args: vec![],
                user,
                agent: self.agent.clone(),
            };
            zapper_openshell::image::build(&config, &b, log)?;
            state.images.insert(name.clone(), hash.clone());
            built.push(name.clone());
        }
        let short = commit[..SHORT.min(commit.len())].to_string();
        let mut made = Vec::new();
        for name in config.profiles.keys() {
            let p = config.resolve(name)?;
            let tag = profile_image(name);
            let base = os.image_of(&p);
            let layer = profile_layer(&p, &base, &dir.join("profiles").join(name))?;
            let fingerprint = format!("{base} {}", sha(&layer));
            if state.profiles.get(name) != Some(&fingerprint) || built.contains(&p.engine.image) {
                build_layer(&dir.join("profiles").join(name), &tag, log)?;
            }
            let labels = BTreeMap::from([
                ("workbench".to_string(), "1".to_string()),
                ("workbench-config".to_string(), short.clone()),
            ]);
            os.put_template(
                &template_name(name),
                &tag,
                p.engine.vcpus,
                p.engine.memory_mib,
                &labels,
            )
            .await?;
            state.profiles.insert(name.clone(), fingerprint);
            made.push(template_name(name));
        }
        state.live = Some(Applied {
            commit: commit.clone(),
            subject: subject.clone(),
            applied_at: zapper_util::now() as u64,
        });
        self.save(&state)?;
        // The checkout before the live one is kept, older ones go.
        prune(&self.data.join("live"), &commit);
        Ok(json!({ "commit": commit, "subject": subject, "built": built, "templates": made }))
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

/// Writes the build context of profile `p`'s image into `ctx`: `root/`, the
/// files and directories it puts in the image by their paths, and the
/// Containerfile, `FROM base`. Its Containerfile, to tell whether it changed.
fn profile_layer(p: &ResolvedProfile, base: &str, ctx: &Path) -> Result<String> {
    let agent = &p.agent;
    let user = &agent.user;
    let home = format!("/home/{user}");
    let harness = zapper_driver::harness::get(&agent.kind).ok_or_else(|| {
        anyhow!(
            "profile {}: agent.kind {:?} isn't a harness",
            p.name,
            agent.kind
        )
    })?;
    let _ = std::fs::remove_dir_all(ctx);
    let root = ctx.join("root");
    std::fs::create_dir_all(&root)?;

    // The environment: the profile's (but secrets: the gateway's providers
    // hold credentials), then the harness's.
    let mut env = BTreeMap::new();
    for (k, v) in &agent.env {
        if secret_refs(v).is_empty() {
            env.insert(k.clone(), v.clone());
        }
    }
    let mut files = Vec::new();
    harness.guest(p, &home, &mut env, &mut files);
    let config_dir = harness.config_dir(&home);
    let named = agent.files.iter().map(|(k, c)| (k.clone(), c)).chain(
        agent
            .config_files
            .iter()
            .map(|(k, c)| (format!("{config_dir}/{}", k.trim_start_matches('/')), c)),
    );
    for (path, content) in named {
        let path = match path.strip_prefix("~/") {
            Some(rest) => format!("{home}/{rest}"),
            None => path,
        };
        if secret_refs(content).is_empty() {
            files.push(zapper_policy::proto::GuestFile {
                user_owned: path.starts_with(&home),
                path,
                content: content.clone(),
                mode: 0o644,
            });
        }
    }
    for f in &files {
        let dest = under(&root, &f.path)?;
        std::fs::create_dir_all(dest.parent().unwrap_or(&root))?;
        std::fs::write(&dest, &f.content)?;
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(f.mode))?;
    }
    // The directories it mounted (skills, config directories, the
    // profile's own), copied in; a session's own work directory is the
    // sandbox's.
    for m in p.mounts.iter().cloned().chain(harness.mounts(agent, &home)) {
        if m.host.contains("{workdir}") {
            continue;
        }
        let host = PathBuf::from(zapper_policy::expand_home(&m.host));
        copy_tree(&host, &under(&root, &m.guest)?)
            .with_context(|| format!("profile {}: copying {}", p.name, host.display()))?;
    }

    let mut file = format!("FROM {base}\nCOPY root/ /\n");
    file.push_str(&format!(
        "RUN mkdir -p {home} && chown -R {user}:{user} {home}\n"
    ));
    for (k, v) in &env {
        if valid_env_name(k) {
            file.push_str(&format!("ENV {k}=\"{}\"\n", env_quote(v)));
        }
    }
    if !p.setup.is_empty() {
        file.push_str(&format!("USER {user}\nWORKDIR {}\n", agent.workdir));
        for cmd in &p.setup {
            file.push_str(&format!(
                "RUN [\"bash\", \"-c\", {}]\n",
                serde_json::to_string(cmd)?
            ));
        }
    }
    file.push_str(&format!("WORKDIR {}\n", agent.workdir));
    std::fs::write(ctx.join("Containerfile"), &file)?;
    Ok(file)
}

/// Builds the profile image in `ctx` as `tag`.
fn build_layer(ctx: &Path, tag: &str, log: &dyn Fn(&str)) -> Result<()> {
    log(&format!("building {tag}"));
    let oci = zapper_openshell::image::engine();
    let out = std::process::Command::new(&oci)
        .args(["build", "-t", tag, "-f"])
        .arg(ctx.join("Containerfile"))
        .arg(ctx)
        .output()
        .with_context(|| format!("running {oci} build"))?;
    for line in String::from_utf8_lossy(&out.stdout)
        .lines()
        .chain(String::from_utf8_lossy(&out.stderr).lines())
    {
        log(line);
    }
    if !out.status.success() {
        bail!("{oci} build of {tag} failed");
    }
    Ok(())
}

/// `path`, an absolute path in the image, under `root`; none that leads out.
fn under(root: &Path, path: &str) -> Result<PathBuf> {
    let rel = path.trim_start_matches('/');
    if !path.starts_with('/') || rel.split('/').any(|c| c == ".." || c.is_empty()) {
        bail!("{path:?} isn't an absolute path in the image");
    }
    Ok(root.join(rel))
}

fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    if from.is_dir() {
        std::fs::create_dir_all(to)?;
        for e in std::fs::read_dir(from)? {
            let e = e?;
            if e.file_type()?.is_symlink() {
                continue;
            }
            copy_tree(&e.path(), &to.join(e.file_name()))?;
        }
    } else if from.is_file() {
        if let Some(d) = to.parent() {
            std::fs::create_dir_all(d)?;
        }
        std::fs::copy(from, to)?;
    }
    Ok(())
}

fn valid_env_name(k: &str) -> bool {
    !k.is_empty()
        && !k.starts_with(|c: char| c.is_ascii_digit())
        && k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// A value in a Containerfile's `ENV k="v"`.
fn env_quote(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('$', "\\$")
        .replace('\n', "\\n")
}

fn sha(s: &str) -> String {
    ring::digest::digest(&ring::digest::SHA256, s.as_bytes())
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
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
    fn a_profiles_image_has_its_harness_files_env_and_setup() {
        let c = Config::from_yaml(
            "profiles:\n  dev:\n    agent:\n      kind: claude-code\n      instructions: be careful\n      env: { MODE: \"a \\\"b\\\" $c\", TOKEN: \"{{secret:x}}\" }\n    setup: [\"echo hi\"]\n",
        )
        .unwrap();
        let p = c.resolve("dev").unwrap();
        let ctx = std::env::temp_dir().join(format!("zapper-wb-layer-{}", std::process::id()));
        let file = profile_layer(&p, "localhost/zapper-openshell-office", &ctx).unwrap();
        assert!(
            file.starts_with("FROM localhost/zapper-openshell-office\n"),
            "{file}"
        );
        assert!(file.contains("ENV MODE=\"a \\\"b\\\" \\$c\""), "{file}");
        assert!(!file.contains("TOKEN"), "secrets are the gateway's: {file}");
        assert!(
            file.contains("RUN [\"bash\", \"-c\", \"echo hi\"]"),
            "{file}"
        );
        let home = format!("home/{}", p.agent.user);
        let written: Vec<String> = walk(&ctx.join("root"));
        assert!(
            written.iter().any(|f| f.starts_with(&home)),
            "the harness's files under the home: {written:?}"
        );
        let _ = std::fs::remove_dir_all(ctx);
    }

    fn walk(dir: &Path) -> Vec<String> {
        let mut out = vec![];
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
                if e.path().is_dir() {
                    stack.push(e.path());
                } else {
                    out.push(e.path().strip_prefix(dir).unwrap().display().to_string());
                }
            }
        }
        out
    }

    #[test]
    fn paths_stay_in_the_image() {
        let r = Path::new("/ctx");
        assert_eq!(under(r, "/home/a/x").unwrap(), Path::new("/ctx/home/a/x"));
        assert!(under(r, "home/a").is_err());
        assert!(under(r, "/home/../etc").is_err());
    }
}
