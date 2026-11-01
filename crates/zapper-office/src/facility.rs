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
use tokio::net::UnixStream;
use tokio::task::JoinHandle;
use zapper_policy::proto::{Forward, GuestFile};
use zapper_policy::{Config, Policy};

use super::prompts::Services;
use super::FirmRun;
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

fn add_service(run: &FirmRun, name: String, port: u16, target: ServiceTarget) -> Result<()> {
    let id = run.client.add_service(ServiceSpec {
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
pub fn register(run: &Arc<FirmRun>) -> Result<()> {
    let id = run.client.add_zone(
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
pub(crate) fn test_cluster(run: &FirmRun, cluster: &Arc<Session>) {
    *run.facility.cluster.lock().unwrap() = Some(cluster.clone());
    register_cluster(run, cluster).unwrap();
}

/// The cluster's API and the apps it serves, as services of the firm's
/// domain on the cluster VM's ports.
fn register_cluster(run: &FirmRun, cluster: &Session) -> Result<()> {
    let dom = &run.domain;
    let to = |port: u32| ServiceTarget::Session {
        id: cluster.id.clone(),
        port,
    };
    add_service(
        run,
        format!("{CLUSTER}.{dom}"),
        API_PORT as u16,
        to(API_PORT),
    )?;
    for name in [format!("apps.{dom}"), format!("*.apps.{dom}")] {
        add_service(run, name.clone(), 80, to(HTTP_PORT))?;
        add_service(run, name, 443, to(HTTPS_PORT))?;
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
    let headers = req.headers().clone();
    let method = req.method().clone();
    let body = match http_body_util::Limited::new(req.into_body(), MAX_GIT_BODY)
        .collect()
        .await
    {
        Ok(b) => b.to_bytes(),
        Err(_) => return plain(StatusCode::PAYLOAD_TOO_LARGE, "push too large"),
    };
    let (resp, pushed) = zapper_git::http_backend(zapper_git::Request {
        root: &super::remote::root(&run.config, &run.dir),
        user: member,
        email: &format!("{member}@{}", run.domain),
        // The pre-receive hook keeps `main` and tags to admin.
        level: level.name(),
        method: method.as_str(),
        path: &path,
        query: &query,
        headers: &headers,
        body,
    })
    .await;
    if push && path.ends_with("/git-receive-pack") && pushed {
        run.journal.append("git_push", json!({ "worker": member }));
        if let Some(repo) = repo_of(&path) {
            super::remote::after_push(run, repo);
        }
    }
    resp
}

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
        sh(run, &c.id, &script).await?;
        if let Some(s) = run.rt(worker).and_then(|w| w.session()) {
            let home = format!("/home/{}", s.resolved.agent.user);
            let _ = sh(run, &s.id, &format!("rm -f {home}/.kube/config")).await;
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
    let (code, out) = sh(run, &c.id, &script).await?;
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
        let (code, _out, err) = crate::client::run_command(
            run.client.as_ref(),
            &s.id,
            vec!["sh".into(), "-c".into(), format!("mkdir -p {home}/.kube && cat > {home}/.kube/config && chmod 600 {home}/.kube/config && chown -R 1000:1000 {home}/.kube")],
            Some(kube.into_bytes()),
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
/// Runs a shell command in session `id` over the office's client; its exit
/// code and output (stdout, then stderr if it failed).
pub(super) async fn sh(run: &FirmRun, id: &str, script: &str) -> Result<(i32, String)> {
    let (code, out, err) = crate::client::run_command(
        run.client.as_ref(),
        id,
        vec!["sh".into(), "-c".into(), script.into()],
        None,
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
            std::fs::write(&hook, zapper_git::PRE_RECEIVE)?;
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
        labels: super::labels(&run.firm, &run.id, CLUSTER, "", false),
        shared_layers: vec![],
        files: vec![],
        mounts: vec![],
        // For the operator's `zapper exec --system`: kubectl as k3s's admin.
        agent_overlay: Some(json!({ "env": { "KUBECONFIG": "/etc/rancher/k3s/k3s.yaml" } })),
        forwards,
        persistent: false,
        accept_changed_image: false,
        ssh: false,
    };
    let _ = config;
    let s = d.create_session(new).await.context("booting the cluster")?;
    *run.facility.cluster.lock().unwrap() = Some(s.clone());
    register_cluster(run, &s)?;
    run.journal.append(
        "facility",
        json!({ "name": name, "state": "starting", "session": s.id }),
    );
    let san = format!("{CLUSTER}.{}", run.domain);
    let script = format!(
        "mkdir -p /var/log && nohup k3s server --node-name {CLUSTER} --tls-san {san} --disable metrics-server --flannel-backend host-gw --write-kubeconfig-mode 600 > /var/log/k3s.log 2>&1 &"
    );
    let (code, out) = sh(run, &s.id, &script).await?;
    if code != 0 {
        bail!("starting k3s: {out}");
    }
    // The API first; the workers' kubeconfig comes from it.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(300);
    loop {
        let (code, out) = sh(run, &s.id, "k3s kubectl get --raw=/readyz 2>&1")
            .await
            .unwrap_or((1, String::new()));
        if code == 0 && out.trim() == "ok" {
            break;
        }
        if tokio::time::Instant::now() > deadline {
            let (_, log) = sh(run, &s.id, "tail -30 /var/log/k3s.log")
                .await
                .unwrap_or_default();
            bail!("k3s did not become ready in 5 minutes; its log ends:\n{log}");
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    let (code, kube) = sh(run, &s.id, "cat /etc/rancher/k3s/k3s.yaml").await?;
    if code != 0 {
        bail!("reading the kubeconfig: {kube}");
    }
    let kube = kube.replace("https://127.0.0.1:6443", &format!("https://{san}:6443"));
    *run.facility.kubeconfig.lock().unwrap() = Some(kube);
    // The firm's two namespaces (prod is what the ingress serves), and the
    // one that holds workers' service accounts.
    let (code, out) = sh(run, &s.id, &format!("for n in dev prod {WORKER_NS}; do k3s kubectl create namespace $n --dry-run=client -o yaml | k3s kubectl apply -f - >/dev/null || exit 1; done")).await?;
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
        let (run, id) = (run.clone(), s.id.clone());
        tokio::spawn(async move { deploy_landing(&run, &id, manifest).await });
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
        let (run, id) = (run.clone(), s.id.clone());
        tokio::spawn(async move { deploy_dashboard(&run, &id).await });
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

async fn deploy_dashboard(run: &FirmRun, cluster_id: &str) {
    let manifest = dashboard_manifest();
    for _ in 0..60 {
        if run.ended() {
            return;
        }
        let applied = crate::client::run_command(
            run.client.as_ref(),
            cluster_id,
            vec![
                "sh".into(),
                "-c".into(),
                "cat > /tmp/dashboard.json && k3s kubectl apply -f /tmp/dashboard.json".into(),
            ],
            Some(manifest.clone().into_bytes()),
            1 << 20,
        )
        .await;
        if let Ok((0, _, _)) = applied {
            break;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    // The token appears once the controller fills the secret.
    for _ in 0..60 {
        if run.ended() {
            return;
        }
        if let Ok((0, t)) = sh(run, cluster_id, "k3s kubectl -n dashboard get secret board-token -o jsonpath='{.data.token}' | base64 -d").await {
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

async fn deploy_landing(run: &FirmRun, cluster_id: &str, manifest: String) {
    for _ in 0..40 {
        if run.ended() {
            return;
        }
        let r = crate::client::run_command(
            run.client.as_ref(),
            cluster_id,
            vec![
                "sh".into(),
                "-c".into(),
                "cat > /tmp/landing.json && k3s kubectl apply -f /tmp/landing.json".into(),
            ],
            Some(manifest.clone().into_bytes()),
            1 << 20,
        )
        .await;
        if let Ok((0, _, _)) = r {
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

pub async fn stop(run: &Arc<FirmRun>) {
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
        run.client.remove_service(id);
    }
    let cluster = run.facility.cluster.lock().unwrap().take();
    if let Some(s) = cluster {
        let _ = run.client.stop_session(&s.id, "firm down").await;
        let _ = run.client.forget_session(&s.id);
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
        run,
        &s.id,
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
    let (code, out) = sh(run, &s.id, script).await?;
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

    /// Starting the run wires the firm's zone and git service through the
    /// office's client; the daemon's registry holds them.
    #[tokio::test]
    async fn register_wires_the_firms_zone_and_git_service() {
        use zapper_core::ServiceApi;
        let t = crate::test_core(FIRM);
        let run = crate::test_run(&t.core, "acme");
        let services = t.core.services();
        assert!(
            services.iter().any(|s| s.name == run.domain),
            "zone {} not registered: {:?}",
            run.domain,
            services.iter().map(|s| &s.name).collect::<Vec<_>>(),
        );
        let git = format!("git.{}", run.domain);
        assert!(
            services
                .iter()
                .any(|s| s.name == git && s.port == Some(443)),
            "git service {git} missing: {:?}",
            services.iter().map(|s| &s.name).collect::<Vec<_>>(),
        );
    }
}
