//! REST API on the daemon's unix socket, and the web UI over TCP, over
//! zapper's core (`zapper_core::Core`).

use std::collections::{BTreeMap, HashMap};
use std::convert::Infallible;
use std::path::PathBuf;
use zapper_core::prelude::*;

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
use zapper_core::{
    ApiError, BuildMessage, BuildRequest, Catchup, Core, Cursor, NewSession, Remember, RuleRemoval,
    ServiceSpec, ServiceTarget, SessionInfo, TermChunk, TerminalInfo,
};
use zapper_policy::proto::ShellRequest;
use zapper_policy::{
    evaluate_exec, evaluate_net, needs_inspection, AnyRule, Config, ExecRequest, Kind, NetRequest,
    Policy, Profile, Scope,
};

mod ui;

type D = State<Core>;

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
fn read_routes() -> Router<Core> {
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
        .route("/services", get(list_services))
        .route("/images", get(list_images))
        .route("/images/builds/{id}", get(follow_build))
}

/// What a client of the core (the office) adds to the API and the UI: its
/// routes, under `/v1` and `/api` behind the same guard as the rest, and its
/// page, the UI's front page instead of the sessions page, with the files
/// the page loads, served under `/mount/` (stylesheets and scripts, by
/// their extension).
#[derive(Clone, Default)]
pub struct Mount {
    pub routes: Router,
    pub page: Option<&'static str>,
    pub assets: &'static [(&'static str, &'static str)],
}

/// The API, with what is mounted on it.
pub fn api(core: Core, mount: &Mount) -> Router {
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
        .route("/services", post(add_service))
        .route("/services/{id}", axum::routing::delete(remove_service))
        .route("/sessions/{id}", axum::routing::delete(delete_session))
        .route("/sessions/{id}/stop", post(stop_session))
        .route("/sessions/{id}/start", post(start_session))
        .route("/sessions/{id}/prompt", post(prompt))
        .route("/sessions/{id}/attach", get(attach))
        .route("/evaluate", post(evaluate))
        .route("/images/{name}/build", post(build_image))
        .route("/approvals/{id}", post(answer));
    Router::new().nest(
        "/v1",
        read_routes()
            .merge(write)
            .with_state(core.clone())
            .merge(mount.routes.clone()),
    )
}

/// The UI: the read routes, plus answering approvals. `remote`: it listens
/// on an address other than loopback (`daemon.ui`), for a remote host.
pub fn ui(core: Core, port: u16, remote: bool, mount: &Mount) -> Router {
    let page = mount.page;
    let mut router = Router::new();
    for &(name, body) in mount.assets {
        router = router.route(
            &format!("/mount/{name}"),
            get(move || ui::asset(name, body)),
        );
    }
    router
        .route("/", get(move || ui::index(page)))
        .route("/sessions", get(move || ui::sessions(page.is_some())))
        .route("/favicon.ico", get(ui::favicon))
        .route("/terminal", get(ui::terminal))
        .route("/assets/xterm.js", get(ui::xterm_js))
        .route("/assets/xterm.css", get(ui::xterm_css))
        .with_state(core.clone())
        .nest(
            "/api",
            read_routes()
                .route("/approvals/{id}", post(answer))
                .route("/sessions/{id}/prompt", post(prompt))
                .with_state(core.clone())
                .merge(mount.routes.clone()),
        )
        .layer(middleware::from_fn_with_state(
            (core, port, remote),
            ui_guard,
        ))
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
    State((d, port, remote)): State<(Core, u16, bool)>,
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
    if query.get("token").map(String::as_str) == Some(d.ui_token()) {
        let cookie = format!(
            "zapper_token={}; HttpOnly; SameSite=Strict; Path=/",
            d.ui_token()
        );
        let mut resp = Redirect::to(req.uri().path()).into_response();
        resp.headers_mut()
            .insert(header::SET_COOKIE, cookie.parse().unwrap());
        return resp;
    }
    // The tokens the browser offers: its cookies, and one in the query.
    let offered: Vec<String> = req
        .headers()
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|c| c.to_str().ok())
        .flat_map(|c| c.split(';'))
        .filter_map(|c| c.trim().strip_prefix("zapper_token=").map(str::to_string))
        .chain(query.get("token").cloned())
        .collect();
    if !offered.iter().any(|t| t == d.ui_token()) {
        // A token from another server, or from before a restart, says so.
        let why = if offered.is_empty() {
            "No token found matching this server.  See URL printed by zapper serve"
        } else {
            "Token does not match current server.  See URL printed by zapper serve"
        };
        return (StatusCode::UNAUTHORIZED, why).into_response();
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
    let sessions = d.sessions();
    let running = sessions.iter().filter(|s| s.is_running()).count();
    Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "startup": d.startup_path(),
        "unsaved_changes": !d.diff().trim().is_empty(),
        "sessions": sessions.len(),
        "running": running,
        "pending_approvals": d.approvals().len(),
        "epoch": d.epoch(),
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
    Ok(Json(json!({ "saved": d.startup_path() })))
}

async fn reload(State(d): D) -> R<Json<Value>> {
    d.reload()?;
    Ok(Json(json!({ "reloaded": d.startup_path() })))
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
    for (scope, policy) in d.rule_layers() {
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
        let rule = d.add_session_rule(&s, rule, position);
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
    Ok(Json(match d.remove_rule(&id)? {
        RuleRemoval::Revoked => json!({ "id": id, "revoked": true }),
        RuleRemoval::Removed(scope, rule) => rule_json(&scope, &rule),
    }))
}

async fn list_profiles(State(d): D) -> Json<Value> {
    let c = d.config();
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
    let c = d.config();
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

async fn list_sessions(State(d): D) -> Json<Vec<SessionInfo>> {
    Json(d.sessions().iter().map(|s| d.session_info(s)).collect())
}

async fn get_session(State(d): D, Path(id): Path<String>) -> R<Json<SessionInfo>> {
    let s = d.session(&id)?;
    Ok(Json(d.session_info(&s)))
}

#[derive(Deserialize)]
struct Prompt {
    text: String,
}

/// Gives a driven session's agent a task: queued for its next turn.
async fn prompt(State(d): D, Path(id): Path<String>, body: String) -> R<(StatusCode, Json<Value>)> {
    let p: Prompt = parse_body(&body)?;
    if !d.with_config(|c| c.daemon.driver) {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "the driver is disabled (daemon.driver: false)".into(),
        ));
    }
    let s = d.session(&id)?;
    if let Some(firm) = s.label("firm") {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            format!(
                "{} works for firm {firm}: mail them instead",
                s.label("member").unwrap_or("this session")
            ),
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
async fn record_status(State(d): D) -> Json<Value> {
    Json(
        tokio::task::spawn_blocking(move || d.record_status())
            .await
            .unwrap_or_default(),
    )
}

/// The session's audit log; for a session that has ended, from its directory.
async fn get_audit(State(d): D, Path(id): Path<String>) -> R<Response> {
    let path = d.audit_path(&id)?;
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
    let list = s.requests();
    Ok(Json(list))
}

/// One request with its headers and bodies.
async fn get_request(State(d): D, Path((id, n)): Path<(String, u64)>) -> R<Json<Value>> {
    let s = d.session(&id)?;
    let r = s
        .request(n)
        .ok_or_else(|| anyhow::anyhow!("no request {n}"))?;
    Ok(Json(r))
}

async fn list_terminals(State(d): D, Path(id): Path<String>) -> R<Json<Vec<TerminalInfo>>> {
    let s = d.session(&id)?;
    let terms = s.terminals();
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
        .terminal(n)
        .ok_or_else(|| anyhow::anyhow!("no terminal {n}"))?;
    let (snapshot, rx) = term.watch();
    let live = BroadcastStream::new(rx).filter_map(|r| async move { r.ok() });
    let stream = stream::iter(snapshot).chain(live).map(|chunk| {
        let data = match chunk {
            TermChunk::Output(bytes) => {
                json!({ "o": base64::engine::general_purpose::STANDARD.encode(bytes) })
            }
            TermChunk::Resize(rows, cols) => json!({ "r": [rows, cols] }),
            TermChunk::End(code) => json!({ "end": code }),
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
    /// Labels for the session, which service selectors pick.
    #[serde(default)]
    labels: BTreeMap<String, String>,
    /// Keep its disks when it stops, to start it again.
    #[serde(default)]
    persistent: bool,
}

/// Labels the operator may give a session: short words, and none of the
/// office's own.
fn check_labels(labels: &BTreeMap<String, String>) -> R<()> {
    let word = |s: &str, extra: &[u8]| {
        !s.is_empty()
            && s.len() <= 63
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || extra.contains(&b))
    };
    for (k, v) in labels {
        if !word(k, b"") || !word(v, b".") {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                format!("label {k}={v}: letters, digits, - and _ (and . in values), at most 63"),
            ));
        }
        if ["firm", "run", "member", "office"].contains(&k.as_str()) {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                format!("label {k} is the office's"),
            ));
        }
    }
    Ok(())
}

async fn create_session(State(d): D, body: String) -> R<Json<SessionInfo>> {
    let req: CreateSession = parse_body(&body)?;
    if !req.workdir.is_absolute() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "workdir must be absolute".into(),
        ));
    }
    check_labels(&req.labels)?;
    let mut policy = Policy::default();
    for r in req.rules {
        policy.insert(r, None);
    }
    let mut new = NewSession::plain(req.profile, req.workdir, policy);
    new.labels = req.labels;
    new.persistent = req.persistent;
    let s = d.create_session(new).await?;
    Ok(Json(d.session_info(&s)))
}

async fn stop_session(State(d): D, Path(id): Path<String>) -> R<Json<SessionInfo>> {
    let s = d.session(&id)?;
    d.stop_session(&s, "stopped by operator").await;
    Ok(Json(d.session_info(&s)))
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct StartSession {
    /// Boot its disks even if they changed while it was stopped; recorded.
    accept_changed_disks: bool,
}

/// Starts a stopped persistent session again; returns once its guest is
/// ready. `409` if its disks changed while it was stopped, unless accepted.
async fn start_session(State(d): D, Path(id): Path<String>, body: String) -> R<Json<SessionInfo>> {
    let req: StartSession = if body.trim().is_empty() {
        StartSession::default()
    } else {
        parse_body(&body)?
    };
    let s = d.session(&id)?;
    d.start_session(&s, req.accept_changed_disks)
        .await
        .map_err(|e| {
            let msg = format!("{e:#}");
            let code = if msg.contains("--accept-changed-disks") {
                StatusCode::CONFLICT
            } else {
                StatusCode::BAD_REQUEST
            };
            ApiError(code, msg)
        })?;
    Ok(Json(d.session_info(&s)))
}

async fn delete_session(State(d): D, Path(id): Path<String>) -> R<Json<Value>> {
    let s = d.session(&id)?;
    d.stop_session(&s, "removed by operator").await;
    d.forget_session(&id)?;
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
            d.pump_shell(s, TokioIo::new(up), guest, shell_id, argv, tty)
                .await;
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
    let layers = d.layers_for(e.session.as_deref(), e.profile.as_deref())?;
    let layers: Vec<(Scope, &Policy)> = layers.iter().map(|(s, p)| (s.clone(), p)).collect();
    if let Some(n) = e.net {
        let inspected = needs_inspection(&layers, &n.host, n.port);
        // As the session would ask it, for `{session}` and `{label:KEY}`.
        let n = match e.session.as_deref() {
            Some(id) => d.session(id)?.caller_of(&n),
            None => n,
        };
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
    /// `EPOCH:SEQ`: resume after this event, as `Last-Event-ID` does.
    after: Option<String>,
}

async fn events(
    State(d): D,
    Query(q): Query<EventsQuery>,
    headers: axum::http::HeaderMap,
) -> R<Sse<impl Stream<Item = Result<SseEvent, Infallible>>>> {
    // Where the subscriber left off: `after=`, or the `Last-Event-ID` an
    // EventSource sends again when it reconnects, which wins over `history`.
    let after = q.after.clone().or_else(|| {
        headers
            .get("last-event-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    });
    let after = match after {
        Some(a) => Some(Cursor::parse(&a).ok_or_else(|| {
            ApiError(
                StatusCode::BAD_REQUEST,
                format!("{a:?} is not an event to resume after: EPOCH:SEQ"),
            )
        })?),
        None => None,
    };
    let epoch = d.epoch();
    // Subscribe first, then read history, so no event falls between them.
    let rx = d.subscribe();
    let (catchup, cut) = d.catch_up(q.session.as_deref(), after.as_ref(), q.history);
    let session = q.session.clone();
    let live = BroadcastStream::new(rx).filter_map(move |e| {
        let session = session.clone();
        async move {
            match e {
                Ok(e) if cut.is_some_and(|c| e.seq <= c) => None,
                Ok(e) => (session.is_none() || e.session == session).then_some(Catchup::Event(e)),
                Err(tokio_stream::wrappers::errors::BroadcastStreamRecvError::Lagged(n)) => Some(
                    Catchup::Gap(format!("this stream fell behind and missed {n} events")),
                ),
            }
        }
    });
    let stream = stream::iter(catchup).chain(live).map(move |item| {
        Ok(match item {
            Catchup::Event(e) => SseEvent::default()
                .id(format!("{epoch}:{}", e.seq))
                .json_data(e.view())
                .unwrap_or_default(),
            Catchup::Gap(why) => SseEvent::default()
                .json_data(json!({ "type": "gap", "ts": zapper_core::util::now(), "epoch": epoch, "summary": why }))
                .unwrap_or_default(),
        })
    });
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

async fn list_images(State(d): D) -> R<Json<Value>> {
    Ok(Json(
        json!({ "images": d.images()?, "builds": d.image_builds() }),
    ))
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct NewBuild {
    /// The recipe's files, by argument name, instead of their defaults.
    args: BTreeMap<String, String>,
    /// Without a recipe: the OCI image to start from, and the steps.
    from: Option<String>,
    run: Vec<String>,
    size: Option<String>,
}

/// Starts building an image in the daemon, from the running config: `202`
/// with the build's id, whose log `/images/builds/{id}` streams; `409`
/// while another build runs.
async fn build_image(
    State(d): D,
    Path(name): Path<String>,
    body: String,
) -> R<(StatusCode, Json<Value>)> {
    let b: NewBuild = if body.trim().is_empty() {
        NewBuild::default()
    } else {
        parse_body(&body)?
    };
    if let Some(running) = d.image_builds().into_iter().find(|b| b.outcome.is_none()) {
        return Err(ApiError(
            StatusCode::CONFLICT,
            format!(
                "image {} is being built (build {}); one build runs at a time",
                running.name, running.id
            ),
        ));
    }
    let req = BuildRequest {
        args: b
            .args
            .into_iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect(),
        from: b.from,
        run: b.run,
        size: b.size,
    };
    let build = d.build_image(&name, req)?;
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({ "id": build.id, "name": build.name })),
    ))
}

/// A build's log as server-sent events: `{"line"}` each, `{"dropped": n}`
/// for lines no longer kept, and last `{"end": {"built": PATH}}` or
/// `{"end": {"failed": WHY}}`, when the stream ends.
async fn follow_build(
    State(d): D,
    Path(id): Path<u64>,
) -> R<Sse<impl Stream<Item = Result<SseEvent, Infallible>>>> {
    let build = d
        .image_build(id)
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("no build {id}")))?;
    let f = build.follow();
    let mut head = Vec::new();
    if f.dropped > 0 {
        head.push(json!({ "dropped": f.dropped }));
    }
    head.extend(f.kept.into_iter().map(|l| json!({ "line": l })));
    let tail = match f.outcome {
        Some(o) => stream::iter(vec![json!({ "end": o })]).boxed(),
        None => stream::unfold(Some(f.live), |live| async move {
            let mut live = live?;
            use tokio::sync::broadcast::error::RecvError;
            match live.recv().await {
                Ok(BuildMessage::Line(l)) => Some((json!({ "line": l }), Some(live))),
                Ok(BuildMessage::End(o)) => Some((json!({ "end": o }), None)),
                Err(RecvError::Lagged(n)) => Some((json!({ "dropped": n }), Some(live))),
                Err(RecvError::Closed) => None,
            }
        })
        .boxed(),
    };
    let stream = stream::iter(head)
        .chain(tail)
        .map(|v| Ok(SseEvent::default().json_data(v).unwrap_or_default()));
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

async fn list_approvals(State(d): D) -> Json<Value> {
    Json(json!(d.approvals()))
}

async fn list_services(State(d): D) -> Json<Value> {
    Json(json!(d.services()))
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ServiceTo {
    Addr {
        addr: std::net::SocketAddr,
    },
    Session {
        session: String,
        port: u32,
    },
    Socket {
        socket: String,
        #[serde(default)]
        tls: bool,
    },
}

#[derive(Deserialize)]
struct NewService {
    name: String,
    port: u16,
    /// The sessions that may use it, by label; every session when empty.
    #[serde(default)]
    sessions: BTreeMap<String, String>,
    to: ServiceTo,
}

/// The operator's services: to an address on the host's network, a port of
/// a session's VM, or a socket in the daemon's `state/services/` directory.
async fn add_service(State(d): D, body: String) -> R<Json<Value>> {
    let n: NewService = parse_body(&body)?;
    let target = match n.to {
        ServiceTo::Addr { addr } => ServiceTarget::Addr(addr),
        ServiceTo::Session { session, port } => {
            d.session(&session)?;
            ServiceTarget::Session { id: session, port }
        }
        ServiceTo::Socket { socket, tls } => ServiceTarget::Socket { name: socket, tls },
    };
    let id = d.add_service(ServiceSpec {
        name: n.name,
        port: n.port,
        sessions: n.sessions,
        target,
        owner: OPERATOR.into(),
    })?;
    Ok(Json(json!(d.services().into_iter().find(|s| s.id == id))))
}

/// Who registers services through the API.
const OPERATOR: &str = "operator";

/// Removes a service the operator added; others are their owners'.
async fn remove_service(State(d): D, Path(id): Path<u64>) -> R<Json<Value>> {
    let Some(s) = d.services().into_iter().find(|s| s.id == id) else {
        return Err(ApiError(StatusCode::NOT_FOUND, format!("no service {id}")));
    };
    if s.owner != OPERATOR {
        return Err(ApiError(
            StatusCode::CONFLICT,
            format!("service {id} is {}'s", s.owner),
        ));
    }
    Ok(Json(json!(d.remove_service(id))))
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
mod tests;

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
