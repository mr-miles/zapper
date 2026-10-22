//! REST API on the daemon's unix socket, and the web UI over TCP.

use std::collections::HashMap;
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, Query, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::stream::{self, Stream, StreamExt};
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_stream::wrappers::BroadcastStream;
use zapper_policy::proto::ShellRequest;
use zapper_policy::{
    evaluate_exec, evaluate_net, needs_inspection, AnyRule, Config, ExecRequest, Kind, NetRequest,
    Policy, Profile, Scope,
};

use super::session::{self, NewSession};
use super::{spec, Daemon, EventBody, Remember};

type D = State<Arc<Daemon>>;

pub struct ApiError(pub StatusCode, pub String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        let msg = format!("{e:#}");
        let code = if msg.starts_with("no ") {
            StatusCode::NOT_FOUND
        } else {
            StatusCode::BAD_REQUEST
        };
        ApiError(code, msg)
    }
}

type R<T> = Result<T, ApiError>;

fn yaml(text: String) -> Response {
    ([(header::CONTENT_TYPE, "application/yaml")], text).into_response()
}

#[derive(Deserialize, Default)]
struct Format {
    format: Option<String>,
}

fn config_response(c: &Config, f: &Format) -> Response {
    match f.format.as_deref() {
        Some("json") => Json(c).into_response(),
        _ => yaml(c.to_yaml()),
    }
}

/// Parses a YAML or JSON body (JSON is YAML).
fn parse_body<T: serde::de::DeserializeOwned>(body: &str) -> R<T> {
    serde_yaml::from_str(body).map_err(|e| ApiError(StatusCode::BAD_REQUEST, format!("body: {e}")))
}

/// Routes shared by the API and the UI: reads only.
fn read_routes() -> Router<Arc<Daemon>> {
    Router::new()
        .route("/status", get(status))
        .route("/config", get(get_config))
        .route("/config/startup", get(get_startup))
        .route("/config/diff", get(get_diff))
        .route("/rules", get(list_rules))
        .route("/profiles", get(list_profiles))
        .route("/sessions", get(list_sessions))
        .route("/sessions/{id}", get(get_session))
        .route("/sessions/{id}/audit", get(get_audit))
        .route("/sessions/{id}/terminals", get(list_terminals))
        .route("/sessions/{id}/requests", get(list_requests))
        .route("/sessions/{id}/requests/{n}", get(get_request))
        .route("/sessions/{id}/terminals/{n}/stream", get(terminal_stream))
        .route("/events", get(events))
        .route("/record", get(record_status))
        .route("/approvals", get(list_approvals))
        .merge(super::office::http::read_routes())
}

pub fn api(daemon: Arc<Daemon>) -> Router {
    let write = Router::new()
        .route("/config", axum::routing::put(put_config))
        .route("/config/save", post(save))
        .route("/config/reload", post(reload))
        .route("/rules", post(add_rule))
        .route("/rules/{id}", axum::routing::delete(delete_rule))
        .route(
            "/profiles/{name}",
            get(get_profile).put(put_profile).delete(delete_profile),
        )
        .route("/sessions", post(create_session))
        .route("/sessions/{id}", axum::routing::delete(delete_session))
        .route("/sessions/{id}/stop", post(stop_session))
        .route("/sessions/{id}/prompt", post(prompt))
        .route("/sessions/{id}/attach", get(attach))
        .route("/evaluate", post(evaluate))
        .route("/approvals/{id}", post(answer))
        .merge(super::office::http::board_routes());
    Router::new()
        .nest("/v1", read_routes().merge(write))
        .with_state(daemon)
}

/// The UI: the read routes, plus answering approvals. `remote`: it listens
/// on an address other than loopback (`daemon.ui`), for a remote host.
pub fn ui(daemon: Arc<Daemon>, port: u16, remote: bool) -> Router {
    Router::new()
        .route("/", get(super::ui::index))
        .route("/sessions", get(super::ui::sessions))
        .route("/favicon.ico", get(super::ui::favicon))
        .route("/terminal", get(super::ui::terminal))
        .route("/assets/xterm.js", get(super::ui::xterm_js))
        .route("/assets/xterm.css", get(super::ui::xterm_css))
        .nest(
            "/api",
            read_routes()
                .route("/approvals/{id}", post(answer))
                .route("/sessions/{id}/prompt", post(prompt))
                .merge(super::office::http::board_routes()),
        )
        .layer(middleware::from_fn_with_state(
            (daemon.clone(), port, remote),
            ui_guard,
        ))
        .with_state(daemon)
}

/// The names a loopback UI answers to.
fn local_hosts(port: u16) -> [String; 3] {
    [
        format!("127.0.0.1:{port}"),
        format!("localhost:{port}"),
        format!("[::1]:{port}"),
    ]
}

/// Whether a request's `Host` may reach the UI. On loopback, only a local
/// name, which defeats DNS rebinding. Listening elsewhere, any well-formed
/// host: the UI is reached by whatever name or address the operator uses,
/// and the token cookie, set for that name only, is what keeps others out.
fn host_ok(host: &str, port: u16, remote: bool) -> bool {
    if remote {
        !host.is_empty()
            && host.len() <= 300
            && host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".-:[]".contains(&b))
    } else {
        local_hosts(port).iter().any(|l| l == host)
    }
}

/// Whether a write's `Origin` is the UI itself: a local name on loopback,
/// else the very host the request was sent to, over http or https (behind
/// a TLS proxy).
fn origin_ok(origin: &str, host: &str, port: u16, remote: bool) -> bool {
    if remote {
        origin == format!("http://{host}") || origin == format!("https://{host}")
    } else {
        local_hosts(port)
            .iter()
            .any(|l| origin == format!("http://{l}"))
    }
}

/// A browser can be tricked into talking to the UI. Every request needs the
/// token (in the query once, then a strict cookie) and an acceptable Host
/// header (`host_ok`). A request that changes something also needs the
/// `X-Zapper-UI` header, which another site's page can't send without a
/// CORS preflight that is never granted, and an Origin naming the UI itself
/// (`origin_ok`).
async fn ui_guard(
    State((d, port, remote)): State<(Arc<Daemon>, u16, bool)>,
    req: Request,
    next: Next,
) -> Response {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("")
        .to_string();
    if !host_ok(&host, port, remote) {
        return (StatusCode::FORBIDDEN, "bad Host header").into_response();
    }
    let query: HashMap<String, String> =
        req.uri().query().map(url_decode_pairs).unwrap_or_default();
    if query.get("token") == Some(&d.ui_token) {
        let cookie = format!(
            "zapper_token={}; HttpOnly; SameSite=Strict; Path=/",
            d.ui_token
        );
        let mut resp = Redirect::to(req.uri().path()).into_response();
        resp.headers_mut()
            .insert(header::SET_COOKIE, cookie.parse().unwrap());
        return resp;
    }
    let cookie_ok = req
        .headers()
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|c| c.to_str().ok())
        .flat_map(|c| c.split(';'))
        .any(|c| c.trim() == format!("zapper_token={}", d.ui_token));
    if !cookie_ok {
        return (
            StatusCode::UNAUTHORIZED,
            "open the URL printed by `zapper serve`",
        )
            .into_response();
    }
    if !matches!(
        *req.method(),
        axum::http::Method::GET | axum::http::Method::HEAD
    ) {
        let origin = req
            .headers()
            .get(header::ORIGIN)
            .and_then(|h| h.to_str().ok())
            .unwrap_or("");
        let marked = req.headers().get("x-zapper-ui").is_some_and(|v| v == "1");
        if !marked || !origin_ok(origin, &host, port, remote) {
            return (
                StatusCode::FORBIDDEN,
                "missing X-Zapper-UI or a local Origin",
            )
                .into_response();
        }
    }
    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    // Published files carry a stricter, sandboxing policy of their own.
    if !h.contains_key("content-security-policy") {
        h.insert("content-security-policy", "default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; frame-ancestors 'none'".parse().unwrap());
    }
    h.insert("x-content-type-options", "nosniff".parse().unwrap());
    h.insert("referrer-policy", "no-referrer".parse().unwrap());
    resp
}

fn url_decode_pairs(q: &str) -> HashMap<String, String> {
    q.split('&')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.to_string(), percent_decode(v)))
        .collect()
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = b
            .get(i + 1..i + 3)
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match (b[i], hex) {
            (b'%', Some(v)) => {
                out.push(v);
                i += 3;
            }
            (b'+', _) => {
                out.push(b' ');
                i += 1;
            }
            (c, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

async fn status(State(d): D) -> Json<Value> {
    let sessions = d.sessions.read().unwrap();
    let running = sessions.values().filter(|s| s.is_running()).count();
    Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "startup": d.startup_path,
        "unsaved_changes": !d.diff().trim().is_empty(),
        "sessions": sessions.len(),
        "running": running,
        "pending_approvals": d.approvals().len(),
    }))
}

async fn get_config(State(d): D, Query(f): Query<Format>) -> Response {
    config_response(&d.redacted_config(), &f)
}

async fn get_startup(State(d): D, Query(f): Query<Format>) -> R<Response> {
    Ok(config_response(&d.startup_config()?, &f))
}

async fn get_diff(State(d): D) -> String {
    d.diff()
}

async fn put_config(State(d): D, body: String) -> R<Json<Value>> {
    let config: Config = parse_body(&body)?;
    d.replace_config(config)?;
    Ok(Json(json!({ "ok": true })))
}

async fn save(State(d): D) -> R<Json<Value>> {
    d.save()?;
    Ok(Json(json!({ "saved": d.startup_path })))
}

async fn reload(State(d): D) -> R<Json<Value>> {
    d.reload()?;
    Ok(Json(json!({ "reloaded": d.startup_path })))
}

#[derive(Deserialize)]
struct ScopeQuery {
    scope: Option<String>,
}

fn rule_json(scope: &Scope, r: &AnyRule) -> Value {
    let mut v = serde_json::to_value(r).unwrap();
    v["scope"] = json!(scope.to_string());
    v
}

async fn list_rules(State(d): D, Query(q): Query<ScopeQuery>) -> R<Json<Vec<Value>>> {
    let filter: Option<Scope> = q.scope.as_deref().map(str::parse).transpose()?;
    let mut out = Vec::new();
    let c = d.config.read().unwrap();
    let mut layers: Vec<(Scope, Policy)> = vec![(Scope::Global, c.policy.clone())];
    layers.extend(
        c.profiles
            .iter()
            .map(|(n, p)| (Scope::Profile(n.clone()), p.policy.clone())),
    );
    for (id, s) in d.sessions.read().unwrap().iter() {
        layers.push((Scope::Session(id.clone()), s.policy.read().unwrap().clone()));
    }
    for run in d.office.runs.read().unwrap().values() {
        for (role, p) in &run.roles {
            layers.push((
                Scope::Role(format!("{}/{role}", run.firm)),
                p.read().unwrap().clone(),
            ));
        }
    }
    for (scope, policy) in layers {
        if filter.as_ref().is_some_and(|f| *f != scope) {
            continue;
        }
        out.extend(policy.rules().iter().map(|r| rule_json(&scope, r)));
    }
    Ok(Json(out))
}

#[derive(Deserialize)]
struct NewRule {
    scope: String,
    #[serde(default)]
    position: Option<usize>,
    rule: AnyRule,
}

async fn add_rule(State(d): D, body: String) -> R<Json<Value>> {
    let NewRule {
        scope,
        position,
        mut rule,
    } = parse_body(&body)?;
    let scope: Scope = scope.parse()?;
    if let Scope::Session(id) = &scope {
        if rule.kind() == Kind::Fs {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                "fs rules apply at boot; add them to a profile".into(),
            ));
        }
        let s = d.session(id)?;
        let rule = s.add_rule(rule, position);
        d.emit(
            Some(&s),
            EventBody::Config {
                change: format!("rule {} added to {scope}", rule.id().unwrap_or("?")),
            },
        );
        return Ok(Json(rule_json(&scope, &rule)));
    }
    let added = d.change(&format!("rule added to {scope}"), |c| {
        if rule.id().is_none() {
            let taken = c.rule_ids();
            let kind = rule.kind();
            rule.set_id(
                (1..)
                    .map(|n| format!("{kind}-{n}"))
                    .find(|i| !taken.contains(i))
                    .unwrap(),
            );
        }
        c.policy_mut(&scope)
            .ok_or_else(|| anyhow::anyhow!("no {scope}"))?
            .insert(rule.clone(), position);
        Ok(rule)
    })?;
    Ok(Json(rule_json(&scope, &added)))
}

async fn delete_rule(State(d): D, Path(id): Path<String>) -> R<Json<Value>> {
    // An officer's standing approval: revoked, so the firm's records agree.
    let runs: Vec<_> = d.office.runs.read().unwrap().values().cloned().collect();
    for run in runs {
        if run.security.lock().unwrap().standing.contains_key(&id) {
            super::office::security::revoke(
                &run,
                zapper_policy::BOARD,
                &id,
                "removed by the operator",
            )?;
            return Ok(Json(json!({ "id": id, "revoked": true })));
        }
    }
    if id.starts_with("s-") {
        for s in d.sessions.read().unwrap().values() {
            if let Some(r) = s.policy.write().unwrap().remove(&id) {
                return Ok(Json(rule_json(&Scope::Session(s.id.clone()), &r)));
            }
        }
        return Err(ApiError(StatusCode::NOT_FOUND, format!("no rule {id}")));
    }
    let (scope, rule) = d.change(&format!("rule {id} removed"), |c| {
        let scope = c
            .find_rule(&id)
            .ok_or_else(|| anyhow::anyhow!("no rule {id}"))?;
        let rule = c.policy_mut(&scope).unwrap().remove(&id).unwrap();
        Ok((scope, rule))
    })?;
    Ok(Json(rule_json(&scope, &rule)))
}

async fn list_profiles(State(d): D) -> Json<Value> {
    let c = d.config.read().unwrap();
    let out: Vec<Value> = c
        .profiles
        .iter()
        .map(|(name, p)| {
            let resolved = c.resolve(name).ok();
            json!({
                "name": name,
                "extends": p.extends,
                "description": p.description,
                "image": resolved.as_ref().map(|r| r.engine.image.clone()),
                "command": resolved.as_ref().map(|r| r.agent.command.clone()),
                "ttl_secs": resolved.as_ref().map(|r| r.engine.ttl_secs),
            })
        })
        .collect();
    Json(json!(out))
}

async fn get_profile(
    State(d): D,
    Path(name): Path<String>,
    Query(f): Query<Format>,
) -> R<Response> {
    let c = d.config.read().unwrap();
    let p = c
        .profiles
        .get(&name)
        .ok_or_else(|| anyhow::anyhow!("no profile {name}"))?;
    let resolved = c.resolve(&name)?;
    let v = json!({ "profile": p, "resolved": resolved });
    Ok(match f.format.as_deref() {
        Some("json") => Json(v).into_response(),
        _ => yaml(serde_yaml::to_string(&v).unwrap()),
    })
}

async fn put_profile(State(d): D, Path(name): Path<String>, body: String) -> R<Json<Value>> {
    let profile: Profile = parse_body(&body)?;
    d.change(&format!("profile {name} set"), |c| {
        c.profiles.insert(name.clone(), profile);
        Ok(())
    })?;
    Ok(Json(json!({ "ok": true })))
}

async fn delete_profile(State(d): D, Path(name): Path<String>) -> R<Json<Value>> {
    d.change(&format!("profile {name} removed"), |c| {
        c.profiles
            .remove(&name)
            .ok_or_else(|| anyhow::anyhow!("no profile {name}"))?;
        Ok(())
    })?;
    Ok(Json(json!({ "ok": true })))
}

async fn list_sessions(State(d): D) -> Json<Vec<session::SessionInfo>> {
    Json(
        d.sessions
            .read()
            .unwrap()
            .values()
            .map(|s| info(&d, s))
            .collect(),
    )
}

async fn get_session(State(d): D, Path(id): Path<String>) -> R<Json<session::SessionInfo>> {
    let s = d.session(&id)?;
    Ok(Json(info(&d, &s)))
}

/// A session's info, saying so when the driver is off.
fn info(d: &Daemon, s: &session::Session) -> session::SessionInfo {
    let mut i = s.info();
    if !d.config.read().unwrap().daemon.driver {
        i.driver = json!("disabled");
    }
    i
}

#[derive(Deserialize)]
struct Prompt {
    text: String,
}

/// Gives a driven session's agent a task: queued for its next turn.
async fn prompt(State(d): D, Path(id): Path<String>, body: String) -> R<(StatusCode, Json<Value>)> {
    let p: Prompt = parse_body(&body)?;
    if !d.config.read().unwrap().daemon.driver {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "the driver is disabled (daemon.driver: false)".into(),
        ));
    }
    let s = d.session(&id)?;
    if let Some(f) = &s.firm {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            format!("{} works for firm {}: mail them instead", f.member, f.firm),
        ));
    }
    let Some(driver) = &s.driver else {
        return Err(ApiError(
            StatusCode::CONFLICT,
            format!("session {id} is not driven (agent.driven: false); attach to it instead"),
        ));
    };
    if !s.is_running() {
        return Err(ApiError(
            StatusCode::CONFLICT,
            format!("session {id} is not running"),
        ));
    }
    let queued = driver.prompt(&p.text)?;
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({ "queued": queued, "driver": driver.info() })),
    ))
}

/// Who keeps the record, and whether it is being written.
async fn record_status() -> Json<Value> {
    Json(
        tokio::task::spawn_blocking(super::record::status)
            .await
            .unwrap_or_default(),
    )
}

/// The session's audit log; for a session that has ended, from its directory.
async fn get_audit(State(d): D, Path(id): Path<String>) -> R<Response> {
    let path = match d.session(&id) {
        Ok(s) => s.audit_path(),
        Err(_) => {
            // The id goes into a path: accept only the form session ids have.
            if !super::is_session_id(&id) {
                return Err(ApiError(
                    StatusCode::BAD_REQUEST,
                    format!("{id:?} is not a session id"),
                ));
            }
            super::record::view(&d.paths.state.join("sessions").join(&id).join("audit.jsonl"))
        }
    };
    let text = tokio::fs::read_to_string(&path).await.map_err(|_| {
        ApiError(
            StatusCode::NOT_FOUND,
            format!("no audit log for session {id}"),
        )
    })?;
    Ok(([(header::CONTENT_TYPE, "application/x-ndjson")], text).into_response())
}

/// The session's HTTP requests and connections, oldest first.
async fn list_requests(State(d): D, Path(id): Path<String>) -> R<Json<Vec<Value>>> {
    let s = d.session(&id)?;
    let list = s.requests.lock().unwrap().list();
    Ok(Json(list))
}

/// One request with its headers and bodies.
async fn get_request(State(d): D, Path((id, n)): Path<(String, u64)>) -> R<Json<Value>> {
    let s = d.session(&id)?;
    let r = s
        .requests
        .lock()
        .unwrap()
        .get(n)
        .ok_or_else(|| anyhow::anyhow!("no request {n}"))?;
    Ok(Json(r))
}

async fn list_terminals(
    State(d): D,
    Path(id): Path<String>,
) -> R<Json<Vec<session::TerminalInfo>>> {
    let s = d.session(&id)?;
    let terms = s
        .terminals
        .lock()
        .unwrap()
        .values()
        .map(|t| t.info())
        .collect();
    Ok(Json(terms))
}

/// A terminal's kept output, then its live output, as server-sent events:
/// `{"o": base64}` output, `{"r": [rows, cols]}` a resize, `{"end": code}`
/// when it closes. Read-only: nothing goes back to the terminal.
async fn terminal_stream(
    State(d): D,
    Path((id, n)): Path<(String, u64)>,
) -> R<Sse<impl Stream<Item = Result<SseEvent, Infallible>>>> {
    use base64::Engine as _;
    let s = d.session(&id)?;
    let term = s
        .terminals
        .lock()
        .unwrap()
        .get(&n)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("no terminal {n}"))?;
    let (snapshot, rx) = term.watch();
    let live = BroadcastStream::new(rx).filter_map(|r| async move { r.ok() });
    let stream = stream::iter(snapshot).chain(live).map(|chunk| {
        let data = match chunk {
            session::TermChunk::Output(bytes) => {
                json!({ "o": base64::engine::general_purpose::STANDARD.encode(bytes) })
            }
            session::TermChunk::Resize(rows, cols) => json!({ "r": [rows, cols] }),
            session::TermChunk::End(code) => json!({ "end": code }),
        };
        Ok(SseEvent::default().data(data.to_string()))
    });
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

#[derive(Deserialize)]
struct CreateSession {
    profile: String,
    workdir: PathBuf,
    #[serde(default)]
    rules: Vec<AnyRule>,
}

async fn create_session(State(d): D, body: String) -> R<Json<session::SessionInfo>> {
    let req: CreateSession = parse_body(&body)?;
    if !req.workdir.is_absolute() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "workdir must be absolute".into(),
        ));
    }
    let mut policy = Policy::default();
    for r in req.rules {
        policy.insert(r, None);
    }
    let s = session::create(
        d.clone(),
        NewSession::plain(req.profile, req.workdir, policy),
    )
    .await?;
    Ok(Json(info(&d, &s)))
}

async fn stop_session(State(d): D, Path(id): Path<String>) -> R<Json<session::SessionInfo>> {
    let s = d.session(&id)?;
    session::stop(&d, &s, "stopped by operator").await;
    Ok(Json(info(&d, &s)))
}

async fn delete_session(State(d): D, Path(id): Path<String>) -> R<Json<Value>> {
    let s = d.session(&id)?;
    session::stop(&d, &s, "removed by operator").await;
    d.sessions.write().unwrap().remove(&id);
    Ok(Json(json!({ "removed": id, "audit": s.audit_path() })))
}

#[derive(Deserialize)]
struct AttachQuery {
    /// `ROWSxCOLS`.
    tty: Option<String>,
    /// JSON array.
    argv: Option<String>,
    #[serde(default)]
    system: bool,
}

/// Upgrades the connection to a terminal in the guest; frames as in
/// `zapper_policy::proto::Frame`.
async fn attach(
    State(d): D,
    Path(id): Path<String>,
    Query(q): Query<AttachQuery>,
    req: Request,
) -> R<Response> {
    let s = d.session(&id)?;
    let tty = q
        .tty
        .as_deref()
        .and_then(|t| t.split_once('x'))
        .and_then(|(r, c)| Some((r.parse().ok()?, c.parse().ok()?)));
    let argv: Vec<String> = match &q.argv {
        Some(a) => serde_json::from_str(a)
            .map_err(|e| ApiError(StatusCode::BAD_REQUEST, format!("argv: {e}")))?,
        None => vec![],
    };
    let shell = ShellRequest {
        id: 0,
        argv,
        tty,
        env: Default::default(),
        system: q.system,
    };
    let (guest, shell_id) = s.open_shell(shell.clone()).await?;
    let argv = if shell.argv.is_empty() {
        s.resolved.agent.command.clone()
    } else {
        shell.argv
    };
    let on_upgrade = hyper::upgrade::on(req);
    tokio::spawn(async move {
        if let Ok(up) = on_upgrade.await {
            session::pump_shell(d, s, TokioIo::new(up), guest, shell_id, argv, tty).await;
        }
    });
    Ok(Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(header::CONNECTION, "upgrade")
        .header(header::UPGRADE, "zapper-shell")
        .body(Body::empty())
        .unwrap())
}

#[derive(Deserialize)]
struct Evaluate {
    session: Option<String>,
    profile: Option<String>,
    net: Option<NetRequest>,
    exec: Option<ExecRequest>,
}

/// Dry run: what would happen to this request.
async fn evaluate(State(d): D, body: String) -> R<Json<Value>> {
    let e: Evaluate = parse_body(&body)?;
    let c = d.config.read().unwrap();
    let layers = match (&e.session, &e.profile) {
        (Some(id), _) => d.session(id)?.layers(&c),
        (None, Some(p)) => {
            let mut l = c.resolve(p)?.layers;
            l.push((Scope::Global, c.policy.clone()));
            l
        }
        (None, None) => vec![(Scope::Global, c.policy.clone())],
    };
    let layers = spec::layers_of(&layers);
    if let Some(n) = e.net {
        let inspected = needs_inspection(&layers, &n.host, n.port);
        let (decision, effects) = evaluate_net(&layers, &n);
        return Ok(Json(
            json!({ "decision": decision, "inspected": inspected, "secrets": effects.secrets }),
        ));
    }
    if let Some(x) = e.exec {
        return Ok(Json(json!({ "decision": evaluate_exec(&layers, &x) })));
    }
    Err(ApiError(StatusCode::BAD_REQUEST, "give net or exec".into()))
}

#[derive(Deserialize)]
struct EventsQuery {
    session: Option<String>,
    #[serde(default)]
    history: bool,
}

async fn events(
    State(d): D,
    Query(q): Query<EventsQuery>,
) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let history = if q.history {
        d.recent_events(q.session.as_deref())
    } else {
        vec![]
    };
    let session = q.session.clone();
    let live = BroadcastStream::new(d.events.subscribe()).filter_map(move |e| {
        let session = session.clone();
        async move {
            let e = e.ok()?;
            (session.is_none() || e.session == session).then_some(e)
        }
    });
    let stream = stream::iter(history)
        .chain(live)
        .map(|e| Ok(SseEvent::default().json_data(&e).unwrap_or_default()));
    Sse::new(stream).keep_alive(KeepAlive::default())
}

async fn list_approvals(State(d): D) -> Json<Value> {
    Json(json!(d.approvals()))
}

#[derive(Deserialize)]
struct Answer {
    allow: bool,
    #[serde(default)]
    remember: Option<Remember>,
}

async fn answer(State(d): D, Path(id): Path<String>, body: String) -> R<Json<Value>> {
    let a: Answer = parse_body(&body)?;
    d.answer(&id, a.allow, a.remember)?;
    Ok(Json(json!({ "ok": true })))
}

#[cfg(test)]
mod guard_tests {
    use super::{host_ok, origin_ok};

    #[test]
    fn loopback_ui_takes_local_names_only() {
        assert!(host_ok("127.0.0.1:7878", 7878, false));
        assert!(host_ok("localhost:7878", 7878, false));
        assert!(!host_ok("127.0.0.1:7879", 7878, false));
        assert!(!host_ok("evil.example:7878", 7878, false), "DNS rebinding");
        assert!(origin_ok(
            "http://localhost:7878",
            "localhost:7878",
            7878,
            false
        ));
        assert!(!origin_ok(
            "http://evil.example",
            "localhost:7878",
            7878,
            false
        ));
        assert!(!origin_ok(
            "https://zapper.lan:7878",
            "zapper.lan:7878",
            7878,
            false
        ));
    }

    #[test]
    fn remote_ui_takes_its_own_origin_only() {
        assert!(host_ok("zapper.lan:7878", 7878, true));
        assert!(host_ok("192.168.1.20:7878", 7878, true));
        assert!(host_ok("[fd00::1]:7878", 7878, true));
        assert!(!host_ok("", 7878, true));
        assert!(!host_ok("a b", 7878, true));
        let h = "zapper.lan:7878";
        assert!(origin_ok("http://zapper.lan:7878", h, 7878, true));
        assert!(
            origin_ok("https://zapper.lan:7878", h, 7878, true),
            "behind a TLS proxy"
        );
        assert!(!origin_ok("http://evil.example", h, 7878, true));
        assert!(!origin_ok(
            "http://zapper.lan:7878.evil.example",
            h,
            7878,
            true
        ));
        assert!(!origin_ok("", h, 7878, true));
    }
}
