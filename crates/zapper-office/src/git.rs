//! Reading a run's git repository for the UI: branches, commits, trees,
//! files and diffs. Read-only; every ref and path from a request is checked
//! before it reaches `git`.

use std::path::Path;

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};

/// Largest file or diff returned.
const MAX_BYTES: usize = 1 << 20;

/// What a run whose repositories outlive it (a remote's, in `git.dir`)
/// recorded of them: where they are, and each one's branches at its start
/// and, once it ended, at its end (`remote::record`).
fn record(run_dir: &Path) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(run_dir.join("git.json")).ok()?).ok()
}

/// A recorded run's branches of a repository, `start` or `end`.
pub fn recorded(run_dir: &Path, name: &str, when: &str) -> Option<serde_json::Map<String, Value>> {
    record(run_dir)?["repos"][name][when].as_object().cloned()
}

/// What `r` meant in the run: once it ended, a branch is as it was then.
fn as_of_run(run_dir: &Path, name: &str, r: &str) -> String {
    recorded(run_dir, name, "end")
        .and_then(|end| end.get(r).and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| r.to_string())
}

/// A run's repositories, as on disk (a past run's config may differ):
/// `product` first, then by name.
pub fn repos(run_dir: &Path) -> Vec<String> {
    if let Some(rec) = record(run_dir) {
        let mut v: Vec<String> = rec["repos"]
            .as_object()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        v.sort_by_key(|n| n.to_ascii_lowercase());
        return v;
    }
    let mut v: Vec<String> = std::fs::read_dir(run_dir.join("git"))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| {
            e.file_name()
                .to_str()?
                .strip_suffix(".git")
                .map(str::to_string)
        })
        .filter(|n| valid_repo_name(n))
        .collect();
    v.sort_by_key(|n| (n != crate::firm::DEFAULT_REPO, n.clone()));
    v
}

/// Where a run's repository is.
pub fn repo_dir(run_dir: &Path, name: &str) -> std::path::PathBuf {
    match record(run_dir).and_then(|r| r["dir"].as_str().map(std::path::PathBuf::from)) {
        Some(root) => root.join(format!("{name}.git")),
        None => run_dir.join("git").join(format!("{name}.git")),
    }
}

/// A repository of the run: `name`, or its first.
fn repo(run_dir: &Path, name: Option<&str>) -> Result<std::path::PathBuf> {
    let all = repos(run_dir);
    let name = match name {
        Some(n) if all.iter().any(|x| x == n) => n.to_string(),
        Some(n) => bail!("this run has no repository {n:?}"),
        None => all
            .first()
            .cloned()
            .ok_or_else(|| anyhow!("this run has no git repository"))?,
    };
    Ok(repo_dir(run_dir, &name))
}

/// The repository's name, as `repo` would pick it.
fn repo_name(run_dir: &Path, name: Option<&str>) -> String {
    name.map(str::to_string)
        .or_else(|| repos(run_dir).into_iter().next())
        .unwrap_or_default()
}

/// A repository's name, as the firm's server serves it and a remote may
/// spell it: letters, digits, `-`, `_` and `.`, not starting with a dot or
/// ending in `.git`.
pub fn valid_repo_name(n: &str) -> bool {
    (1..=100).contains(&n.len())
        && !n.starts_with('.')
        && !n.ends_with(".git")
        && n.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// A branch name or a commit id, safe to pass to git.
pub fn check_ref(r: &str) -> Result<()> {
    let ok = !r.is_empty()
        && r.len() <= 200
        && !r.starts_with('-')
        && !r.contains("..")
        && !r.ends_with('/')
        && r.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._/-".contains(&b));
    if !ok {
        bail!("{r:?} is not a branch or commit");
    }
    Ok(())
}

/// A path inside the tree: relative, no `.`, `..` or empty parts.
pub fn check_path(p: &str) -> Result<()> {
    if p.is_empty() {
        return Ok(());
    }
    if p.len() > 1000
        || p.starts_with('/')
        || p.split('/').any(|c| c.is_empty() || c == "." || c == "..")
        || p.contains('\0')
        || p.contains('\n')
    {
        bail!("{p:?} is not a path in the repository");
    }
    Ok(())
}

async fn git(repo: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let out = tokio::process::Command::new("git")
        .arg("--git-dir")
        .arg(repo)
        .args(["-c", "core.quotepath=off", "-c", "color.ui=never"])
        .args(args)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", "/nonexistent")
        .output()
        .await?;
    if !out.status.success() {
        let e = String::from_utf8_lossy(&out.stderr);
        bail!("{}", e.lines().next().unwrap_or("git failed"));
    }
    Ok(out.stdout)
}

/// Branches, `main` first, with their last commit.
pub async fn branches(run_dir: &Path, name: Option<&str>) -> Result<Value> {
    let r = repo(run_dir, name)?;
    // An ended run's branches are as they were when it ended.
    let out = match recorded(run_dir, &repo_name(run_dir, name), "end") {
        Some(end) => {
            let mut out = Vec::new();
            for (b, c) in end {
                let Some(c) = c.as_str() else { continue };
                let one = git(&r, &["show", "-s", "--format=%H%x1f%an%x1f%ct%x1f%s", c, "--"]).await?;
                out.extend_from_slice(format!("{b}\x1f{}\n", String::from_utf8_lossy(&one).trim()).as_bytes());
            }
            out
        }
        None => git(&r, &["for-each-ref", "--sort=-committerdate", "--format=%(refname:short)%1f%(objectname)%1f%(authorname)%1f%(committerdate:unix)%1f%(subject)", "refs/heads/"]).await?,
    };
    let mut list: Vec<Value> = String::from_utf8_lossy(&out)
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\x1f').collect();
            (f.len() >= 5).then(|| json!({ "name": f[0], "commit": f[1], "author": f[2], "ts": f[3].parse::<f64>().unwrap_or(0.0), "subject": f[4] }))
        })
        .collect();
    list.sort_by_key(|b| if b["name"] == "main" { 0 } else { 1 });
    Ok(json!(list))
}

/// Commits reachable from a ref, newest first, each with its subject and
/// the first line of its description.
pub async fn log(run_dir: &Path, name: Option<&str>, r: &str, limit: usize) -> Result<Value> {
    check_ref(r)?;
    let rp = repo(run_dir, name)?;
    let r = &as_of_run(run_dir, &repo_name(run_dir, name), r);
    let n = format!("--max-count={}", limit.clamp(1, 1000));
    let out = git(
        &rp,
        &[
            "log",
            &n,
            "--date=unix",
            "--pretty=format:%H%x1f%an%x1f%ad%x1f%s%x1f%b%x1e",
            r,
            "--",
        ],
    )
    .await?;
    let commits: Vec<Value> = String::from_utf8_lossy(&out)
        .split('\x1e')
        .filter_map(|rec| {
            let f: Vec<&str> = rec.trim_start_matches('\n').splitn(5, '\x1f').collect();
            (f.len() == 5).then(|| {
                let body: String = f[4].lines().map(str::trim).find(|l| !l.is_empty() && !is_trailer(l)).unwrap_or("").chars().take(300).collect();
                json!({ "commit": f[0], "author": f[1], "ts": f[2].parse::<f64>().unwrap_or(0.0), "subject": f[3], "body": body })
            })
        })
        .collect();
    Ok(json!(commits))
}

/// A trailer such as `Co-Authored-By: …` or `Signed-off-by: …`: not a
/// description.
fn is_trailer(line: &str) -> bool {
    let Some((key, _)) = line.split_once(": ") else {
        return false;
    };
    let key = key.to_ascii_lowercase();
    (key.ends_with("-by") && key.bytes().all(|b| b.is_ascii_alphabetic() || b == b'-'))
        || key == "change-id"
}

/// The entries of a directory at a ref.
pub async fn tree(run_dir: &Path, name: Option<&str>, r: &str, path: &str) -> Result<Value> {
    check_ref(r)?;
    check_path(path)?;
    let rp = repo(run_dir, name)?;
    let r = &as_of_run(run_dir, &repo_name(run_dir, name), r);
    let spec = if path.is_empty() {
        r.to_string()
    } else {
        format!("{r}:{path}")
    };
    let out = git(&rp, &["ls-tree", "-l", "-z", &spec]).await?;
    let mut entries: Vec<Value> = out
        .split(|b| *b == 0)
        .filter(|e| !e.is_empty())
        .filter_map(|e| {
            let line = String::from_utf8_lossy(e);
            let (meta, name) = line.split_once('\t')?;
            let f: Vec<&str> = meta.split_whitespace().collect();
            Some(json!({ "name": name, "type": f.get(1)?, "size": f.get(3).and_then(|s| s.parse::<u64>().ok()) }))
        })
        .collect();
    entries.sort_by_key(|e| {
        (
            e["type"] != "tree",
            e["name"].as_str().unwrap_or("").to_lowercase(),
        )
    });
    Ok(json!(entries))
}

/// A file at a ref, as text when it is UTF-8.
pub async fn blob(run_dir: &Path, name: Option<&str>, r: &str, path: &str) -> Result<Value> {
    check_ref(r)?;
    check_path(path)?;
    if path.is_empty() {
        bail!("give a file path");
    }
    let rp = repo(run_dir, name)?;
    let r = &as_of_run(run_dir, &repo_name(run_dir, name), r);
    let spec = format!("{r}:{path}");
    let size: usize = String::from_utf8_lossy(&git(&rp, &["cat-file", "-s", &spec]).await?)
        .trim()
        .parse()
        .map_err(|_| anyhow!("no file {path}"))?;
    if size > MAX_BYTES {
        return Ok(
            json!({ "path": path, "size": size, "text": null, "note": "too large to show" }),
        );
    }
    let data = git(&rp, &["cat-file", "blob", &spec]).await?;
    Ok(match String::from_utf8(data) {
        Ok(t) => json!({ "path": path, "size": size, "text": t }),
        Err(_) => json!({ "path": path, "size": size, "text": null, "note": "binary file" }),
    })
}

/// A commit: its message, files changed and diff.
pub async fn commit(run_dir: &Path, name: Option<&str>, id: &str) -> Result<Value> {
    if id.len() < 7 || id.len() > 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("{id:?} is not a commit id");
    }
    let rp = repo(run_dir, name)?;
    let head = git(
        &rp,
        &[
            "show",
            "-s",
            "--date=unix",
            "--format=%H%x1f%an%x1f%ad%x1f%P%x1f%B",
            id,
            "--",
        ],
    )
    .await?;
    let head = String::from_utf8_lossy(&head).into_owned();
    let f: Vec<&str> = head.splitn(5, '\x1f').collect();
    if f.len() < 5 {
        bail!("no commit {id}");
    }
    let stat = git(&rp, &["show", "--format=", "--numstat", id, "--"]).await?;
    let files: Vec<Value> = String::from_utf8_lossy(&stat)
        .lines()
        .filter_map(|l| {
            let p: Vec<&str> = l.splitn(3, '\t').collect();
            (p.len() == 3).then(|| json!({ "added": p[0], "removed": p[1], "path": p[2] }))
        })
        .collect();
    let mut diff = git(
        &rp,
        &["show", "--format=", "--patch", "--no-ext-diff", id, "--"],
    )
    .await?;
    let truncated = diff.len() > MAX_BYTES;
    diff.truncate(MAX_BYTES);
    Ok(json!({
        "commit": f[0], "author": f[1], "ts": f[2].parse::<f64>().unwrap_or(0.0), "parents": f[3].split_whitespace().collect::<Vec<_>>(),
        "message": f[4].trim_end(), "files": files, "diff": String::from_utf8_lossy(&diff), "truncated": truncated,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refs_and_paths() {
        assert!(check_ref("main").is_ok());
        assert!(check_ref("feature/todo-api").is_ok());
        assert!(check_ref("--upload-pack=x").is_err());
        assert!(check_ref("main..evil").is_err());
        assert!(check_ref("a b").is_err());
        assert!(check_path("src/app.py").is_ok());
        assert!(check_path("").is_ok());
        assert!(check_path("../x").is_err());
        assert!(check_path("/etc/passwd").is_err());
        assert!(check_path("a//b").is_err());
        assert!(is_trailer("Co-Authored-By: Claude <noreply@anthropic.com>"));
        assert!(is_trailer("Signed-off-by: ada <ada@acme>"));
        assert!(!is_trailer("Note: keeps the API stable"));
        assert!(!is_trailer("Adds the README"));
    }
}
