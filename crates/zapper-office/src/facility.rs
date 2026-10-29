//! A firm's services: its git repository, served by the daemon, and its
//! cluster, a k3s VM booted before the workers. Workers reach both by name
//! under the firm's domain through the proxy; the daemon routes those names
//! itself, so no VM reaches another VM or a host address.

use std::collections::HashMap;
use std::convert::Infallible;
use std::path::Path;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use zapper_core::prelude::*;

use anyhow::{anyhow, bail, Context, Result};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::task::JoinHandle;
use zapper_policy::proto::{Forward, GuestFile};
use zapper_policy::{Config, Policy};

use super::prompts::Services;
use super::{drive, FirmRun};
use zapper_core::util::random_id;
use zapper_core::Core;
use zapper_core::{NewSession, ServiceHandler, ServiceIo, ServiceSpec, ServiceTarget, Session};

/// Ports the cluster VM forwards from vsock to itself.
pub const API_PORT: u32 = 6443;
pub const HTTP_PORT: u32 = 80;
pub const HTTPS_PORT: u32 = 443;
/// The dashboard's NodePort, forwarded for the board only.
pub const DASHBOARD_PORT: u32 = 30466;
/// Headlamp, pinned in the k3s image (`HEADLAMP_IMAGE` in the Makefile).
const HEADLAMP_IMAGE: &str = "ghcr.io/headlamp-k8s/headlamp:v0.46.0";
/// The facility's name in the firm, and its host name.
pub const CLUSTER: &str = "cluster";
const MAX_GIT_BODY: usize = 512 << 20;

#[derive(Default)]
pub struct Facilities {
    pub cluster: Mutex<Option<Arc<Session>>>,
    pub kubeconfig: Mutex<Option<String>>,
    /// Per worker, for the git server.
    pub git_passwords: Mutex<HashMap<String, String>>,
    /// What the daemon knows of the firm's git remote.
    pub remote: super::remote::State,
    product: Mutex<Option<JoinHandle<()>>>,
    dashboard: Mutex<Option<JoinHandle<()>>>,
    /// The token the board signs in to the dashboard with.
    pub dashboard_token: Mutex<Option<String>>,
    /// The run's services and zone, removed when it stops.
    pub services: Mutex<Vec<u64>>,
}

impl Facilities {
    pub fn status(&self) -> Value {
        json!({
            "cluster": self.cluster.lock().unwrap().as_ref().map(|s| s.id.clone()),
            "product": self.product.lock().unwrap().is_some(),
            "dashboard": self.dashboard.lock().unwrap().is_some(),
            "dashboard_token": self.dashboard_token.lock().unwrap().clone(),
        })
    }

    pub fn session_of(&self, name: &str) -> Option<Arc<Session>> {
        (name == CLUSTER)
            .then(|| self.cluster.lock().unwrap().clone())
            .flatten()
    }
}

/// Opens a host-initiated vsock connection to a port of a session's VM.
pub async fn vsock_connect(s: &Session, port: u32) -> Result<UnixStream> {
    s.connect_port(port).await
}

/// The firm's git server, a service of its domain: TLS is terminated by
/// the daemon, and who asks is the session's member label.
struct GitServer {
    run: Weak<FirmRun>,
}

impl ServiceHandler for GitServer {
    fn serve(&self, s: Arc<Session>, io: Box<dyn ServiceIo>) {
        let Some(run) = self.run.upgrade() else {
            return;
        };
        let member = s.label("member").unwrap_or_default().to_string();
        tokio::spawn(async move {
            let svc = hyper::service::service_fn(move |req| {
                let (run, member) = (run.clone(), member.clone());
                async move { Ok::<_, Infallible>(git_request(&run, &member, req).await) }
            });
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(io), svc)
                .await;
        });
    }
}

fn add_service(
    d: &impl ServiceApi,
    run: &FirmRun,
    name: String,
    port: u16,
    target: ServiceTarget,
) -> Result<()> {
    let id = d.add_service(ServiceSpec {
        name,
        port,
        sessions: super::run_sessions(run),
        target,
        owner: format!("firm {}", run.firm),
    })?;
    run.facility.services.lock().unwrap().push(id);
    Ok(())
}

/// The firm's domain, a zone of the run's sessions (its names are the
/// firm's services, never looked up on the host), and its git server.
pub fn register(d: &impl ServiceApi, run: &Arc<FirmRun>) -> Result<()> {
    let id = d.add_zone(
        &run.domain,
        super::run_sessions(run),
        &format!("firm {}", run.firm),
    )?;
    run.facility.services.lock().unwrap().push(id);
    if run.config.git_internal() {
        let handler = Arc::new(GitServer {
            run: Arc::downgrade(run),
        });
        add_service(
            d,
            run,
            format!("git.{}", run.domain),
            443,
            ServiceTarget::Handler { tls: true, handler },
        )?;
    }
    Ok(())
}

/// For tests: `cluster` as the run's cluster, as `start` makes it once it
/// has booted.
#[cfg(test)]
pub(crate) fn test_cluster(d: &Core, run: &FirmRun, cluster: &Arc<Session>) {
    *run.facility.cluster.lock().unwrap() = Some(cluster.clone());
    register_cluster(d, run, cluster).unwrap();
}

/// The cluster's API and the apps it serves, as services of the firm's
/// domain on the cluster VM's ports.
fn register_cluster(d: &impl ServiceApi, run: &FirmRun, cluster: &Session) -> Result<()> {
    let dom = &run.domain;
    let to = |port: u32| ServiceTarget::Session {
        id: cluster.id.clone(),
        port,
    };
    add_service(
        d,
        run,
        format!("{CLUSTER}.{dom}"),
        API_PORT as u16,
        to(API_PORT),
    )?;
    for name in [format!("apps.{dom}"), format!("*.apps.{dom}")] {
        add_service(d, run, name.clone(), 80, to(HTTP_PORT))?;
        add_service(d, run, name, 443, to(HTTPS_PORT))?;
    }
    Ok(())
}

fn plain(status: StatusCode, msg: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("content-type", "text/plain")
        .body(Full::new(Bytes::from(format!("{msg}\n"))))
        .unwrap()
}

/// The repository a git request is for: `/<name>.git/...`.
fn repo_of(path: &str) -> Option<&str> {
    path.strip_prefix('/')?
        .split('/')
        .next()?
        .strip_suffix(".git")
        .filter(|n| super::git::valid_repo_name(n))
}

fn basic_auth(h: &hyper::HeaderMap) -> Option<(String, String)> {
    use base64::Engine as _;
    let v = h
        .get(hyper::header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Basic ")?;
    let raw = base64::engine::general_purpose::STANDARD
        .decode(v.trim())
        .ok()?;
    let s = String::from_utf8(raw).ok()?;
    let (u, p) = s.split_once(':')?;
    Some((u.to_string(), p.to_string()))
}

/// One request to the firm's git server: checks the worker's password and
/// rights, then runs `git http-backend`.
/// Whether `member` may make this git request, and their grant's level:
/// their password, the repository the path names, and their grant on it.
pub(super) fn authorize(
    run: &FirmRun,
    member: &str,
    headers: &hyper::HeaderMap,
    path: &str,
    query: &str,
) -> Result<super::access::Level, Box<Response<Full<Bytes>>>> {
    let Some((user, pass)) = basic_auth(headers) else {
        let mut r = plain(StatusCode::UNAUTHORIZED, "credentials required");
        r.headers_mut()
            .insert("www-authenticate", "Basic realm=\"git\"".parse().unwrap());
        return Err(Box::new(r));
    };
    let ok = user == member
        && run
            .facility
            .git_passwords
            .lock()
            .unwrap()
            .get(member)
            .is_some_and(|p| *p == pass);
    if !ok {
        let mut r = plain(
            StatusCode::UNAUTHORIZED,
            "wrong credentials for this worker",
        );
        r.headers_mut()
            .insert("www-authenticate", "Basic realm=\"git\"".parse().unwrap());
        return Err(Box::new(r));
    }
    let push = path.ends_with("/git-receive-pack") || query.contains("service=git-receive-pack");
    if path.split('/').any(|c| c == "..") {
        return Err(Box::new(plain(StatusCode::BAD_REQUEST, "bad path")));
    }
    let repos = run.config.git_repos();
    let Some(repo) = repo_of(path).filter(|r| repos.iter().any(|x| x == r)) else {
        let urls: Vec<String> = repos
            .iter()
            .map(|r| format!("https://git.{}/{r}.git", run.domain))
            .collect();
        return Err(Box::new(plain(
            StatusCode::NOT_FOUND,
            &format!("no such repository; the firm's: {}", urls.join(", ")),
        )));
    };
    // The worker's grant on this repository, read on every request: a
    // change or revocation applies to the next fetch or push.
    let level = super::access::level(run, member, &format!("git:{repo}"));
    if level == super::access::Level::None {
        return Err(Box::new(plain(
            StatusCode::FORBIDDEN,
            &format!(
                "you have no access to {repo}; ask for it with credentials_request (git:{repo})"
            ),
        )));
    }
    if push && !level.may_push() {
        return Err(Box::new(plain(
            StatusCode::FORBIDDEN,
            &format!("you can read {repo}, not push to it; ask for write with credentials_request (git:{repo})"),
        )));
    }
    Ok(level)
}

pub(crate) async fn git_request(
    run: &Arc<FirmRun>,
    member: &str,
    req: Request<hyper::body::Incoming>,
) -> Response<Full<Bytes>> {
    let path = req.uri().path().to_string();
    let query = req.uri().query().unwrap_or("").to_string();
    let push = path.ends_with("/git-receive-pack") || query.contains("service=git-receive-pack");
    let level = match authorize(run, member, req.headers(), &path, &query) {
        Ok(level) => level,
        Err(refused) => return *refused,
    };
    // A remote's repository is cloned, or fetched, when this run first uses
    // it.
    if let Some(repo) = repo_of(&path) {
        if let Err(e) = super::remote::ensure(run, repo).await {
            return plain(
                StatusCode::SERVICE_UNAVAILABLE,
                &format!("{repo} couldn't be brought from the firm's remote: {e:#}; try again shortly, and tell the board if it persists"),
            );
        }
    }
    let method = req.method().to_string();
    let headers = req.headers().clone();
    let body = match http_body_util::Limited::new(req.into_body(), MAX_GIT_BODY)
        .collect()
        .await
    {
        Ok(b) => b.to_bytes(),
        Err(_) => return plain(StatusCode::PAYLOAD_TOO_LARGE, "push too large"),
    };
    let header = |k: &str| {
        headers
            .get(k)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    };
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("http-backend")
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env(
            "GIT_PROJECT_ROOT",
            super::remote::root(&run.config, &run.dir),
        )
        .env("GIT_HTTP_EXPORT_ALL", "1")
        .env("PATH_INFO", &path)
        .env("QUERY_STRING", &query)
        .env("REQUEST_METHOD", &method)
        .env("CONTENT_TYPE", header("content-type"))
        .env("CONTENT_LENGTH", body.len().to_string())
        .env("HTTP_CONTENT_ENCODING", header("content-encoding"))
        .env("GIT_PROTOCOL", header("git-protocol"))
        .env("REMOTE_USER", member)
        // The pre-receive hook keeps `main` and tags to admin.
        .env("ZAPPER_GIT_LEVEL", level.name())
        .env("REMOTE_ADDR", "127.0.0.1")
        .env("GIT_COMMITTER_NAME", member)
        .env("GIT_COMMITTER_EMAIL", format!("{member}@{}", run.domain))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let Ok(mut child) = cmd.spawn() else {
        return plain(
            StatusCode::INTERNAL_SERVER_ERROR,
            "git http-backend is missing on the host",
        );
    };
    let mut stdin = child.stdin.take().unwrap();
    let feed = tokio::spawn(async move {
        let _ = stdin.write_all(&body).await;
    });
    let out = match child.wait_with_output().await {
        Ok(o) => o,
        Err(e) => return plain(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let _ = feed.await;
    // CGI: headers, a blank line, the body.
    let split = out
        .stdout
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| (i, 4))
        .or_else(|| {
            out.stdout
                .windows(2)
                .position(|w| w == b"\n\n")
                .map(|i| (i, 2))
        });
    let Some((at, sep)) = split else {
        return plain(
            StatusCode::INTERNAL_SERVER_ERROR,
            &String::from_utf8_lossy(&out.stderr),
        );
    };
    let head = String::from_utf8_lossy(&out.stdout[..at]).into_owned();
    let mut resp = Response::builder().status(200);
    for line in head.lines() {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let (k, v) = (k.trim(), v.trim());
        if k.eq_ignore_ascii_case("status") {
            let code = v
                .split_whitespace()
                .next()
                .and_then(|c| c.parse::<u16>().ok())
                .unwrap_or(500);
            resp = resp.status(code);
        } else {
            resp = resp.header(k, v);
        }
    }
    if push && path.ends_with("/git-receive-pack") && out.status.success() {
        run.journal.append("git_push", json!({ "worker": member }));
        if let Some(repo) = repo_of(&path) {
            super::remote::after_push(run, repo);
        }
    }
    resp.body(Full::new(Bytes::copy_from_slice(&out.stdout[at + sep..])))
        .unwrap_or_else(|_| plain(StatusCode::INTERNAL_SERVER_ERROR, "bad CGI response"))
}

/// The repository's `pre-receive` hook, by the pusher's grant
/// (`ZAPPER_GIT_LEVEL`, set by the git handler, as `REMOTE_USER` is):
///
/// - write: branches, never `main`, branches under `main/`, or tags;
/// - merge: `main` only, and only to merge a branch already on the server
///   that someone else pushed and wrote: a fast-forward to its tip, or a
///   merge commit of the old `main` and its tip whose tree is exactly the
///   merge's (nothing added in the merge itself);
/// - admin: anything.
///
/// It records who pushed each commit new to the repository
/// (`zapper-pushed`), which the merge check reads.
pub(super) const PRE_RECEIVE: &str = r#"#!/bin/sh
# Installed by zapper: write pushes branches, merge merges someone else's
# branch into main, admin does anything.
zero=0000000000000000000000000000000000000000
pushed="$GIT_DIR/zapper-pushed"
refuse() { echo "zapper: $*" >&2; exit 1; }

# The branch other than main whose tip is $1, if there is one.
branch_at() {
    git for-each-ref --format='%(objectname) %(refname)' refs/heads/ |
        awk -v sha="$1" '$1 == sha && $2 != "refs/heads/main" { print $2; exit }'
}

# Refuses a merge of old..tip with any commit the merger pushed or wrote.
not_theirs() {
    for c in $(git rev-list "$1..$2"); do
        if [ -f "$pushed" ] && grep -q "^$c $REMOTE_USER\$" "$pushed"; then
            refuse "$REMOTE_USER pushed $c: someone else merges your work (ask for a review)"
        fi
    done
    if git log --format='%an%n%cn' "$1..$2" | grep -qx "$REMOTE_USER"; then
        refuse "$REMOTE_USER wrote commits on this branch: someone else merges your work (ask for a review)"
    fi
}

merge_into_main() {
    old=$1 new=$2
    [ "$old" != "$zero" ] || refuse "main doesn't exist yet: someone with admin starts it"
    [ "$new" != "$zero" ] || refuse "main can't be deleted"
    if [ -n "$(branch_at "$new")" ] && git merge-base --is-ancestor "$old" "$new"; then
        not_theirs "$old" "$new"
        return
    fi
    set -- $(git rev-list --parents -n 1 "$new")
    [ $# -eq 3 ] || refuse "with merge, main moves only to a branch's tip or to a merge of a branch into it"
    [ "$2" = "$old" ] || refuse "a merge's first parent must be main as it is ($old)"
    tip=$3
    [ -n "$(branch_at "$tip")" ] || refuse "merge a branch that is on the server ($tip isn't a branch's tip)"
    merged=$(git merge-tree --write-tree "$old" "$tip" 2>/dev/null) ||
        refuse "the branch doesn't merge cleanly into main (or git is older than 2.38): its author rebases it"
    [ "$(echo "$merged" | head -n 1)" = "$(git rev-parse "$new^{tree}")" ] ||
        refuse "a merge may add nothing of its own: push the merge exactly as git makes it"
    not_theirs "$old" "$tip"
}

while read old new ref; do
    # Who pushed each commit new to the repository, for the merge check.
    if [ "$new" != "$zero" ]; then
        git rev-list "$new" --not --all | sed "s/\$/ $REMOTE_USER/" >> "$pushed"
    fi
    case "$ZAPPER_GIT_LEVEL" in
        admin) ;;
        merge)
            [ "$ref" = refs/heads/main ] ||
                refuse "$REMOTE_USER has merge: they merge into main, and push nothing else ($ref); someone with write pushes branches"
            merge_into_main "$old" "$new" ;;
        *)
            case "$ref" in
                refs/heads/main|refs/heads/main/*|refs/tags/*)
                    refuse "$REMOTE_USER has write access: push a branch; $ref needs merge (into main) or admin (ask with credentials_request)" ;;
            esac ;;
    esac
done
exit 0
"#;

/// The namespace that holds workers' service accounts.
const WORKER_NS: &str = "zapper-workers";

/// Network access to git or the cluster follows the grant: a rule in the
/// worker's own layer allows the host while they hold one.
pub fn network_follows_grant(
    run: &FirmRun,
    worker: &str,
    resource: &str,
    level: super::access::Level,
) {
    let Some(s) = run.rt(worker).and_then(|w| w.session()) else {
        return;
    };
    // One rule per grant (`git:api` → `credentials-git-api`): a revoked
    // repository leaves the others' rules, and so the host, in place.
    let id = format!("credentials-{}", resource.replace(':', "-"));
    if level == super::access::Level::None {
        s.remove_rule(&id);
        return;
    }
    let (host, port) = if resource.starts_with("git:") {
        (format!("git.{}", run.domain), 443)
    } else {
        (format!("{CLUSTER}.{}", run.domain), API_PORT as u16)
    };
    s.put_rule(zapper_policy::AnyRule::Net(zapper_policy::NetRule {
        id: Some(id),
        action: zapper_policy::Action::Allow,
        host,
        ports: vec![port],
        methods: vec![],
        path: None,
        protocol: Default::default(),
        secrets: vec![],
        set_headers: Default::default(),
        final_: false,
        comment: Some(format!("credentials: {resource} {}", level.name())),
    }));
}

/// A worker's access to the cluster, as their grant says: their own
/// service account with RoleBindings (read: `view` in dev; write: `edit` in
/// dev; admin: also `admin` in prod), and a kubeconfig in their VM with its
/// token. No grant: no account, no kubeconfig. Kubernetes enforces it.
pub async fn cluster_access(
    _d: &Core,
    run: &FirmRun,
    worker: &str,
    level: super::access::Level,
    expires: Option<f64>,
) -> Result<()> {
    use super::access::Level;
    let Some(c) = run.facility.cluster.lock().unwrap().clone() else {
        return Ok(());
    };
    let sa = format!("w-{worker}");
    let bindings: Vec<(&str, &str)> = match level {
        Level::None => vec![],
        // Merge is git's alone and never granted on the cluster: if it were,
        // it would be no more than read.
        Level::Read | Level::Merge => vec![("dev", "view")],
        Level::Write => vec![("dev", "edit")],
        Level::Admin => vec![("dev", "edit"), ("prod", "admin")],
    };
    let mut script = format!("k3s kubectl delete rolebinding -A -l zapper/worker={worker} --ignore-not-found >/dev/null; ");
    if level == Level::None {
        script.push_str(&format!(
            "k3s kubectl -n {WORKER_NS} delete serviceaccount {sa} --ignore-not-found >/dev/null"
        ));
        sh(&c, &script).await?;
        if let Some(s) = run.rt(worker).and_then(|w| w.session()) {
            let home = format!("/home/{}", s.resolved.agent.user);
            let _ = sh(&s, &format!("rm -f {home}/.kube/config")).await;
        }
        return Ok(());
    }
    script.push_str(&format!("k3s kubectl -n {WORKER_NS} create serviceaccount {sa} --dry-run=client -o yaml | k3s kubectl apply -f - >/dev/null; "));
    for (ns, role) in &bindings {
        script.push_str(&format!(
            "k3s kubectl -n {ns} create rolebinding {sa}-{role} --clusterrole={role} --serviceaccount={WORKER_NS}:{sa} && k3s kubectl -n {ns} label rolebinding {sa}-{role} zapper/worker={worker} >/dev/null; "
        ));
    }
    // The token lives as long as the grant (the owners': 30 days); deleting
    // the account ends it sooner.
    let secs = expires
        .map(|e| (e - super::now()).max(600.0) as u64)
        .unwrap_or(30 * 86400);
    script.push_str(&format!(
        "k3s kubectl -n {WORKER_NS} create token {sa} --duration={secs}s"
    ));
    let (code, out) = sh(&c, &script).await?;
    if code != 0 {
        bail!("RBAC for {worker}: {out}");
    }
    let token = out.lines().last().unwrap_or("").trim().to_string();
    let ca = run
        .facility
        .kubeconfig
        .lock()
        .unwrap()
        .as_deref()
        .and_then(|k| {
            k.lines().find_map(|l| {
                l.trim()
                    .strip_prefix("certificate-authority-data:")
                    .map(|v| v.trim().to_string())
            })
        })
        .ok_or_else(|| anyhow!("no cluster CA yet"))?;
    let kube = format!(
        "apiVersion: v1\nkind: Config\nclusters:\n- name: {CLUSTER}\n  cluster:\n    server: https://{CLUSTER}.{d}:{API_PORT}\n    certificate-authority-data: {ca}\nusers:\n- name: {worker}\n  user:\n    token: {token}\ncontexts:\n- name: {worker}\n  context:\n    cluster: {CLUSTER}\n    user: {worker}\n    namespace: dev\ncurrent-context: {worker}\n",
        d = run.domain
    );
    if let Some(s) = run.rt(worker).and_then(|w| w.session()) {
        let home = format!("/home/{}", s.resolved.agent.user);
        let mut out = Vec::new();
        let (code, err) = drive::guest_command(
            &s,
            vec!["sh".into(), "-c".into(), format!("mkdir -p {home}/.kube && cat > {home}/.kube/config && chmod 600 {home}/.kube/config && chown -R 1000:1000 {home}/.kube")],
            Some(std::io::Cursor::new(kube.into_bytes())),
            &mut out,
            1 << 16,
        )
        .await?;
        if code != 0 {
            bail!("writing {worker}'s kubeconfig: {err}");
        }
    }
    Ok(())
}

/// Runs a shell command as root in a session's VM; its stdout.
pub(super) async fn sh(s: &Session, script: &str) -> Result<(i32, String)> {
    let mut out = Vec::new();
    let (code, err) = drive::guest_command(
        s,
        vec!["sh".into(), "-c".into(), script.into()],
        None::<tokio::io::Empty>,
        &mut out,
        4 << 20,
    )
    .await?;
    let mut text = String::from_utf8_lossy(&out).into_owned();
    if code != 0 {
        text.push_str(&err);
    }
    Ok((code, text))
}

pub async fn start(d: &Core, run: &Arc<FirmRun>, config: &Config) -> Result<()> {
    let repos = run.config.git_repos();
    if run.config.git_remote().is_some() {
        // The repositories are the remote's, kept in git.dir across runs.
        super::remote::prepare(run).await?;
        tokio::spawn(super::remote::watch(run.clone()));
    }
    if !repos.is_empty() {
        for name in repos.iter().filter(|_| run.config.git_remote().is_none()) {
            let repo = run.dir.join("git").join(format!("{name}.git"));
            std::fs::create_dir_all(&repo)?;
            let ok = tokio::process::Command::new("git")
                .args(["init", "--quiet", "--bare", "-b", "main"])
                .arg(&repo)
                .status()
                .await
                .context("git init (is git installed?)")?;
            if !ok.success() {
                bail!("git init {name} failed");
            }
            let _ = tokio::process::Command::new("git")
                .arg("--git-dir")
                .arg(&repo)
                .args(["config", "http.receivepack", "true"])
                .status()
                .await;
            // Below admin, pushes may not touch `main` or tags. The hook is
            // the daemon's: the repository lives on the host, out of
            // workers' reach.
            let hook = repo.join("hooks").join("pre-receive");
            std::fs::create_dir_all(repo.join("hooks"))?;
            std::fs::write(&hook, PRE_RECEIVE)?;
            std::fs::set_permissions(&hook, std::os::unix::fs::PermissionsExt::from_mode(0o755))?;
        }
        // An empty knowledge base gets its layout.
        super::kb::seed(run).await?;
        // A password per worker is who they are, not what they may do: their
        // grant decides that.
        let mut pw = run.facility.git_passwords.lock().unwrap();
        for w in run.rts() {
            pw.insert(w.name.clone(), random_id(24));
        }
        run.journal.append(
            "facility",
            json!({ "name": "git", "state": "ready", "repos": repos }),
        );
    }
    let Some((name, f)) = run.config.facilities.iter().next() else {
        return Ok(());
    };
    run.journal
        .append("facility", json!({ "name": name, "state": "booting" }));
    let forwards = [API_PORT, HTTP_PORT, HTTPS_PORT, DASHBOARD_PORT]
        .iter()
        .map(|p| Forward {
            vsock_port: *p,
            tcp_port: *p as u16,
            nic: true,
        })
        .collect();
    let new = NewSession {
        profile: f.profile.clone(),
        workdir: run.dir.join("empty"),
        policy: Policy::default(),
        labels: super::labels(&run.firm, &run.id, CLUSTER, false),
        shared_layers: vec![],
        files: vec![],
        mounts: vec![],
        // For the operator's `zapper exec --system`: kubectl as k3s's admin.
        agent_overlay: Some(json!({ "env": { "KUBECONFIG": "/etc/rancher/k3s/k3s.yaml" } })),
        forwards,
    };
    let _ = config;
    let s = d.create_session(new).await.context("booting the cluster")?;
    *run.facility.cluster.lock().unwrap() = Some(s.clone());
    register_cluster(d, run, &s)?;
    run.journal.append(
        "facility",
        json!({ "name": name, "state": "starting", "session": s.id }),
    );
    let san = format!("{CLUSTER}.{}", run.domain);
    let script = format!(
        "mkdir -p /var/log && nohup k3s server --node-name {CLUSTER} --tls-san {san} --disable metrics-server --flannel-backend host-gw --write-kubeconfig-mode 600 > /var/log/k3s.log 2>&1 &"
    );
    let (code, out) = sh(&s, &script).await?;
    if code != 0 {
        bail!("starting k3s: {out}");
    }
    // The API first; the workers' kubeconfig comes from it.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(300);
    loop {
        let (code, out) = sh(&s, "k3s kubectl get --raw=/readyz 2>&1")
            .await
            .unwrap_or((1, String::new()));
        if code == 0 && out.trim() == "ok" {
            break;
        }
        if tokio::time::Instant::now() > deadline {
            let (_, log) = sh(&s, "tail -30 /var/log/k3s.log")
                .await
                .unwrap_or_default();
            bail!("k3s did not become ready in 5 minutes; its log ends:\n{log}");
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    let (code, kube) = sh(&s, "cat /etc/rancher/k3s/k3s.yaml").await?;
    if code != 0 {
        bail!("reading the kubeconfig: {kube}");
    }
    let kube = kube.replace("https://127.0.0.1:6443", &format!("https://{san}:6443"));
    *run.facility.kubeconfig.lock().unwrap() = Some(kube);
    // The firm's two namespaces (prod is what the ingress serves), and the
    // one that holds workers' service accounts.
    let (code, out) = sh(&s, &format!("for n in dev prod {WORKER_NS}; do k3s kubectl create namespace $n --dry-run=client -o yaml | k3s kubectl apply -f - >/dev/null || exit 1; done")).await?;
    if code != 0 {
        bail!("creating the namespaces: {out}");
    }
    run.journal.append(
        "facility",
        json!({ "name": name, "state": "ready", "session": s.id }),
    );
    if let Some(l) = &run.config.landing {
        // In the background: Traefik comes up after the API.
        let manifest = landing_manifest(&l.display_name(&run.firm), &l.motto);
        let (run, s) = (run.clone(), s.clone());
        tokio::spawn(async move { deploy_landing(&run, &s, manifest).await });
    }
    if let Some(addr) = &run.config.product_port {
        let task = serve_product(run, addr).await?;
        *run.facility.product.lock().unwrap() = Some(task);
        run.journal.append(
            "facility",
            json!({ "name": "product", "state": "ready", "address": addr }),
        );
    }
    if let Some(addr) = &run.config.dashboard_port {
        let task = forward_port(run, addr, DASHBOARD_PORT, "dashboard").await?;
        if addr
            .parse::<std::net::SocketAddr>()
            .is_ok_and(|a| !a.ip().is_loopback())
        {
            eprintln!(
                "{}",
                zapper_core::util::exposed_warning(&format!(
                    "firm {}'s Kubernetes dashboard (dashboard_port) listens on {addr}",
                    run.firm
                ))
            );
        }
        *run.facility.dashboard.lock().unwrap() = Some(task);
        let (run, s) = (run.clone(), s.clone());
        tokio::spawn(async move { deploy_dashboard(&run, &s).await });
    }
    Ok(())
}

/// The product port: an HTTP proxy to the cluster's ingress, which asks for
/// `apps.<domain>` as workers do (see `product`).
async fn serve_product(run: &Arc<FirmRun>, addr: &str) -> Result<JoinHandle<()>> {
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("product_port {addr}"))?;
    let run = run.clone();
    Ok(tokio::spawn(async move {
        loop {
            let Ok((conn, peer)) = listener.accept().await else {
                break;
            };
            tracing::debug!(firm = %run.firm, "product port: connection from {peer}");
            tokio::spawn(super::product::serve(run.clone(), conn, HTTP_PORT));
        }
    }))
}

/// Forwards a local address to a port of the cluster VM, for the board.
async fn forward_port(
    run: &Arc<FirmRun>,
    addr: &str,
    port: u32,
    what: &'static str,
) -> Result<JoinHandle<()>> {
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("{what}_port {addr}"))?;
    let run = run.clone();
    Ok(tokio::spawn(async move {
        loop {
            let Ok((mut conn, peer)) = listener.accept().await else {
                break;
            };
            let run = run.clone();
            tokio::spawn(async move {
                let Some(c) = run.facility.cluster.lock().unwrap().clone() else {
                    return;
                };
                tracing::debug!(firm = %run.firm, "{what} port: connection from {peer}");
                if let Ok(mut up) = vsock_connect(&c, port).await {
                    let _ = tokio::io::copy_bidirectional(&mut conn, &mut up).await;
                }
            });
        }
    }))
}

/// Headlamp in namespace `dashboard`, from the image's air-gap bundle, on a
/// NodePort that only the board's port reaches; and a cluster-admin token for
/// the board to sign in with.
fn dashboard_manifest() -> String {
    let labels = json!({ "app": "headlamp" });
    let list = json!({
        "apiVersion": "v1", "kind": "List", "items": [
            { "apiVersion": "v1", "kind": "Namespace", "metadata": { "name": "dashboard" } },
            { "apiVersion": "v1", "kind": "ServiceAccount", "metadata": { "name": "board", "namespace": "dashboard" } },
            { "apiVersion": "rbac.authorization.k8s.io/v1", "kind": "ClusterRoleBinding", "metadata": { "name": "dashboard-board" },
              "roleRef": { "apiGroup": "rbac.authorization.k8s.io", "kind": "ClusterRole", "name": "cluster-admin" },
              "subjects": [{ "kind": "ServiceAccount", "name": "board", "namespace": "dashboard" }] },
            { "apiVersion": "v1", "kind": "Secret", "type": "kubernetes.io/service-account-token",
              "metadata": { "name": "board-token", "namespace": "dashboard", "annotations": { "kubernetes.io/service-account.name": "board" } } },
            { "apiVersion": "apps/v1", "kind": "Deployment", "metadata": { "name": "headlamp", "namespace": "dashboard" }, "spec": {
                "selector": { "matchLabels": labels }, "template": { "metadata": { "labels": labels }, "spec": {
                    "containers": [{ "name": "headlamp", "image": HEADLAMP_IMAGE, "imagePullPolicy": "IfNotPresent",
                        "args": ["-in-cluster"], "ports": [{ "containerPort": 4466 }] }] } } } },
            { "apiVersion": "v1", "kind": "Service", "metadata": { "name": "headlamp", "namespace": "dashboard" },
              "spec": { "type": "NodePort", "selector": labels, "ports": [{ "port": 80, "targetPort": 4466, "nodePort": DASHBOARD_PORT }] } },
        ]
    });
    serde_json::to_string(&list).unwrap()
}

async fn deploy_dashboard(run: &FirmRun, s: &Session) {
    let manifest = dashboard_manifest();
    for _ in 0..60 {
        if run.ended() {
            return;
        }
        let mut out = Vec::new();
        let input = std::io::Cursor::new(manifest.clone().into_bytes());
        let applied = drive::guest_command(
            s,
            vec![
                "sh".into(),
                "-c".into(),
                "cat > /tmp/dashboard.json && k3s kubectl apply -f /tmp/dashboard.json".into(),
            ],
            Some(input),
            &mut out,
            1 << 20,
        )
        .await;
        if let Ok((0, _)) = applied {
            break;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    // The token appears once the controller fills the secret.
    for _ in 0..60 {
        if run.ended() {
            return;
        }
        if let Ok((0, t)) = sh(s, "k3s kubectl -n dashboard get secret board-token -o jsonpath='{.data.token}' | base64 -d").await {
            let t = t.trim().to_string();
            if t.len() > 20 {
                *run.facility.dashboard_token.lock().unwrap() = Some(t);
                run.journal.append("facility", json!({ "name": "dashboard", "state": "ready", "address": run.config.dashboard_port }));
                return;
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    run.journal.append(
        "facility",
        json!({ "name": "dashboard", "state": "failed" }),
    );
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// The landing page, in namespace `landing`, with the firm's mark (a camera
/// iris: eight shutter blades around an opening): busybox's httpd from the
/// cluster's air-gap images serving one page, behind an Ingress on `/` at
/// Traefik's lowest priority, so any Ingress the firm adds on `/` wins.
pub fn landing_manifest(name: &str, motto: &str) -> String {
    let (n, m) = (html_escape(name), html_escape(motto));
    let page = format!(
        r##"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><title>{n}</title>
<style>
  body {{ margin:0; min-height:100vh; display:flex; align-items:center; justify-content:center; background:#F5F4EE; color:#3D3929; font:18px/1.6 ui-sans-serif, system-ui, sans-serif; }}
  main {{ text-align:center; padding:48px 24px; }}
  svg {{ width:96px; height:96px; }}
  h1 {{ font:500 56px/1.1 ui-serif, Georgia, serif; margin:24px 0 12px; letter-spacing:-0.5px; }}
  p {{ margin:0; color:#D97757; font-size:22px; }}
  small {{ display:block; margin-top:48px; color:#8A8577; font-size:13px; }}
  @media (prefers-color-scheme: dark) {{ body {{ background:#262624; color:#F5F4EE; }} small {{ color:#A6A196; }} }}
</style></head>
<body><main>
<svg viewBox="0 0 64 64" aria-label="logo"><g fill="none" stroke="#D97757" stroke-width="3" stroke-linejoin="round" stroke-linecap="round"><circle cx="32" cy="32" r="30"/><path d="M42.16 36.21L36.21 42.16L27.79 42.16L21.84 36.21L21.84 27.79L27.79 21.84L36.21 21.84L42.16 27.79Z"/><path d="M42.16 36.21L42.16 60.23M36.21 42.16L19.23 59.15M27.79 42.16L3.77 42.16M21.84 36.21L4.85 19.23M21.84 27.79L21.84 3.77M27.79 21.84L44.77 4.85M36.21 21.84L60.23 21.84M42.16 27.79L59.15 44.77"/></g></svg>
<h1>{n}</h1>
<p>{m}</p>
<small>Something new is being built here.</small>
</main></body></html>
"##
    );
    let labels = json!({ "app": "landing" });
    let list = json!({
        "apiVersion": "v1", "kind": "List", "items": [
            { "apiVersion": "v1", "kind": "Namespace", "metadata": { "name": "landing" } },
            { "apiVersion": "v1", "kind": "ConfigMap", "metadata": { "name": "landing", "namespace": "landing" }, "data": { "index.html": page } },
            { "apiVersion": "apps/v1", "kind": "Deployment", "metadata": { "name": "landing", "namespace": "landing" }, "spec": {
                "selector": { "matchLabels": labels }, "template": { "metadata": { "labels": labels }, "spec": {
                    "containers": [{ "name": "httpd", "image": "rancher/mirrored-library-busybox:1.37.0", "imagePullPolicy": "IfNotPresent",
                        "command": ["httpd", "-f", "-p", "8080", "-h", "/www"], "volumeMounts": [{ "name": "page", "mountPath": "/www" }] }],
                    "volumes": [{ "name": "page", "configMap": { "name": "landing" } }] } } } },
            { "apiVersion": "v1", "kind": "Service", "metadata": { "name": "landing", "namespace": "landing" }, "spec": { "selector": labels, "ports": [{ "port": 80, "targetPort": 8080 }] } },
            { "apiVersion": "networking.k8s.io/v1", "kind": "Ingress", "metadata": { "name": "landing", "namespace": "landing",
                "annotations": { "traefik.ingress.kubernetes.io/router.priority": "1" } }, "spec": { "rules": [{ "http": { "paths": [
                    { "path": "/", "pathType": "Prefix", "backend": { "service": { "name": "landing", "port": { "number": 80 } } } }] } }] } },
        ]
    });
    serde_json::to_string(&list).unwrap()
}

async fn deploy_landing(run: &FirmRun, s: &Session, manifest: String) {
    for _ in 0..40 {
        if run.ended() {
            return;
        }
        let mut out = Vec::new();
        let input = std::io::Cursor::new(manifest.clone().into_bytes());
        let r = drive::guest_command(
            s,
            vec![
                "sh".into(),
                "-c".into(),
                "cat > /tmp/landing.json && k3s kubectl apply -f /tmp/landing.json".into(),
            ],
            Some(input),
            &mut out,
            1 << 20,
        )
        .await;
        if let Ok((0, _)) = r {
            run.journal.append(
                "facility",
                json!({ "name": "landing", "state": "deployed" }),
            );
            return;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    run.journal
        .append("facility", json!({ "name": "landing", "state": "failed" }));
}

pub async fn stop(d: &Core, run: &Arc<FirmRun>) {
    for t in [
        run.facility.product.lock().unwrap().take(),
        run.facility.dashboard.lock().unwrap().take(),
    ]
    .into_iter()
    .flatten()
    {
        t.abort();
    }
    let services: Vec<u64> = run.facility.services.lock().unwrap().drain(..).collect();
    for id in services {
        d.remove_service(id);
    }
    let cluster = run.facility.cluster.lock().unwrap().take();
    if let Some(s) = cluster {
        d.stop_session(&s, "firm down").await;
        let _ = d.forget_session(&s.id);
    }
}

/// The commits of a run's repositories, newest first, each with its
/// repository.
pub async fn git_log(run_dir: &Path) -> Value {
    let mut commits: Vec<Value> = Vec::new();
    for name in super::git::repos(run_dir) {
        let repo = super::git::repo_dir(run_dir, &name);
        // A run whose repositories outlive it shows what it added: from its
        // branches at the end (or now) back to where they were at its start.
        let shas = |when: &str| -> Vec<String> {
            super::git::recorded(run_dir, &name, when)
                .map(|m| {
                    m.values()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default()
        };
        let mut range: Vec<String> = match super::git::recorded(run_dir, &name, "end") {
            Some(_) => shas("end"),
            None => vec!["--branches".into()],
        };
        if super::git::recorded(run_dir, &name, "start").is_some() {
            range.push("--not".into());
            range.extend(shas("start"));
        } else if range == ["--branches"] {
            range = vec!["--all".into()];
        }
        let out = tokio::process::Command::new("git")
            .arg("--git-dir")
            .arg(&repo)
            .args([
                "log",
                "--date=unix",
                "--max-count=500",
                "--pretty=format:%H%x1f%an%x1f%ad%x1f%s%x1f%D",
            ])
            .args(&range)
            .output()
            .await;
        let Ok(out) = out else { continue };
        commits.extend(String::from_utf8_lossy(&out.stdout).lines().filter_map(|l| {
            let f: Vec<&str> = l.split('\x1f').collect();
            (f.len() >= 4).then(|| json!({ "repo": name, "commit": f[0], "author": f[1], "ts": f[2].parse::<f64>().unwrap_or(0.0), "subject": f[3], "refs": f.get(4).unwrap_or(&"") }))
        }));
    }
    commits.sort_by(|a, b| {
        b["ts"]
            .as_f64()
            .partial_cmp(&a["ts"].as_f64())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    commits.truncate(500);
    json!(commits)
}

pub fn services_for(run: &FirmRun, _role: &str) -> Services {
    let d = &run.domain;
    // Shown to everyone: access to each comes with a grant.
    let kube = !run.config.facilities.is_empty();
    let cluster = run.facility.cluster.lock().unwrap().is_some();
    Services {
        git: run
            .config
            .git_repos()
            .iter()
            .map(|r| format!("https://git.{d}/{r}.git"))
            .collect(),
        remote_main: run
            .config
            .git_repos()
            .into_iter()
            .filter(|r| run.config.git_remote().is_some() && !run.config.main_is_local(r))
            .collect(),
        local_main: run
            .config
            .git_repos()
            .into_iter()
            .filter(|r| run.config.git_remote().is_some() && run.config.main_is_local(r))
            .collect(),
        kube: kube.then(|| format!("https://{CLUSTER}.{d}:{API_PORT}")),
        product: cluster.then(|| format!("http://apps.{d}")),
    }
}

/// Files that give a worker its credentials for the firm's services.
pub fn credentials_for(
    run: &FirmRun,
    worker: &str,
    _role: &str,
    home: &str,
) -> Result<Vec<GuestFile>> {
    // The kubeconfig comes with a cluster grant (cluster_access); the git
    // password is the worker's identity, and their grant decides the rest.
    let mut files = Vec::new();
    if let Some(pw) = run.facility.git_passwords.lock().unwrap().get(worker) {
        let d = &run.domain;
        files.push(GuestFile {
            path: format!("{home}/.git-credentials"),
            content: format!("https://{worker}:{pw}@git.{d}\n"),
            mode: 0o600,
            user_owned: true,
        });
        files.push(GuestFile {
            path: format!("{home}/.gitconfig"),
            content: format!("[user]\n\tname = {worker}\n\temail = {worker}@{d}\n[credential]\n\thelper = store\n[init]\n\tdefaultBranch = main\n[push]\n\tautoSetupRemote = true\n"),
            mode: 0o644,
            user_owned: true,
        });
    }
    Ok(files)
}

/// Whether namespace prod runs anything (the firm has shipped): what
/// `break_cluster: prod` waits for.
pub async fn prod_has_workload(run: &FirmRun) -> Result<bool> {
    let s = run
        .facility
        .cluster
        .lock()
        .unwrap()
        .clone()
        .ok_or_else(|| anyhow!("the cluster isn't running"))?;
    let (code, out) = sh(
        &s,
        "k3s kubectl --request-timeout=10s -n prod get deploy,sts,ds -o name 2>&1",
    )
    .await?;
    if code != 0 {
        bail!("looking at prod: {}", out.trim());
    }
    Ok(!out.trim().is_empty())
}

/// Breaks the cluster on purpose, for a scenario: `prod` deletes the
/// workloads of namespace prod (redeploying fixes it), `api` freezes the
/// API server (resume it with `zapper exec --system`: kill -CONT its
/// process). Its output.
pub async fn break_cluster(run: &FirmRun, mode: &str) -> Result<String> {
    let s = run
        .facility
        .cluster
        .lock()
        .unwrap()
        .clone()
        .ok_or_else(|| anyhow!("the cluster isn't running"))?;
    let script = match mode {
        "prod" => "k3s kubectl -n prod delete deploy,sts,ds,job,cronjob,ingress,svc --all --ignore-not-found 2>&1",
        "api" => "for d in /proc/[0-9]*; do tr '\\0' ' ' < $d/cmdline 2>/dev/null | grep -q '^k3s server' && kill -STOP ${d#/proc/}; done; echo frozen",
        other => bail!("break_cluster mode {other}: prod or api"),
    };
    let (code, out) = sh(&s, script).await?;
    if code != 0 {
        bail!("breaking the cluster: {out}");
    }
    Ok(out.trim().chars().take(500).collect())
}

#[cfg(test)]
mod git_tests;

#[cfg(test)]
mod tests {
    #[test]
    fn git_requests_name_their_repository() {
        use super::repo_of;
        assert_eq!(repo_of("/product.git/info/refs"), Some("product"));
        assert_eq!(repo_of("/product2.git/git-receive-pack"), Some("product2"));
        assert_eq!(repo_of("/product/info/refs"), None, "the .git suffix");
        // As a remote may spell them; but no hidden or path-like names.
        assert_eq!(repo_of("/My_Repo.v2.git/info/refs"), Some("My_Repo.v2"));
        assert_eq!(repo_of("/.git/info/refs"), None);
        assert_eq!(repo_of("/.hidden.git/info/refs"), None);
        assert_eq!(repo_of("/a b.git/info/refs"), None);
        assert_eq!(repo_of(""), None);
    }

    #[test]
    fn landing_escapes_text() {
        let m = super::landing_manifest("A <b>&</b>", "it's \"new\"");
        let v: serde_json::Value = serde_json::from_str(&m).unwrap();
        let page = v["items"][1]["data"]["index.html"].as_str().unwrap();
        assert!(
            page.contains("<h1>A &lt;b&gt;&amp;&lt;/b&gt;</h1>"),
            "{page}"
        );
        assert!(page.contains("it&#39;s &quot;new&quot;"));
        assert_eq!(
            v["items"][4]["metadata"]["annotations"]
                ["traefik.ingress.kubernetes.io/router.priority"],
            "1"
        );
    }

    /// A fake `ServiceApi` recording what was registered — enough to test the
    /// firm's service wiring without the daemon's real service registry.
    #[derive(Default)]
    struct FakeServices {
        zones: std::sync::Mutex<Vec<String>>,
        services: std::sync::Mutex<Vec<(String, u16)>>,
    }

    impl zapper_core::ServiceApi for FakeServices {
        fn add_service(&self, spec: zapper_core::ServiceSpec) -> anyhow::Result<u64> {
            self.services.lock().unwrap().push((spec.name, spec.port));
            Ok(0)
        }
        fn add_zone(
            &self,
            domain: &str,
            _sessions: std::collections::BTreeMap<String, String>,
            _owner: &str,
        ) -> anyhow::Result<u64> {
            self.zones.lock().unwrap().push(domain.to_string());
            Ok(0)
        }
        fn remove_service(&self, _id: u64) -> Option<zapper_core::ServiceInfo> {
            None
        }
        fn services(&self) -> Vec<zapper_core::ServiceInfo> {
            vec![]
        }
        fn add_mcp_server(
            &self,
            _name: &str,
            _sessions: std::collections::BTreeMap<String, String>,
            _server: std::sync::Arc<dyn zapper_core::McpServer>,
        ) -> anyhow::Result<u64> {
            Ok(0)
        }
        fn remove_mcp_server(&self, _id: u64) -> bool {
            false
        }
    }

    const FIRM: &str = "
profiles:
  w:
    agent: { kind: claude-code }
firms:
  acme:
    roles:
      ceo: { profile: w, agent: ceo }
    workers:
      - { name: ceo, role: ceo }
    git: { repos: [api] }
";

    /// `register` depends only on `ServiceApi`: a fake stands in for the real
    /// registry, and we check it wired up the firm's zone and git service.
    #[tokio::test]
    async fn register_wires_the_firms_zone_and_git_service() {
        let t = crate::test_core(FIRM);
        let run = crate::test_run(&t.core, "acme");
        let fake = FakeServices::default();
        super::register(&fake, &run).unwrap();
        assert!(
            fake.zones.lock().unwrap().contains(&run.domain),
            "zone {} not registered: {:?}",
            run.domain,
            fake.zones.lock().unwrap(),
        );
        let git = format!("git.{}", run.domain);
        assert!(
            fake.services
                .lock()
                .unwrap()
                .iter()
                .any(|(name, port)| *name == git && *port == 443),
            "git service {git} missing: {:?}",
            fake.services.lock().unwrap(),
        );
    }
}
