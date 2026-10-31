//! A firm's git remote (proposal 09): agents work only with the firm's own
//! git server, and the daemon is the remote's only client.
//!
//! The repositories are the ones the firm's GitHub App is installed on, kept
//! in `git.dir` across runs: each cloned (if absent) or fetched when a run
//! first uses it. Each
//! repository's `main` has one source of truth, so it only ever flows one
//! way:
//!
//! - **the remote's** (the default, for code): the board merges in GitHub's
//!   UI, and the daemon fetches `main` back. On the firm's server nobody
//!   pushes `main`. Agents send a branch for review with `publish_branch`;
//!   the daemon pushes it as `<worker>/<branch>`, opens its pull request, and
//!   mails the author when its checks finish, when it is reviewed or
//!   commented on, and when it is merged or closed.
//! - **the firm's** (`git.local`, the knowledge base): merged on the firm's
//!   server by whoever holds `admin`, and pushed out after each update,
//!   fast-forward only. If the remote's `main` moved by another route, the
//!   daemon stops pushing and tells the board.
//!
//! The daemon's tokens come from `zapper mint github-app`, so the App's key
//! is read by a short-lived process and never by the daemon; they are
//! revoked when the run stops.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use zapper_core::mint;
use zapper_github::{GitHub, Token};

use super::FirmRun;
use crate::firm::Firm;

/// How often the daemon looks at the remote.
const POLL_SECS: u64 = 60;
/// What the daemon's tokens may do: push branches and `main` (of a
/// local-main repository), open pull requests, read their checks.
const PERMISSIONS: [&str; 5] = [
    "contents=write",
    "pull_requests=write",
    "checks=read",
    "actions=read",
    "statuses=read",
];
/// The most of a failing job's log `pr_status` returns.
const LOG_TAIL: usize = 4 << 10;

/// Tells the board, once per repository and run, that `repo` has no `main`
/// on the remote, which only it can make, and how.
fn no_main(run: &FirmRun, repo: &str, worker: &str, branch: &str) {
    if !run
        .facility
        .remote
        .no_main
        .lock()
        .unwrap()
        .insert(repo.to_string())
    {
        return;
    }
    let full = run
        .config
        .remote_repo(repo)
        .unwrap_or_else(|| repo.to_string());
    let url = url(run, repo).unwrap_or_default();
    run.journal.append(
        "git_remote",
        json!({ "repo": repo, "event": "no main", "worker": worker, "branch": branch }),
    );
    run.notice(
        super::BOARD,
        &format!("{repo} has no main on the remote"),
        &format!("[Automatic notice] {worker} wants to open a pull request for their branch {branch} of {full}, but the repository has no main there yet, so there is nothing to merge into. Only you can make it. An empty first commit is enough, from a fresh directory with your own credentials:\n\n    git init {repo} && cd {repo}\n    git commit --allow-empty -m \"Start the repository\"\n    git push {url} HEAD:refs/heads/main\n\nThen make main the default branch, and check its ruleset. The firm's server fetches it within a minute; tell {worker}, who moves the branch onto it and publishes it again.\n"),
    );
}

/// A branch published for review.
#[derive(Debug, Clone)]
pub struct Published {
    pub worker: String,
    pub repo: String,
    pub branch: String,
    pub number: u64,
    pub url: String,
    /// The head whose finished checks the author was told about.
    reported: Option<String>,
    /// Reviews and comments the author was told about.
    seen: BTreeSet<(String, u64)>,
}

/// What the daemon knows of the remote while a run goes on.
#[derive(Default)]
pub struct State {
    /// The token commands used, to revoke what they minted.
    specs: Mutex<HashMap<String, zapper_policy::mint::Spec>>,
    /// Open pull requests, by repository and branch on the remote.
    published: Mutex<BTreeMap<(String, String), Published>>,
    /// Local-main repositories whose remote `main` moved by another route.
    diverged: Mutex<BTreeSet<String>>,
    /// The repositories this run has brought in step, each once.
    touched: Mutex<HashMap<String, Arc<tokio::sync::OnceCell<()>>>>,
    /// Repositories with no `main` on the remote the board was told of, once
    /// each.
    no_main: Mutex<BTreeSet<String>>,
}

/// The firm for a run that starts now: with a remote, the account its App is
/// installed on and the repositories it reaches there (or the ones the run
/// was given, which must be among them), by name.
pub async fn with_repositories(mut firm: Firm) -> Result<Firm> {
    let Some(gh) = firm.git_remote().cloned() else {
        return Ok(firm);
    };
    let file = zapper_github::app_file(&gh.config)?;
    let given = firm
        .git
        .as_ref()
        .map(|g| g.found.clone())
        .unwrap_or_default();
    let (app, owner, names) = resolve(&file, &given).await?;
    let g = firm.git.as_mut().expect("a remote is under git");
    for l in &g.local {
        if !names.iter().any(|n| n.eq_ignore_ascii_case(l)) {
            bail!("git.local: {l} is not a repository the App is installed on (it reaches: {}); create it on the remote and install the App on it", names.join(", "));
        }
    }
    g.owner = owner;
    g.found = names;
    g.api_url = app.api_url;
    g.web_url = app.web_url;
    Ok(firm)
}

/// The account the App in `file` is installed on and the names of its
/// repositories there, sorted as GitHub spells them; or the `given` ones
/// (`name` or `owner/name`), which must be among them. With the App, whose
/// file says where its GitHub is.
pub async fn resolve(
    file: &Path,
    given: &[String],
) -> Result<(zapper_github::App, String, Vec<String>)> {
    if !file.exists() {
        bail!(
            "no GitHub App at {}; create it with `zapper github setup --org <account>` (or --user <login>)",
            file.display()
        );
    }
    let app = zapper_github::App::load(file, &mut |w| tracing::warn!("{w}"))?;
    let gh = GitHub::for_app(&app);
    let inst = gh.the_installation(&app).await?;
    let owner = inst.account.clone().unwrap_or_default();
    let mut names: Vec<String> = gh
        .installation_repositories(&app, inst.id)
        .await?
        .into_iter()
        .filter_map(|r| {
            let (o, n) = r.split_once('/')?;
            o.eq_ignore_ascii_case(&owner).then(|| n.to_string())
        })
        .filter(|n| crate::git::valid_repo_name(n))
        .collect();
    names.sort_by_key(|n| n.to_ascii_lowercase());
    names.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    if given.is_empty() {
        return Ok((app, owner, names));
    }
    let mut chosen: Vec<String> = Vec::new();
    for g in given {
        let name = match g.split_once('/') {
            Some((o, _)) if !o.eq_ignore_ascii_case(&owner) => {
                bail!("{g}: this firm's repositories are {owner}'s (the App's account)")
            }
            Some((_, n)) => n,
            None => g.as_str(),
        };
        let found = names
            .iter()
            .find(|x| x.eq_ignore_ascii_case(name))
            .ok_or_else(|| anyhow!("the GitHub App is not installed on {owner}/{name}; install it there or check the name. It reaches: {}", names.join(", ")))?;
        if !chosen.contains(found) {
            chosen.push(found.clone());
        }
    }
    Ok((app, owner, chosen))
}

/// The remote's API, as its App's file says.
fn github(run: &FirmRun) -> GitHub {
    GitHub::new(
        run.config
            .git
            .as_ref()
            .map(|g| g.api_url.clone())
            .filter(|u| !u.is_empty()),
    )
}

/// Where the firm's repositories are: `git.dir` with a remote, else the run's
/// own `git` directory.
pub fn root(firm: &Firm, run_dir: &Path) -> PathBuf {
    match firm
        .git
        .as_ref()
        .filter(|g| g.remote.is_some())
        .and_then(|g| g.dir.as_deref())
    {
        Some(d) => PathBuf::from(zapper_policy::expand_home(d)),
        None => run_dir.join("git"),
    }
}

fn repo_path(run: &FirmRun, repo: &str) -> PathBuf {
    root(&run.config, &run.dir).join(format!("{repo}.git"))
}

fn url(run: &FirmRun, repo: &str) -> Result<String> {
    let full = run
        .config
        .remote_repo(repo)
        .ok_or_else(|| anyhow!("{repo} has no remote"))?;
    let base = run
        .config
        .git
        .as_ref()
        .map(|g| g.web_url.as_str())
        .filter(|u| !u.is_empty())
        .unwrap_or(zapper_github::WEB_URL);
    Ok(format!("{}/{full}.git", base.trim_end_matches('/')))
}

/// A token for one repository, minted (and cached until near its expiry) by
/// the mint command.
async fn token(run: &FirmRun, repo: &str) -> Result<Token> {
    let gh = run
        .config
        .git_remote()
        .ok_or_else(|| anyhow!("no remote"))?;
    let full = run
        .config
        .remote_repo(repo)
        .ok_or_else(|| anyhow!("{repo} has no remote"))?;
    let mut argv: Vec<String> = if gh.mint.is_empty() {
        ["zapper", "mint", "github-app"].map(String::from).to_vec()
    } else {
        gh.mint.clone()
    };
    argv.extend(["--app".into(), gh.config.clone(), "--repo".into(), full]);
    for p in PERMISSIONS {
        argv.extend(["--perm".into(), p.into()]);
    }
    let spec = zapper_policy::mint::Spec::new(&argv, None, None);
    run.facility
        .remote
        .specs
        .lock()
        .unwrap()
        .insert(repo.to_string(), spec.clone());
    let value = mint::resolve(&spec)
        .await
        .context("minting a token for the remote")?;
    Ok(Token {
        value,
        expires_at: String::new(),
    })
}

/// Runs git on a bare repository; with a token, authenticated to the remote
/// by a header passed in the environment, never on the command line.
async fn git(dir: &Path, args: &[&str], token: Option<&Token>) -> Result<String> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("--git-dir")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null());
    if let Some(t) = token {
        cmd.env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "http.extraHeader")
            .env("GIT_CONFIG_VALUE_0", zapper_github::git_auth_header(t));
    }
    let out = cmd
        .output()
        .await
        .context("running git (is it installed?)")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!("git {}: {}", args.first().unwrap_or(&""), err.trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// A ref's commit, or `None` if there is no such ref.
async fn rev(dir: &Path, r: &str) -> Option<String> {
    git(
        dir,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{r}^{{commit}}"),
        ],
        None,
    )
    .await
    .ok()
    .filter(|s| !s.is_empty())
}

async fn is_ancestor(dir: &Path, a: &str, b: &str) -> bool {
    git(dir, &["merge-base", "--is-ancestor", a, b], None)
        .await
        .is_ok()
}

/// The hook of a repository whose `main` is the remote's: nobody pushes
/// `main`, `main/*` or tags; the daemon moves `main` from the remote.
const PRE_RECEIVE_REMOTE: &str = r#"#!/bin/sh
# Installed by zapper: main comes from the remote, where the board merges.
if [ "$ZAPPER_GIT_LEVEL" = merge ]; then
    echo "zapper: main of this repository is merged on the remote, by the board; merge pushes nothing here" >&2
    exit 1
fi
while read old new ref; do
    case "$ref" in
        refs/heads/main|refs/heads/main/*|refs/tags/*)
            echo "zapper: $ref comes from the remote; push a branch and send it for review with publish_branch" >&2
            exit 1 ;;
    esac
done
exit 0
"#;

/// Readies the firm's repositories for a run without cloning any: the
/// folder made, any repository the App no longer reaches moved to
/// `archive/`. Each is brought in step when it is first used (`ensure`).
pub async fn prepare(run: &Arc<FirmRun>) -> Result<()> {
    let root = root(&run.config, &run.dir);
    std::fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;
    archive_unknown(&root, &run.config.git_repos(), run)?;
    Ok(())
}

/// A repository, ready for this run: cloned into `git.dir` if it isn't
/// there, fetched if it is, with the hook of its kind of `main`, its
/// starting point recorded, and, for the knowledge base, its layout. Done
/// once per repository and run, when it is first used; callers of the same
/// repository wait for the one doing it, and a failure is tried again on
/// the next use.
pub async fn ensure(run: &Arc<FirmRun>, name: &str) -> Result<()> {
    if run.config.git_remote().is_none() {
        return Ok(());
    }
    let cell = run
        .facility
        .remote
        .touched
        .lock()
        .unwrap()
        .entry(name.to_string())
        .or_default()
        .clone();
    cell.get_or_try_init(|| bring_in(run, name)).await?;
    Ok(())
}

/// The repositories this run has used.
fn touched(run: &FirmRun) -> Vec<String> {
    let t = run.facility.remote.touched.lock().unwrap();
    t.iter()
        .filter(|(_, c)| c.initialized())
        .map(|(n, _)| n.clone())
        .collect()
}

async fn bring_in(run: &Arc<FirmRun>, name: &str) -> Result<()> {
    let root = root(&run.config, &run.dir);
    std::fs::create_dir_all(&root)?;
    let dir = root.join(format!("{name}.git"));
    let token = token(run, name).await?;
    if dir.exists() {
        install_hook(run, name, &dir).await?;
        if let Err(e) = fetch(run, name, &dir, &token).await {
            // The copy here is still good to work on; the remote catches up
            // when it can be reached.
            run.journal.append(
                "git_remote",
                json!({ "repo": name, "event": "fetch failed", "error": format!("{e:#}") }),
            );
        }
    } else {
        let out = tokio::process::Command::new("git")
            .args(["clone", "--bare", "--quiet"])
            .arg(url(run, name)?)
            .arg(&dir)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "http.extraHeader")
            .env("GIT_CONFIG_VALUE_0", zapper_github::git_auth_header(&token))
            .output()
            .await
            .context("git clone (is git installed?)")?;
        if !out.status.success() {
            bail!(
                "cloning {name} from the remote: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        install_hook(run, name, &dir).await?;
        run.journal
            .append("git_remote", json!({ "repo": name, "event": "cloned" }));
    }
    if run.config.knowledge_base().as_deref() == Some(name) {
        super::kb::seed(run).await?;
    }
    record_repo(run, name, "start").await;
    Ok(())
}

async fn install_hook(run: &FirmRun, name: &str, dir: &Path) -> Result<()> {
    git(dir, &["config", "http.receivepack", "true"], None).await?;
    let hook = if run.config.main_is_local(name) {
        zapper_git::PRE_RECEIVE
    } else {
        PRE_RECEIVE_REMOTE
    };
    std::fs::create_dir_all(dir.join("hooks"))?;
    let path = dir.join("hooks").join("pre-receive");
    std::fs::write(&path, hook)?;
    std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755))?;
    Ok(())
}

/// Moves a repository the App no longer reaches out of the way, never
/// deleting it.
fn archive_unknown(root: &Path, names: &[String], run: &FirmRun) -> Result<()> {
    for e in std::fs::read_dir(root)?.flatten() {
        let file = e.file_name().to_string_lossy().into_owned();
        let Some(name) = file.strip_suffix(".git") else {
            continue;
        };
        if names.iter().any(|n| n == name) || !e.path().is_dir() {
            continue;
        }
        let archive = root.join("archive");
        std::fs::create_dir_all(&archive)?;
        let mut to = archive.join(&file);
        if to.exists() {
            to = archive.join(format!("{name}-{}.git", crate::now() as u64));
        }
        std::fs::rename(e.path(), &to)?;
        run.journal.append(
            "git_remote",
            json!({ "repo": name, "event": "archived", "to": to.display().to_string() }),
        );
    }
    Ok(())
}

/// Brings a repository's `main` in step: the remote's fetched in, or the
/// firm's pushed out.
async fn fetch(run: &Arc<FirmRun>, name: &str, dir: &Path, token: &Token) -> Result<bool> {
    if run.config.main_is_local(name) {
        push_main(run, name).await?;
        return Ok(false);
    }
    let before = rev(dir, "refs/heads/main").await;
    git(
        dir,
        &[
            "fetch",
            "--quiet",
            &url(run, name)?,
            "+refs/heads/main:refs/heads/main",
        ],
        Some(token),
    )
    .await?;
    let after = rev(dir, "refs/heads/main").await;
    if before != after {
        run.journal.append(
            "git_remote",
            json!({ "repo": name, "event": "main fetched", "from": before, "to": after }),
        );
    }
    Ok(before != after)
}

/// Pushes a local-main repository's `main` to the remote, fast-forward only.
/// If the remote's moved by another route, it stops and tells the board,
/// once.
pub async fn push_main(run: &Arc<FirmRun>, name: &str) -> Result<()> {
    let dir = repo_path(run, name);
    let Some(local) = rev(&dir, "refs/heads/main").await else {
        return Ok(());
    };
    let token = token(run, name).await?;
    let u = url(run, name)?;
    // An empty remote has no main to fetch.
    let _ = git(
        &dir,
        &[
            "fetch",
            "--quiet",
            &u,
            "+refs/heads/main:refs/zapper/remote-main",
        ],
        Some(&token),
    )
    .await;
    let remote = rev(&dir, "refs/zapper/remote-main").await;
    if remote.as_deref() == Some(local.as_str()) {
        run.facility.remote.diverged.lock().unwrap().remove(name);
        return Ok(());
    }
    if let Some(r) = &remote {
        if !is_ancestor(&dir, r, &local).await {
            if run
                .facility
                .remote
                .diverged
                .lock()
                .unwrap()
                .insert(name.to_string())
            {
                run.journal.append(
                    "git_remote",
                    json!({ "repo": name, "event": "diverged", "local": local, "remote": r }),
                );
                run.notice(
                    crate::firm::BOARD,
                    &format!("{name}: main changed on the remote"),
                    &format!("[Automatic notice] {name}'s main is the firm's, and is pushed to the remote; but the remote's main ({r}) is not behind the firm's ({local}), so it changed there by another route. Nothing more is pushed until the remote's main is reset to a commit of the firm's.\n"),
                );
            }
            return Ok(());
        }
    }
    git(
        &dir,
        &["push", "--quiet", &u, "refs/heads/main:refs/heads/main"],
        Some(&token),
    )
    .await?;
    run.facility.remote.diverged.lock().unwrap().remove(name);
    run.journal.append(
        "git_remote",
        json!({ "repo": name, "event": "main pushed", "to": local }),
    );
    Ok(())
}

/// After a push to the firm's server: a local-main repository's new `main`
/// goes out. (A push means the repository was brought in first.)
pub fn after_push(run: &Arc<FirmRun>, repo: &str) {
    if run.config.git_remote().is_none() || !run.config.main_is_local(repo) {
        return;
    }
    let (run, repo) = (run.clone(), repo.to_string());
    tokio::spawn(async move {
        if let Err(e) = push_main(&run, &repo).await {
            run.journal.append(
                "git_remote",
                json!({ "repo": repo, "event": "push failed", "error": format!("{e:#}") }),
            );
        }
    });
}

/// A repository's branches and their commits, when this run first used it
/// or at its end, in `git.json` in the run directory: what replay and the
/// Git tab show of a run whose repositories outlive it.
async fn record_repo(run: &FirmRun, name: &str, when: &str) {
    record(run, &[name.to_string()], when).await
}

async fn record(run: &FirmRun, names: &[String], when: &str) {
    let root = root(&run.config, &run.dir);
    let path = run.dir.join("git.json");
    // One writer at a time: repositories are brought in concurrently.
    static WRITING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _one = WRITING.lock().await;
    let mut v: Value = std::fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_else(|| json!({ "dir": root.display().to_string(), "repos": {} }));
    for name in names {
        let refs = git(
            &root.join(format!("{name}.git")),
            &[
                "for-each-ref",
                "--format=%(refname:short) %(objectname)",
                "refs/heads",
            ],
            None,
        )
        .await
        .unwrap_or_default();
        let refs: serde_json::Map<String, Value> = refs
            .lines()
            .filter_map(|l| l.split_once(' '))
            .map(|(r, c)| (r.to_string(), json!(c)))
            .collect();
        v["repos"][name][when] = Value::Object(refs);
    }
    if let Ok(b) = serde_json::to_vec_pretty(&v) {
        let _ = std::fs::write(&path, b);
    }
}

/// Sends a worker's branch for review: checked, pushed to the remote as
/// `<worker>/<branch>`, and its pull request opened, or updated if one is
/// open.
pub async fn publish_branch(
    run: &Arc<FirmRun>,
    worker: &str,
    repo: &str,
    branch: &str,
    title: &str,
    body: &str,
) -> Result<Value> {
    if run.config.git_remote().is_none() {
        bail!("this firm's repositories have no remote: review happens on the firm's server");
    }
    let resource = super::access::resource(run, &format!("git:{repo}"))?;
    let repo = resource.trim_start_matches("git:").to_string();
    if run.config.main_is_local(&repo) {
        bail!("{repo}'s main is the firm's: ask whoever holds merge (or admin) on it to review and merge your branch");
    }
    if !super::access::level(run, worker, &resource).covers(super::access::Level::Write) {
        bail!("publishing a branch of {repo} needs write on it (credentials_request git:{repo})");
    }
    crate::git::check_ref(branch)?;
    if branch == "main" || branch.starts_with("main/") {
        bail!("publish a branch of your own, not main");
    }
    if title.trim().is_empty() {
        bail!("give the pull request a title");
    }
    ensure(run, &repo).await?;
    let dir = repo_path(run, &repo);
    let local = format!("refs/heads/{branch}");
    let head = rev(&dir, &local).await.ok_or_else(|| {
        anyhow!("{repo} has no branch {branch} on the firm's server: push it first")
    })?;
    let token = token(run, &repo).await?;
    // A pull request needs a main to merge into: one made on the remote a
    // moment ago is fetched now; with none there, nothing is pushed, and
    // the board, who makes main, is told.
    if rev(&dir, "refs/heads/main").await.is_none() {
        let _ = fetch(run, &repo, &dir, &token).await;
    }
    let Some(main) = rev(&dir, "refs/heads/main").await else {
        no_main(run, &repo, worker, branch);
        bail!("{repo} has no main on the remote yet, so a pull request has nothing to merge into. The board makes main and has been told: end your turn and wait for it. Then `git fetch`, move your branch onto main (`git rebase --onto origin/main --root {branch}` when it has no history in common with it), push it, and publish again");
    };
    {
        if !is_ancestor(&dir, &main, &head).await {
            bail!("{branch} isn't based on the tip of main: `git fetch && git rebase origin/main`, push, then publish again");
        }
        let changed = git(&dir, &["diff", "--name-only", &main, &head], None).await?;
        if let Some(f) = changed
            .lines()
            .find(|f| f.starts_with(".github/workflows/"))
        {
            bail!("{branch} changes {f}: changes to GitHub Actions workflows are the board's. Take them out of the branch and send the board the patch");
        }
    }
    let remote_branch = format!("{worker}/{branch}");
    git(
        &dir,
        &[
            "push",
            "--quiet",
            "--force",
            &url(run, &repo)?,
            &format!("{local}:refs/heads/{remote_branch}"),
        ],
        Some(&token),
    )
    .await
    .context("pushing the branch to the remote")?;
    let gh = github(run);
    let full = run.config.remote_repo(&repo).unwrap_or_default();
    let pull = match gh.open_pull(&token, &full, &remote_branch).await? {
        Some(p) => p,
        None => {
            let body = format!(
                "{}\n\n---\nPublished by {worker}, of {}, from the firm's git server.",
                body.trim(),
                run.company
            );
            gh.create_pull(&token, &full, &remote_branch, "main", title.trim(), &body)
                .await?
        }
    };
    run.facility
        .remote
        .published
        .lock()
        .unwrap()
        .entry((repo.clone(), remote_branch.clone()))
        .or_insert(Published {
            worker: worker.into(),
            repo: repo.clone(),
            branch: branch.into(),
            number: pull.number,
            url: pull.url.clone(),
            reported: None,
            seen: BTreeSet::new(),
        });
    run.journal.append(
        "git_remote",
        json!({ "repo": repo, "event": "published", "worker": worker, "branch": branch, "commit": head, "pull": pull.number, "url": pull.url }),
    );
    Ok(json!({
        "pull": pull.number, "url": pull.url, "branch": remote_branch, "commit": head,
        "note": "You'll get mail when its checks finish, when it is reviewed or commented on, and when the board merges or closes it; pr_status has the details now. Pushing the branch again and publishing again updates it.",
    }))
}

/// A published branch's pull request: its state, checks (with the end of the
/// failing jobs' logs), reviews and comments.
pub async fn pr_status(
    run: &Arc<FirmRun>,
    worker: &str,
    repo: &str,
    branch: &str,
) -> Result<Value> {
    let resource = super::access::resource(run, &format!("git:{repo}"))?;
    let repo = resource.trim_start_matches("git:").to_string();
    if super::access::level(run, worker, &resource) == super::access::Level::None {
        bail!("you have no access to {repo}");
    }
    let remote_branch = if branch.contains('/')
        && run
            .facility
            .remote
            .published
            .lock()
            .unwrap()
            .contains_key(&(repo.clone(), branch.to_string()))
    {
        branch.to_string()
    } else {
        format!("{worker}/{branch}")
    };
    let token = token(run, &repo).await?;
    let gh = github(run);
    let full = run
        .config
        .remote_repo(&repo)
        .ok_or_else(|| anyhow!("{repo} has no remote"))?;
    let number = run
        .facility
        .remote
        .published
        .lock()
        .unwrap()
        .get(&(repo.clone(), remote_branch.clone()))
        .map(|p| p.number);
    let pull = match number {
        Some(n) => gh.pull(&token, &full, n).await?,
        None => gh
            .open_pull(&token, &full, &remote_branch)
            .await?
            .ok_or_else(|| {
                anyhow!("{branch} of {repo} has no open pull request: send it with publish_branch")
            })?,
    };
    let checks = gh
        .checks(&token, &full, &pull.head_sha)
        .await
        .unwrap_or_default();
    let mut out_checks = Vec::new();
    for c in &checks {
        let mut v = json!(c);
        if c.failed() {
            match gh.job_log(&token, &full, c.id).await {
                Ok(log) => {
                    let start = log.len().saturating_sub(LOG_TAIL);
                    let start = (start..log.len())
                        .find(|i| log.is_char_boundary(*i))
                        .unwrap_or(0);
                    v["log"] = json!(&log[start..]);
                }
                Err(e) => v["log_unavailable"] = json!(format!("{e:#}")),
            }
        }
        out_checks.push(v);
    }
    let remarks = gh
        .remarks(&token, &full, pull.number)
        .await
        .unwrap_or_default();
    Ok(json!({ "pull": pull, "checks": out_checks, "remarks": remarks }))
}

/// Keeps the run in step with the remote until it stops: remote `main`s
/// fetched, and what happened to each published branch told to its author.
pub async fn watch(run: Arc<FirmRun>) {
    loop {
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_secs(POLL_SECS)) => {}
            _ = run.stopping.notified() => return,
        }
        if run.ended() {
            return;
        }
        poll(&run).await;
    }
}

/// One look at the remote, for the repositories this run has used.
pub async fn poll(run: &Arc<FirmRun>) {
    for name in touched(run) {
        if run.config.main_is_local(&name) {
            if let Err(e) = push_main(run, &name).await {
                run.journal.append(
                    "git_remote",
                    json!({ "repo": name, "event": "push failed", "error": format!("{e:#}") }),
                );
            }
            continue;
        }
        let dir = repo_path(run, &name);
        match token(run, &name).await {
            Ok(t) => {
                if let Err(e) = fetch(run, &name, &dir, &t).await {
                    run.journal.append(
                        "git_remote",
                        json!({ "repo": name, "event": "fetch failed", "error": format!("{e:#}") }),
                    );
                }
            }
            Err(e) => {
                run.journal.append(
                    "git_remote",
                    json!({ "repo": name, "event": "fetch failed", "error": format!("{e:#}") }),
                );
            }
        }
    }
    let open: Vec<Published> = run
        .facility
        .remote
        .published
        .lock()
        .unwrap()
        .values()
        .cloned()
        .collect();
    for p in open {
        if let Err(e) = follow(run, p.clone()).await {
            run.journal.append("git_remote", json!({ "repo": p.repo, "event": "pull request check failed", "pull": p.number, "error": format!("{e:#}") }));
        }
    }
}

/// What happened to one pull request since the last look, told to its
/// author.
async fn follow(run: &Arc<FirmRun>, mut p: Published) -> Result<()> {
    let token = token(run, &p.repo).await?;
    let gh = github(run);
    let full = run.config.remote_repo(&p.repo).unwrap_or_default();
    let key = (p.repo.clone(), format!("{}/{}", p.worker, p.branch));
    let pull = gh.pull(&token, &full, p.number).await?;
    let what = format!("{} #{} ({})", p.repo, p.number, p.branch);
    if pull.merged || pull.state == "closed" {
        let merged = pull.merged;
        run.facility.remote.published.lock().unwrap().remove(&key);
        run.journal.append("git_remote", json!({ "repo": p.repo, "event": if merged { "merged" } else { "closed" }, "pull": p.number, "worker": p.worker }));
        if merged {
            // The board merged: main moves on the firm's server too.
            let dir = repo_path(run, &p.repo);
            let _ = fetch(run, &p.repo, &dir, &token).await;
        }
        run.notice(
            &p.worker,
            &format!("{} {what}", if merged { "Merged:" } else { "Closed:" }),
            &format!(
                "[Automatic notice] Your pull request {what}, \"{}\", was {} by the board. {}\n{}\n",
                pull.title,
                if merged { "merged" } else { "closed without merging" },
                if merged { "main on the firm's server has it now." } else { "Ask the board why if it isn't clear from the reviews." },
                p.url
            ),
        );
        return Ok(());
    }
    let mut lines = Vec::new();
    if p.reported.as_deref() != Some(pull.head_sha.as_str()) {
        let checks = gh
            .checks(&token, &full, &pull.head_sha)
            .await
            .unwrap_or_default();
        if !checks.is_empty() && checks.iter().all(|c| c.status == "completed") {
            let failed: Vec<&str> = checks
                .iter()
                .filter(|c| c.failed())
                .map(|c| c.name.as_str())
                .collect();
            lines.push(if failed.is_empty() {
                format!("Its checks passed ({}) at {}.", checks.len(), short(&pull.head_sha))
            } else {
                format!("Its checks finished at {} and {} failed: {}. pr_status has their logs; fix the branch, push, and publish again.", short(&pull.head_sha), failed.len(), failed.join(", "))
            });
            p.reported = Some(pull.head_sha.clone());
        }
    }
    let remarks = gh
        .remarks(&token, &full, p.number)
        .await
        .unwrap_or_default();
    for r in remarks {
        if p.seen.insert((r.kind.clone(), r.id)) {
            let verdict = r
                .verdict
                .as_deref()
                .map(|v| format!(" ({v})"))
                .unwrap_or_default();
            let at =
                r.at.as_deref()
                    .map(|a| format!(" on {a}"))
                    .unwrap_or_default();
            let body: String = r.body.chars().take(2000).collect();
            lines.push(format!(
                "{} {}{verdict}{at}: {}",
                r.author,
                r.kind,
                body.trim()
            ));
        }
    }
    run.facility
        .remote
        .published
        .lock()
        .unwrap()
        .insert(key, p.clone());
    if !lines.is_empty() {
        run.notice(
            &p.worker,
            &format!("Pull request {what}"),
            &format!(
                "[Automatic notice] On your pull request {what}:\n\n{}\n\n{}\n",
                lines.join("\n\n"),
                p.url
            ),
        );
    }
    Ok(())
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(8)]
}

/// The run is stopping: a local-main repository's `main` goes out a last
/// time, the end is recorded, and every token the daemon minted is revoked.
pub async fn finish(run: &Arc<FirmRun>) {
    if run.config.git_remote().is_some() {
        for name in touched(run) {
            if run.config.main_is_local(&name) {
                if let Err(e) = push_main(run, &name).await {
                    run.journal.append(
                        "git_remote",
                        json!({ "repo": name, "event": "push failed", "error": format!("{e:#}") }),
                    );
                }
            }
        }
    }
    if run.config.git_remote().is_some() {
        record(run, &touched(run), "end").await;
    }
    let specs: Vec<(String, zapper_policy::mint::Spec)> =
        run.facility.remote.specs.lock().unwrap().drain().collect();
    let gh = &github(run);
    let revokes = specs.into_iter().filter_map(|(repo, spec)| {
        let value = mint::peek(&spec);
        mint::forget(&spec);
        value.map(|value| async move {
            let t = Token {
                value,
                expires_at: String::new(),
            };
            let r = tokio::time::timeout(std::time::Duration::from_secs(10), gh.revoke(&t))
                .await
                .unwrap_or_else(|_| Err(anyhow!("timed out")));
            (repo, r)
        })
    });
    for (repo, r) in futures_util::future::join_all(revokes).await {
        run.journal.append("git_remote", json!({ "repo": repo, "event": if r.is_ok() { "token revoked" } else { "token revoke failed" }, "error": r.err().map(|e| format!("{e:#}")) }));
    }
}
