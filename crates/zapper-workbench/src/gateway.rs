//! The gateway: the workbench's API (`/w/v1`) and its UI. It owns remote
//! security: it authenticates every client (a token, or the UI's cookie
//! with the same `Host`, `Origin` and header checks as zapper's UI) and
//! authorizes each request against what that token allows, then makes the
//! calls to zapper itself, over zapper's trusted socket. It never passes a
//! client's request through as it came: a session it creates gets the
//! token's labels and a directory of the workbench's; a rule goes only to
//! the own layer of a session the token reaches; a terminal is never a
//! `system` one; lists, approvals and events are filtered to the token's
//! sessions.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, Query, Request, State};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{delete, get, post};
use axum::{Extension, Json, Router};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::facility::Facilities;
use crate::sse;
use crate::tokens::{Grant, Right, Tokens};
use crate::zapper::{url_encode, Zapper};

/// What the gateway's handlers share.
#[derive(Clone)]
pub struct Ctx {
    pub zapper: Zapper,
    pub tokens: Tokens,
    pub facilities: Arc<Facilities>,
    /// Where sessions' directories are made (`{workdir}`): never a client's
    /// path on the host.
    pub work: PathBuf,
    /// The port the UI is served on, and whether it listens beyond loopback.
    pub port: u16,
    pub remote: bool,
}

/// An error, as the gateway answers it: `{"error": …}` with its status.
#[derive(Debug)]
pub struct Error(pub StatusCode, pub String);

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

impl From<anyhow::Error> for Error {
    fn from(e: anyhow::Error) -> Self {
        Error(StatusCode::BAD_GATEWAY, format!("{e:#}"))
    }
}

impl From<(StatusCode, String)> for Error {
    fn from((s, why): (StatusCode, String)) -> Self {
        Error(s, why)
    }
}

type R<T> = Result<T, Error>;

fn need(g: &Grant, right: Right) -> R<()> {
    if g.may(right) {
        Ok(())
    } else {
        Err(Error(
            StatusCode::FORBIDDEN,
            format!(
                "token {} may not {}",
                g.name,
                serde_json::to_value(right).unwrap().as_str().unwrap()
            ),
        ))
    }
}

/// A session id as zapper makes them: short, letters and digits.
fn session_id(id: &str) -> R<&str> {
    if !id.is_empty() && id.len() <= 32 && id.bytes().all(|b| b.is_ascii_alphanumeric()) {
        Ok(id)
    } else {
        Err(Error(StatusCode::NOT_FOUND, format!("no session {id}")))
    }
}

/// The session `id`'s info, if the grant reaches it; one it doesn't is
/// answered as one that isn't there.
async fn reached(ctx: &Ctx, g: &Grant, id: &str) -> R<Value> {
    let id = session_id(id)?;
    let info = ctx
        .zapper
        .call(Method::GET, &format!("/v1/sessions/{id}"), None)
        .await?
        .ok()?;
    if !g.reaches(&info) {
        return Err(Error(StatusCode::NOT_FOUND, format!("no session {id}")));
    }
    Ok(info)
}

/// zapper's answer to `GET path` passed on as it comes (a stream, or a
/// log), with its status and content type.
async fn passed_on(ctx: &Ctx, path: &str, headers: &HeaderMap) -> R<Response> {
    let resp = ctx.zapper.stream(path, headers).await?;
    let mut out = Response::builder().status(resp.status());
    for h in [header::CONTENT_TYPE, header::CACHE_CONTROL] {
        if let Some(v) = resp.headers().get(&h) {
            out = out.header(h, v);
        }
    }
    Ok(out.body(Body::new(resp.into_body())).unwrap())
}

// Status and profiles.

/// The token's grant, and zapper's state: `zapper.error` when zapper
/// can't be reached, which is no reason to sign anyone out.
async fn status(State(ctx): State<Ctx>, Extension(g): Extension<Grant>) -> Json<Value> {
    let zapper = match ctx.zapper.call(Method::GET, "/v1/status", None).await {
        Ok(a) => match a.ok() {
            Ok(s) => json!({ "version": s["version"], "epoch": s["epoch"] }),
            Err((_, why)) => json!({ "error": why }),
        },
        Err(e) => json!({ "error": format!("{e:#}") }),
    };
    Json(json!({
        "zapper": zapper,
        "token": { "name": g.name, "labels": g.labels, "rights": g.rights },
    }))
}

async fn profiles(State(ctx): State<Ctx>, Extension(g): Extension<Grant>) -> R<Json<Value>> {
    need(&g, Right::Read)?;
    let p = ctx
        .zapper
        .call(Method::GET, "/v1/profiles", None)
        .await?
        .ok()?;
    // Names only: a profile's rules and secrets' sources are the operator's.
    let names: Vec<Value> = match &p {
        Value::Object(m) => m.keys().map(|k| json!(k)).collect(),
        Value::Array(a) => a
            .iter()
            .filter_map(|v| {
                v.get("name")
                    .or(Some(v))
                    .and_then(|n| n.as_str())
                    .map(|n| json!(n))
            })
            .collect(),
        _ => vec![],
    };
    Ok(Json(Value::Array(names)))
}

// Sessions.

async fn list_sessions(State(ctx): State<Ctx>, Extension(g): Extension<Grant>) -> R<Json<Value>> {
    need(&g, Right::Read)?;
    let all = ctx
        .zapper
        .call(Method::GET, "/v1/sessions", None)
        .await?
        .ok()?;
    let mine: Vec<Value> = all
        .as_array()
        .into_iter()
        .flatten()
        .filter(|s| g.reaches(s))
        .cloned()
        .collect();
    Ok(Json(Value::Array(mine)))
}

async fn get_session(
    State(ctx): State<Ctx>,
    Extension(g): Extension<Grant>,
    Path(id): Path<String>,
) -> R<Json<Value>> {
    need(&g, Right::Read)?;
    Ok(Json(reached(&ctx, &g, &id).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewSession {
    profile: String,
    /// A directory under the workbench's `work/`, by name; a new empty one
    /// when not given.
    workdir: Option<String>,
    #[serde(default)]
    labels: BTreeMap<String, String>,
    #[serde(default)]
    persistent: bool,
    #[serde(default)]
    ssh: bool,
    #[serde(default)]
    rules: Vec<Value>,
}

/// The labels the workbench gives what it creates.
pub const LABEL: (&str, &str) = ("workbench", "1");

/// The directory a new session's `{workdir}` is: `name` under `work`, which
/// must be there, or a new empty one.
fn workdir(work: &std::path::Path, name: Option<&str>) -> R<PathBuf> {
    match name {
        Some(n) => {
            let plain = !n.is_empty()
                && n.len() <= 64
                && !n.starts_with('.')
                && n.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b));
            let dir = work.join(n);
            if !plain || !dir.is_dir() {
                return Err(Error(
                    StatusCode::BAD_REQUEST,
                    format!("workdir {n:?}: the name of a directory in the workbench's work/"),
                ));
            }
            Ok(dir)
        }
        None => {
            let mut b = [0u8; 6];
            ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut b)
                .map_err(|_| anyhow::anyhow!("no randomness"))?;
            let name: String = b.iter().map(|b| format!("{b:02x}")).collect();
            let dir = work.join(format!("new-{name}"));
            std::fs::create_dir_all(&dir).map_err(anyhow::Error::from)?;
            Ok(dir)
        }
    }
}

async fn create_session(
    State(ctx): State<Ctx>,
    Extension(g): Extension<Grant>,
    Json(new): Json<NewSession>,
) -> R<Json<Value>> {
    need(&g, Right::Create)?;
    if !new.rules.is_empty() {
        need(&g, Right::Rules)?;
    }
    if new.rules.iter().any(crate::secrets::names_a_secret) {
        need(&g, Right::Secrets)?;
    }
    let mut labels = new.labels;
    for (k, v) in g
        .labels
        .iter()
        .chain([(&LABEL.0.to_string(), &LABEL.1.to_string())])
    {
        match labels.get(k) {
            Some(theirs) if theirs != v => {
                return Err(Error(
                    StatusCode::BAD_REQUEST,
                    format!("label {k} is {v} for sessions this token creates"),
                ))
            }
            _ => {
                labels.insert(k.clone(), v.clone());
            }
        }
    }
    let dir = workdir(&ctx.work, new.workdir.as_deref())?;
    let body = json!({
        "profile": new.profile,
        "workdir": dir,
        "labels": labels,
        "persistent": new.persistent,
        "ssh": new.ssh,
        "rules": new.rules,
    });
    Ok(Json(
        ctx.zapper
            .call(Method::POST, "/v1/sessions", Some(&body))
            .await?
            .ok()?,
    ))
}

async fn stop_session(
    State(ctx): State<Ctx>,
    Extension(g): Extension<Grant>,
    Path(id): Path<String>,
) -> R<Json<Value>> {
    need(&g, Right::Control)?;
    reached(&ctx, &g, &id).await?;
    Ok(Json(
        ctx.zapper
            .call(Method::POST, &format!("/v1/sessions/{id}/stop"), None)
            .await?
            .ok()?,
    ))
}

#[derive(Deserialize, Default)]
struct StartSession {
    #[serde(default)]
    accept_changed_disks: bool,
}

async fn start_session(
    State(ctx): State<Ctx>,
    Extension(g): Extension<Grant>,
    Path(id): Path<String>,
    body: Option<Json<StartSession>>,
) -> R<Json<Value>> {
    need(&g, Right::Control)?;
    reached(&ctx, &g, &id).await?;
    let accept = body.map(|b| b.0.accept_changed_disks).unwrap_or_default();
    // Booting disks changed while stopped is the operator's decision.
    if accept && g != Grant::owner() {
        return Err(Error(
            StatusCode::FORBIDDEN,
            "only the owner's token accepts changed disks".into(),
        ));
    }
    let body = json!({ "accept_changed_disks": accept });
    Ok(Json(
        ctx.zapper
            .call(
                Method::POST,
                &format!("/v1/sessions/{id}/start"),
                Some(&body),
            )
            .await?
            .ok()?,
    ))
}

async fn remove_session(
    State(ctx): State<Ctx>,
    Extension(g): Extension<Grant>,
    Path(id): Path<String>,
) -> R<Json<Value>> {
    need(&g, Right::Control)?;
    reached(&ctx, &g, &id).await?;
    Ok(Json(
        ctx.zapper
            .call(Method::DELETE, &format!("/v1/sessions/{id}"), None)
            .await?
            .ok()?,
    ))
}

/// A session's read-only views, as zapper has them: `audit`, `verify`,
/// `requests`, `terminals`.
async fn session_view(
    State(ctx): State<Ctx>,
    Extension(g): Extension<Grant>,
    Path((id, view)): Path<(String, String)>,
) -> R<Response> {
    need(&g, Right::Read)?;
    if !["audit", "verify", "requests", "terminals"].contains(&view.as_str()) {
        return Err(Error(StatusCode::NOT_FOUND, format!("no view {view}")));
    }
    reached(&ctx, &g, &id).await?;
    passed_on(
        &ctx,
        &format!("/v1/sessions/{id}/{view}"),
        &HeaderMap::new(),
    )
    .await
}

/// One of a session's requests, or one of its terminals' output.
async fn session_item(
    State(ctx): State<Ctx>,
    Extension(g): Extension<Grant>,
    Path((id, view, n)): Path<(String, String, u64)>,
) -> R<Response> {
    need(&g, Right::Read)?;
    let path = match view.as_str() {
        "requests" => format!("/v1/sessions/{id}/requests/{n}"),
        "terminals" => format!("/v1/sessions/{id}/terminals/{n}/stream"),
        _ => return Err(Error(StatusCode::NOT_FOUND, format!("no view {view}"))),
    };
    reached(&ctx, &g, &id).await?;
    passed_on(&ctx, &path, &HeaderMap::new()).await
}

// Rules, in a session's own layer only.

async fn session_rules(
    State(ctx): State<Ctx>,
    Extension(g): Extension<Grant>,
    Path(id): Path<String>,
) -> R<Json<Value>> {
    need(&g, Right::Read)?;
    reached(&ctx, &g, &id).await?;
    Ok(Json(
        ctx.zapper
            .call(Method::GET, &format!("/v1/rules?scope=session:{id}"), None)
            .await?
            .ok()?,
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewRule {
    rule: Value,
    position: Option<usize>,
}

async fn add_rule(
    State(ctx): State<Ctx>,
    Extension(g): Extension<Grant>,
    Path(id): Path<String>,
    Json(new): Json<NewRule>,
) -> R<Json<Value>> {
    need(&g, Right::Rules)?;
    // A rule that names a secret has the session use it: the secrets right.
    if crate::secrets::names_a_secret(&new.rule) {
        need(&g, Right::Secrets)?;
    }
    reached(&ctx, &g, &id).await?;
    let body =
        json!({ "scope": format!("session:{id}"), "rule": new.rule, "position": new.position });
    Ok(Json(
        ctx.zapper
            .call(Method::POST, "/v1/rules", Some(&body))
            .await?
            .ok()?,
    ))
}

async fn remove_rule(
    State(ctx): State<Ctx>,
    Extension(g): Extension<Grant>,
    Path((id, rule)): Path<(String, String)>,
) -> R<Json<Value>> {
    need(&g, Right::Rules)?;
    reached(&ctx, &g, &id).await?;
    // Only a rule of this session's own layer: zapper's route removes one
    // from any layer.
    let own = ctx
        .zapper
        .call(Method::GET, &format!("/v1/rules?scope=session:{id}"), None)
        .await?
        .ok()?;
    let theirs = own
        .as_array()
        .into_iter()
        .flatten()
        .any(|r| r["id"] == rule.as_str());
    if !theirs {
        return Err(Error(
            StatusCode::NOT_FOUND,
            format!("session {id} has no rule {rule}"),
        ));
    }
    Ok(Json(
        ctx.zapper
            .call(Method::DELETE, &format!("/v1/rules/{rule}"), None)
            .await?
            .ok()?,
    ))
}

// Terminals.

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AttachQuery {
    tty: Option<String>,
    argv: Option<String>,
    #[serde(default)]
    persist: bool,
    terminal: Option<u64>,
}

/// A terminal in the session, as the token's: run as the workload user
/// (never `system`), its events naming the token.
async fn attach(
    State(ctx): State<Ctx>,
    Extension(g): Extension<Grant>,
    Path(id): Path<String>,
    Query(q): Query<AttachQuery>,
    req: Request,
) -> R<Response> {
    need(&g, Right::Terminal)?;
    reached(&ctx, &g, &id).await?;
    let mut query = vec![format!("client=workbench:{}", url_encode(&g.name))];
    if let Some(t) = &q.tty {
        query.push(format!("tty={}", url_encode(t)));
    }
    if let Some(a) = &q.argv {
        query.push(format!("argv={}", url_encode(a)));
    }
    if q.persist {
        query.push("persist=true".into());
    }
    if let Some(n) = q.terminal {
        query.push(format!("terminal={n}"));
    }
    let (guest, n) = ctx
        .zapper
        .upgrade(&format!("/v1/sessions/{id}/attach?{}", query.join("&")))
        .await
        .map_err(|e| Error(StatusCode::BAD_GATEWAY, format!("{e:#}")))?;
    let on_upgrade = hyper::upgrade::on(req);
    tokio::spawn(async move {
        if let Ok(up) = on_upgrade.await {
            let mut client = hyper_util::rt::TokioIo::new(up);
            let mut guest = guest;
            let _ = tokio::io::copy_bidirectional(&mut client, &mut guest).await;
        }
    });
    let mut resp = Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(header::CONNECTION, "upgrade")
        .header(header::UPGRADE, "zapper-shell");
    if let Some(n) = n {
        resp = resp.header("x-zapper-terminal", n);
    }
    Ok(resp.body(Body::empty()).unwrap())
}

// Approvals.

async fn approvals(State(ctx): State<Ctx>, Extension(g): Extension<Grant>) -> R<Json<Value>> {
    need(&g, Right::Read)?;
    let all = ctx
        .zapper
        .call(Method::GET, "/v1/approvals", None)
        .await?
        .ok()?;
    let mut reach = Reach::new(&ctx, &g);
    let mut mine = vec![];
    for a in all.as_array().into_iter().flatten() {
        if reach.session(a["session"].as_str()).await {
            mine.push(a.clone());
        }
    }
    Ok(Json(Value::Array(mine)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Answer {
    allow: bool,
    remember: Option<String>,
}

async fn answer(
    State(ctx): State<Ctx>,
    Extension(g): Extension<Grant>,
    Path(id): Path<String>,
    Json(a): Json<Answer>,
) -> R<Json<Value>> {
    need(&g, Right::Answer)?;
    let all = ctx
        .zapper
        .call(Method::GET, "/v1/approvals", None)
        .await?
        .ok()?;
    let session = all
        .as_array()
        .into_iter()
        .flatten()
        .find(|p| p["id"] == id.as_str())
        .and_then(|p| p["session"].as_str().map(str::to_string));
    let missing = || Error(StatusCode::NOT_FOUND, format!("no approval {id}"));
    let session = session.ok_or_else(missing)?;
    if !Reach::new(&ctx, &g).session(Some(&session)).await {
        return Err(missing());
    }
    let body = json!({ "allow": a.allow, "remember": a.remember });
    Ok(Json(
        ctx.zapper
            .call(
                Method::POST,
                &format!("/v1/approvals/{}", url_encode(&id)),
                Some(&body),
            )
            .await?
            .ok()?,
    ))
}

/// Which sessions a grant reaches, asked of zapper once each.
struct Reach<'a> {
    ctx: &'a Ctx,
    g: &'a Grant,
    known: HashMap<String, bool>,
}

impl<'a> Reach<'a> {
    fn new(ctx: &'a Ctx, g: &'a Grant) -> Self {
        Reach {
            ctx,
            g,
            known: HashMap::new(),
        }
    }

    /// Whether the grant reaches `session`; no session is reached by a
    /// grant that reaches all only.
    async fn session(&mut self, session: Option<&str>) -> bool {
        let Some(id) = session else {
            return self.g.reaches_all();
        };
        if self.g.reaches_all() {
            return true;
        }
        if let Some(&r) = self.known.get(id) {
            return r;
        }
        let r = reached(self.ctx, self.g, id).await.is_ok();
        self.known.insert(id.to_string(), r);
        r
    }
}

// Events.

#[derive(Deserialize)]
struct EventsQuery {
    session: Option<String>,
    #[serde(default)]
    history: bool,
    after: Option<String>,
}

/// zapper's events, as server-sent events, those of the token's sessions
/// only; resumed through zapper's cursors (`after=`, or `Last-Event-ID`),
/// which pass through as they are, so a restart of the gateway loses no
/// client's place.
async fn events(
    State(ctx): State<Ctx>,
    Extension(g): Extension<Grant>,
    Query(q): Query<EventsQuery>,
    headers: HeaderMap,
) -> R<Response> {
    need(&g, Right::Read)?;
    let mut query = vec![];
    if let Some(s) = &q.session {
        reached(&ctx, &g, s).await?;
        query.push(format!("session={s}"));
    }
    if q.history {
        query.push("history=true".into());
    }
    if let Some(a) = &q.after {
        query.push(format!("after={}", url_encode(a)));
    }
    let mut pass = HeaderMap::new();
    if let Some(v) = headers.get("last-event-id") {
        pass.insert("last-event-id", v.clone());
    }
    let resp = ctx
        .zapper
        .stream(&format!("/v1/events?{}", query.join("&")), &pass)
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let bytes = http_body_util::BodyExt::collect(resp.into_body())
            .await
            .map(|c| c.to_bytes())
            .unwrap_or_default();
        return Err(Error(status, String::from_utf8_lossy(&bytes).into_owned()));
    }
    let sse_response = |body: Body| {
        Response::builder()
            .header(header::CONTENT_TYPE, "text/event-stream")
            .header(header::CACHE_CONTROL, "no-cache")
            .body(body)
            .unwrap()
    };
    // One session's, or every one's for a grant that reaches all: as they come.
    if q.session.is_some() || g.reaches_all() {
        return Ok(sse_response(Body::new(resp.into_body())));
    }
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(64);
    tokio::spawn(async move {
        let mut blocks = sse::Blocks::new(resp.into_body());
        let mut reach = Reach::new(&ctx, &g);
        while let Some(block) = blocks.next().await {
            let keep = match block.json() {
                // Keep-alives, and what isn't an event.
                None => true,
                Some(e) if e["type"] == "gap" => true,
                Some(e) => reach.session(e["session"].as_str()).await,
            };
            if keep && tx.send(Ok(block.bytes())).await.is_err() {
                return;
            }
        }
    });
    Ok(sse_response(Body::from_stream(
        tokio_stream::wrappers::ReceiverStream::new(rx),
    )))
}

/// Only the owner's token: zapper's config as a whole is the operator's.
fn owner(g: &Grant) -> R<()> {
    if *g == Grant::owner() {
        Ok(())
    } else {
        Err(Error(
            StatusCode::FORBIDDEN,
            "only the owner's token reads or saves zapper's config".into(),
        ))
    }
}

/// What saving would change in zapper's startup file.
async fn config_diff(State(ctx): State<Ctx>, Extension(g): Extension<Grant>) -> R<Json<Value>> {
    owner(&g)?;
    let d = ctx
        .zapper
        .call(Method::GET, "/v1/config/diff", None)
        .await?
        .ok()?;
    Ok(Json(json!({ "diff": d })))
}

/// Saves zapper's running config to its startup file: everything running,
/// the facilities' changes and any other.
async fn config_save(State(ctx): State<Ctx>, Extension(g): Extension<Grant>) -> R<Json<Value>> {
    owner(&g)?;
    Ok(Json(
        ctx.zapper
            .call(Method::POST, "/v1/config/save", None)
            .await?
            .ok()?,
    ))
}

async fn facilities(State(ctx): State<Ctx>) -> Json<Value> {
    Json(ctx.facilities.view())
}

/// A facility's panel's script, `NAME.js`.
async fn panel(State(ctx): State<Ctx>, Path(file): Path<String>) -> R<Response> {
    let script = file
        .strip_suffix(".js")
        .and_then(|name| ctx.facilities.panel_script(name))
        .ok_or_else(|| Error(StatusCode::NOT_FOUND, format!("no panel {file}")))?;
    Ok((
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        script,
    )
        .into_response())
}

// The UI.

const PAGE: &str = include_str!("ui.html");

async fn page() -> Response {
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], PAGE).into_response()
}

fn local_hosts(port: u16) -> [String; 3] {
    [
        format!("127.0.0.1:{port}"),
        format!("localhost:{port}"),
        format!("[::1]:{port}"),
    ]
}

/// Whether a request's `Host` may reach the UI: a local name on loopback,
/// which defeats DNS rebinding; listening elsewhere, any well-formed host.
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

/// Whether a write's `Origin` is the UI itself.
fn origin_ok(origin: &str, host: &str, port: u16, remote: bool) -> bool {
    if remote {
        origin == format!("http://{host}") || origin == format!("https://{host}")
    } else {
        local_hosts(port)
            .iter()
            .any(|l| origin == format!("http://{l}"))
    }
}

const COOKIE: &str = "workbench_token";

fn host_of(req_headers: &HeaderMap) -> &str {
    req_headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("")
}

/// A browser's request reaches the UI only by a name it may have
/// (`host_ok`); one that changes something only with the `X-Workbench-UI`
/// header, which another site's page can't send without a preflight that
/// is never granted, and an `Origin` naming the UI (`origin_ok`).
fn from_the_ui(ctx: &Ctx, method: &Method, headers: &HeaderMap) -> Result<(), Error> {
    let host = host_of(headers);
    if !host_ok(host, ctx.port, ctx.remote) {
        return Err(Error(StatusCode::FORBIDDEN, "bad Host header".into()));
    }
    if !matches!(*method, Method::GET | Method::HEAD) {
        let origin = headers
            .get(header::ORIGIN)
            .and_then(|h| h.to_str().ok())
            .unwrap_or("");
        let marked = headers.get("x-workbench-ui").is_some_and(|v| v == "1");
        if !marked || !origin_ok(origin, host, ctx.port, ctx.remote) {
            return Err(Error(
                StatusCode::FORBIDDEN,
                "missing X-Workbench-UI or the UI's Origin".into(),
            ));
        }
    }
    Ok(())
}

/// The browser's grant: its cookie's token, set at sign-in (`login`).
fn browser(ctx: &Ctx, req: &Request) -> Result<Grant, Error> {
    from_the_ui(ctx, req.method(), req.headers())?;
    let offered = req
        .headers()
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|c| c.to_str().ok())
        .flat_map(|c| c.split(';'))
        .find_map(|c| {
            c.trim()
                .strip_prefix(&format!("{COOKIE}="))
                .map(str::to_string)
        })
        .unwrap_or_default();
    ctx.tokens.grant(&offered).ok_or_else(|| {
        Error(
            StatusCode::UNAUTHORIZED,
            "not signed in: sign in at /login".into(),
        )
    })
}

/// Authenticates a request. Automation sends its token as `Authorization:
/// Bearer`, which a browser never sends by itself. The UI's browser sends
/// the cookie it was given at sign-in, checked as zapper's UI is
/// (`from_the_ui`). A token never travels in an address, where it would
/// be kept in histories and logs.
async fn guard(State(ctx): State<Ctx>, mut req: Request, next: Next) -> Response {
    let bearer = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .map(str::to_string);
    let grant = match bearer {
        Some(token) => match ctx.tokens.grant(token.trim()) {
            Some(g) => g,
            None => return Error(StatusCode::UNAUTHORIZED, "no such token".into()).into_response(),
        },
        None => match browser(&ctx, &req) {
            Ok(g) => g,
            // The page itself, not signed in: to the sign-in page.
            Err(Error(StatusCode::UNAUTHORIZED, _)) if req.uri().path() == "/" => {
                return Redirect::to("/login").into_response()
            }
            Err(e) => return e.into_response(),
        },
    };
    req.extensions_mut().insert(grant);
    next.run(req).await
}

/// What every response carries: no framing, no sniffing, no referrer, and
/// scripts and styles from the workbench only.
async fn hardened(req: Request, next: Next) -> Response {
    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    h.insert("content-security-policy", "default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; frame-ancestors 'none'".parse().unwrap());
    h.insert("x-content-type-options", "nosniff".parse().unwrap());
    h.insert("referrer-policy", "no-referrer".parse().unwrap());
    h.insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    resp
}

const LOGIN: &str = include_str!("login.html");

async fn login_page(State(ctx): State<Ctx>, headers: HeaderMap) -> R<Response> {
    from_the_ui(&ctx, &Method::GET, &headers)?;
    Ok(([(header::CONTENT_TYPE, "text/html; charset=utf-8")], LOGIN).into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SignIn {
    token: String,
}

/// Signs the browser in with the token it was given, sent in the body:
/// the cookie, if it is a token. A write from the UI like any other.
async fn login(State(ctx): State<Ctx>, headers: HeaderMap, Json(s): Json<SignIn>) -> R<Response> {
    from_the_ui(&ctx, &Method::POST, &headers)?;
    let token = s.token.trim();
    let Some(g) = ctx.tokens.grant(token) else {
        return Err(Error(StatusCode::UNAUTHORIZED, "not a token".into()));
    };
    let cookie = format!("{COOKIE}={token}; HttpOnly; SameSite=Strict; Path=/");
    Ok((
        [(header::SET_COOKIE, cookie)],
        Json(json!({ "token": g.name })),
    )
        .into_response())
}

/// Signs the browser out: its cookie gone.
async fn logout(State(ctx): State<Ctx>, headers: HeaderMap) -> R<Response> {
    from_the_ui(&ctx, &Method::POST, &headers)?;
    let cookie = format!("{COOKIE}=; Max-Age=0; HttpOnly; SameSite=Strict; Path=/");
    Ok(([(header::SET_COOKIE, cookie)], Json(json!({}))).into_response())
}

/// The workbench's API and UI, behind the gateway.
pub fn app(ctx: Ctx) -> Router {
    let api = Router::new()
        .route("/status", get(status))
        .route("/profiles", get(profiles))
        .route("/sessions", get(list_sessions).post(create_session))
        .route("/sessions/{id}", get(get_session).delete(remove_session))
        .route("/sessions/{id}/stop", post(stop_session))
        .route("/sessions/{id}/start", post(start_session))
        .route("/sessions/{id}/rules", get(session_rules).post(add_rule))
        .route("/sessions/{id}/rules/{rule}", delete(remove_rule))
        .route("/sessions/{id}/attach", get(attach))
        .route("/sessions/{id}/{view}", get(session_view))
        .route("/sessions/{id}/{view}/{n}", get(session_item))
        .route("/approvals", get(approvals))
        .route("/approvals/{id}", post(answer))
        .route("/events", get(events))
        .route("/facilities", get(facilities))
        .route("/config/diff", get(config_diff))
        .route("/config/save", post(config_save))
        .merge(ctx.facilities.routes());
    let guarded = Router::new()
        .route("/", get(page))
        .route("/panels/{file}", get(panel))
        .nest("/w/v1", api)
        .layer(middleware::from_fn_with_state(ctx.clone(), guard));
    Router::new()
        .route("/login", get(login_page).post(login))
        .route("/logout", post(logout))
        .merge(guarded)
        .layer(middleware::from_fn(hardened))
        .with_state(ctx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_loopback_ui_takes_local_names_only() {
        assert!(host_ok("127.0.0.1:7880", 7880, false));
        assert!(host_ok("localhost:7880", 7880, false));
        assert!(!host_ok("evil.example:7880", 7880, false), "DNS rebinding");
        assert!(!host_ok("127.0.0.1:7881", 7880, false));
        assert!(origin_ok(
            "http://localhost:7880",
            "localhost:7880",
            7880,
            false
        ));
        assert!(!origin_ok(
            "http://evil.example",
            "localhost:7880",
            7880,
            false
        ));
        assert!(host_ok("wb.example:443", 7880, true));
        assert!(origin_ok("https://wb.example", "wb.example", 7880, true));
        assert!(!origin_ok("https://evil.example", "wb.example", 7880, true));
    }

    #[test]
    fn a_workdir_is_one_of_the_workbenchs_directories_or_a_new_one() {
        let work = std::env::temp_dir().join(format!("zapper-wb-work-{}", std::process::id()));
        std::fs::create_dir_all(work.join("proj")).unwrap();
        assert_eq!(workdir(&work, Some("proj")).unwrap(), work.join("proj"));
        for bad in ["../etc", "/etc", ".hidden", "missing", "a/b", ""] {
            assert!(workdir(&work, Some(bad)).is_err(), "{bad}");
        }
        let fresh = workdir(&work, None).unwrap();
        assert!(fresh.is_dir() && fresh.starts_with(&work));
        let _ = std::fs::remove_dir_all(work);
    }
}
