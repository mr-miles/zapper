//! REST routes for firms, under `/v1/firms` on the API socket and `/api/firms`
//! in the UI. Reads serve both; of the writes, the UI gets the board's own:
//! mail, firm up/down/pause, and security decisions.

use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::firm::BOARD;
use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use futures_util::stream::Stream;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{drive, security};
use crate::Office;
use zapper_core::ApiError;

/// The office's own state: the handlers reach firms, runs and the running
/// config through it (`run`, `active`, `firm_dir`, `client().config()`),
/// never through a `Core`. Co-hosted it is `Core::office()`; a standalone
/// office binds its own (P4-d).
type S = State<Arc<Office>>;
type R<T> = Result<T, ApiError>;

/// A running firm resolved from the request's `{f}` path segment: the
/// handler takes `Active(run)` instead of repeating
/// `let run = office.active(&f)?;` after `State(office), Path(f)`. Errors the
/// same way that preamble did (no such firm, or not running). Handlers that
/// still need the office state keep `State(office)` alongside it.
struct Active(Arc<crate::FirmRun>);

impl axum::extract::FromRequestParts<Arc<Office>> for Active {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &Arc<Office>,
    ) -> Result<Self, ApiError> {
        let Path(params) =
            Path::<std::collections::HashMap<String, String>>::from_request_parts(parts, state)
                .await
                .map_err(|e| bad(e.to_string()))?;
        let f = params
            .get("f")
            .ok_or_else(|| bad("no firm in the request path"))?;
        Ok(Active(state.active(f)?))
    }
}

fn bad(msg: impl Into<String>) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, msg.into())
}

fn body(text: &str) -> R<Value> {
    if text.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_yaml::from_str(text).map_err(|e| bad(format!("body: {e}")))
}

pub fn read_routes() -> Router<Arc<Office>> {
    Router::new()
        .route("/firms", get(list))
        .route("/firms/{f}", get(status))
        .route("/firms/{f}/runs", get(runs))
        .route("/firms/{f}/runs/{run}/journal", get(journal))
        .route("/firms/{f}/runs/{run}/config", get(run_config))
        .route("/firms/{f}/runs/{run}/verify", get(run_verify))
        .route(
            "/firms/{f}/runs/{run}/transcripts/{worker}",
            get(transcript),
        )
        .route(
            "/firms/{f}/runs/{run}/drive/{artifact}/{version}/{*path}",
            get(drive_file),
        )
        .route("/firms/{f}/runs/{run}/git", get(git_log))
        .route("/firms/{f}/runs/{run}/git/repos", get(git_repos))
        .route("/firms/{f}/runs/{run}/git/branches", get(git_branches))
        .route("/firms/{f}/runs/{run}/git/log", get(git_ref_log))
        .route("/firms/{f}/runs/{run}/git/tree", get(git_tree))
        .route("/firms/{f}/runs/{run}/git/blob", get(git_blob))
        .route("/firms/{f}/runs/{run}/git/commits/{id}", get(git_commit))
        .route("/firms/{f}/scenarios", get(scenarios))
        .route("/firms/{f}/mail", get(inbox))
        .route("/firms/{f}/threads/{id}", get(thread))
        .route("/firms/{f}/drive", get(drive_list))
        .route("/firms/{f}/security", get(security_state))
        .route("/firms/{f}/credentials", get(credentials_state))
}

/// The board's writes, in the API and the UI: including deleting a past run
/// and notes on runs.
pub fn board_routes() -> Router<Arc<Office>> {
    Router::new()
        // The task's attachments ride in the body, base64 like an upload's.
        .route(
            "/firms/{f}/up",
            post(up).layer(axum::extract::DefaultBodyLimit::max(
                drive::BOARD_UPLOAD_MAX / 3 * 4 + (256 << 10),
            )),
        )
        .route("/firms/{f}/down", post(down))
        .route("/firms/{f}/pause", post(pause))
        .route("/firms/{f}/mail", post(send))
        .route("/firms/{f}/mail/read", post(mark_read))
        // Base64 grows a file by a third; leave room for the JSON around it.
        .route(
            "/firms/{f}/drive/upload",
            post(upload).layer(axum::extract::DefaultBodyLimit::max(
                drive::BOARD_UPLOAD_MAX / 3 * 4 + (64 << 10),
            )),
        )
        .route("/firms/{f}/security/requests/{id}", post(decide))
        .route("/firms/{f}/security/rules/{id}", delete(revoke))
        .route("/firms/{f}/security/rules/{id}/promote", post(promote))
        .route("/firms/{f}/runs/{run}", delete(prune))
        .route("/firms/{f}/runs/{run}/note", put(run_note))
        .route("/firms/{f}/budget/{id}", post(budget_decide))
        .route("/firms/{f}/release", post(board_release))
        .route("/firms/{f}/fund", post(board_fund))
        .route(
            "/firms/{f}/security/screenings/{id}",
            post(screening_decide),
        )
        .route(
            "/firms/{f}/credentials/requests/{id}",
            post(credentials_decide),
        )
        .route("/firms/{f}/credentials/grants", post(credentials_grant))
        .route(
            "/firms/{f}/credentials/grants/{worker}/{resource}",
            delete(credentials_revoke),
        )
}

async fn list(State(office): S) -> Json<Value> {
    let config = office.client().config();
    let out: Vec<Value> = crate::firm::all(&config)
        .iter()
        .map(|(name, f)| {
            let run = office.run(name).ok();
            json!({
                "name": name, "description": f.description, "workers": f.workers.len(),
                "run": run.as_ref().map(|r| r.id.clone()), "state": run.as_ref().map(|r| json!(*r.state.lock().unwrap())),
            })
        })
        .collect();
    Json(json!(out))
}

async fn status(State(office): S, Path(f): Path<String>) -> R<Json<Value>> {
    let config = office.client().config();
    let run = office.run(&f).ok();
    // A running firm's chart is its live one (the founders it booted with,
    // and its hires), whatever the config says now.
    let booted = run.as_ref().filter(|r| !r.ended()).map(|r| r.chart());
    let configured = crate::firm::get(&config, &f);
    let firm = match (&booted, &configured) {
        (Some(b), _) => b,
        (None, Some(c)) => c,
        (None, None) => return Err(anyhow::anyhow!("no firm {f}").into()),
    };
    let live = run.as_ref().filter(|r| !r.ended());
    Ok(Json(json!({
        "name": f,
        "company": live.map(|r| r.company.clone()).unwrap_or_else(|| super::default_company(&f, firm)),
        "ceo": firm.root(),
        "officer": firm.security.officers.iter().find(|o| Some(o.as_str()) != firm.root()),
        "officers": firm.security.officers,
        "seed": configured.as_ref().map(|x| x.budget.seed).unwrap_or(firm.budget.seed),
        "start": configured.as_ref().map(|x| start_terms(x, &config)),
        "articles": live.map(|r| r.articles.clone()),
        "description": firm.description,
        "board": firm.board.name,
        "product_port": firm.product_port,
        "dashboard_port": firm.dashboard_port,
        // Whether it has repositories of its own (`git.repos`), for the Git tab.
        "local_git": firm.git_internal(),
        "chart": firm.workers.iter().map(|w| json!({
            "name": w.name, "role": w.role, "title": firm.title_of(&w.name), "reports_to": w.reports_to,
            "officer": firm.is_officer(&w.name), "avatar": firm.roles.get(&w.role).map(|r| json!(r.avatar)),
            "model": firm.roles.get(&w.role).and_then(|r| super::effective_model(r, &config)),
        })).collect::<Vec<_>>(),
        "run": run.map(|r| r.status()),
    })))
}

/// The firm's scenarios as configured.
async fn scenarios(State(office): S, Path(f): Path<String>) -> R<Json<Value>> {
    let config = office.client().config();
    let firm = crate::firm::get(&config, &f).ok_or_else(|| anyhow::anyhow!("no firm {f}"))?;
    Ok(Json(json!(firm.scenarios)))
}

/// What the board's start form offers: the config's founders and terms,
/// and the charter.
fn start_terms(firm: &crate::firm::Firm, config: &zapper_policy::Config) -> Value {
    // Profiles a scenario's hire slot may put hires on: those an office
    // worker can run on, with their agent and model.
    let profiles: Vec<Value> = config
        .profiles
        .keys()
        .filter(|n| crate::firm::office_profile_problem(config, n).is_none())
        .filter_map(|n| {
            let p = config.resolve(n).ok()?;
            Some(json!({ "name": n, "agent": p.agent.kind, "model": p.agent.model, "description": config.profiles[n].description }))
        })
        .collect();
    let root = firm.root();
    json!({
        "ceo": root,
        "officer_role": firm.roles.values().any(|r| r.officer),
        "seed": firm.budget.seed,
        "security_share": firm.budget.security_share,
        "max_concurrent": firm.budget.max_concurrent,
        "daily_turns": firm.budget.daily_turns,
        "scenarios": firm.scenarios,
        "profiles": profiles,
        "roles": firm.roles.iter().map(|(n, r)| json!({ "name": n, "title": r.title, "officer": r.officer })).collect::<Vec<_>>(),
        "charter": super::prompts::CHARTER.iter().map(|a| json!({ "id": a.id, "title": a.title, "text": a.text })).collect::<Vec<_>>(),
    })
}

/// Checks a run's record against the keeper's chains and signatures.
async fn run_verify(State(office): S, Path((f, run)): Path<(String, String)>) -> R<Json<Value>> {
    let dir = super::run_dir(&office, &f, &run)?;
    Ok(Json(
        tokio::task::spawn_blocking(move || zapper_core::record::verify_state_dir(&dir))
            .await
            .map_err(|e| bad(e.to_string()))??,
    ))
}

/// The config a run started with (secret values hidden) and its skills.
async fn run_config(State(office): S, Path((f, run)): Path<(String, String)>) -> R<Json<Value>> {
    Ok(Json(super::run_config(&super::run_dir(&office, &f, &run)?)))
}

async fn runs(State(office): S, Path(f): Path<String>) -> R<Json<Value>> {
    if !office.client().config().firms.contains_key(&f) && !office.firm_dir(&f).is_dir() {
        return Err(anyhow::anyhow!("no firm {f}").into());
    }
    Ok(Json(json!(super::list_runs(&office, &f))))
}

#[derive(Deserialize, Default)]
struct Follow {
    #[serde(default)]
    follow: bool,
    /// Byte offset to start from.
    #[serde(default)]
    from: u64,
}

/// A JSON-lines file, whole or followed as server-sent events.
async fn lines(path: PathBuf, q: Follow) -> R<Response> {
    if !q.follow {
        let text = tokio::fs::read_to_string(&path).await.map_err(|_| {
            ApiError(
                StatusCode::NOT_FOUND,
                format!(
                    "no {}",
                    path.file_name().unwrap_or_default().to_string_lossy()
                ),
            )
        })?;
        return Ok(([(header::CONTENT_TYPE, "application/x-ndjson")], text).into_response());
    }
    Ok(Sse::new(tail(path, q.from))
        .keep_alive(KeepAlive::default())
        .into_response())
}

/// Follows a file: complete lines as they are written.
fn tail(path: PathBuf, from: u64) -> impl Stream<Item = Result<SseEvent, Infallible>> {
    futures_util::stream::unfold(
        (
            path,
            from,
            Vec::<u8>::new(),
            std::collections::VecDeque::<String>::new(),
        ),
        |(path, mut offset, mut partial, mut ready)| async move {
            loop {
                if let Some(line) = ready.pop_front() {
                    return Some((
                        Ok(SseEvent::default().data(line)),
                        (path, offset, partial, ready),
                    ));
                }
                use tokio::io::{AsyncReadExt, AsyncSeekExt};
                if let Ok(mut f) = tokio::fs::File::open(&path).await {
                    if f.seek(std::io::SeekFrom::Start(offset)).await.is_ok() {
                        let mut buf = Vec::new();
                        if let Ok(n) = (&mut f).take(4 << 20).read_to_end(&mut buf).await {
                            offset += n as u64;
                            partial.extend_from_slice(&buf);
                            while let Some(i) = partial.iter().position(|c| *c == b'\n') {
                                let line: Vec<u8> = partial.drain(..=i).collect();
                                let s = String::from_utf8_lossy(&line).trim().to_string();
                                if !s.is_empty() {
                                    ready.push_back(s);
                                }
                            }
                        }
                    }
                }
                if ready.is_empty() {
                    tokio::time::sleep(Duration::from_millis(400)).await;
                }
            }
        },
    )
}

async fn journal(
    State(office): S,
    Path((f, run)): Path<(String, String)>,
    Query(q): Query<Follow>,
) -> R<Response> {
    let dir = super::run_dir(&office, &f, &run)?;
    lines(zapper_core::record::view(&dir.join("journal.jsonl")), q).await
}

async fn transcript(
    State(office): S,
    Path((f, run, worker)): Path<(String, String, String)>,
    Query(q): Query<Follow>,
) -> R<Response> {
    let dir = super::run_dir(&office, &f, &run)?;
    if !zapper_policy::valid_worker_name(&worker) {
        return Err(bad(format!("{worker:?} is not a worker name")));
    }
    lines(
        zapper_core::record::view(&dir.join("transcripts").join(format!("{worker}.jsonl"))),
        q,
    )
    .await
}

/// A published file. Served as its own type with a sandboxing CSP, so a
/// published page or SVG can't run script in the UI's origin.
async fn drive_file(
    State(office): S,
    Path((f, run, artifact, version, path)): Path<(String, String, String, String, String)>,
) -> R<Response> {
    let dir = super::run_dir(&office, &f, &run)?;
    let version: u32 = version
        .trim_start_matches('v')
        .parse()
        .map_err(|_| bad("bad version"))?;
    let (sha, _) = drive::find_file(&dir, &artifact, version, &path)
        .map_err(|e| ApiError(StatusCode::NOT_FOUND, format!("{e:#}")))?;
    let data = tokio::fs::read(dir.join("drive").join(&sha))
        .await
        .map_err(|_| ApiError(StatusCode::NOT_FOUND, "file missing".into()))?;
    Ok((
        [
            (header::CONTENT_TYPE, drive::content_type(&path)),
            (header::CONTENT_SECURITY_POLICY, "sandbox; default-src 'none'; img-src 'self' data:; style-src 'unsafe-inline'; frame-ancestors 'none'"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::CONTENT_DISPOSITION, "inline"),
        ],
        data,
    )
        .into_response())
}

async fn git_log(State(office): S, Path((f, run)): Path<(String, String)>) -> R<Json<Value>> {
    let dir = super::run_dir(&office, &f, &run)?;
    Ok(Json(super::facility::git_log(&dir).await))
}

#[derive(Deserialize)]
struct GitQuery {
    /// The repository; the run's first if unset.
    #[serde(default)]
    repo: Option<String>,
    #[serde(rename = "ref", default = "main_ref")]
    r: String,
    #[serde(default)]
    path: String,
    #[serde(default)]
    limit: Option<usize>,
}

fn main_ref() -> String {
    "main".into()
}

async fn git_repos(State(office): S, Path((f, run)): Path<(String, String)>) -> R<Json<Value>> {
    Ok(Json(json!(super::git::repos(&super::run_dir(
        &office, &f, &run
    )?))))
}

async fn git_branches(
    State(office): S,
    Path((f, run)): Path<(String, String)>,
    Query(q): Query<GitQuery>,
) -> R<Json<Value>> {
    Ok(Json(
        super::git::branches(&super::run_dir(&office, &f, &run)?, q.repo.as_deref()).await?,
    ))
}

async fn git_ref_log(
    State(office): S,
    Path((f, run)): Path<(String, String)>,
    Query(q): Query<GitQuery>,
) -> R<Json<Value>> {
    Ok(Json(
        super::git::log(
            &super::run_dir(&office, &f, &run)?,
            q.repo.as_deref(),
            &q.r,
            q.limit.unwrap_or(200),
        )
        .await?,
    ))
}

async fn git_tree(
    State(office): S,
    Path((f, run)): Path<(String, String)>,
    Query(q): Query<GitQuery>,
) -> R<Json<Value>> {
    Ok(Json(
        super::git::tree(
            &super::run_dir(&office, &f, &run)?,
            q.repo.as_deref(),
            &q.r,
            &q.path,
        )
        .await?,
    ))
}

async fn git_blob(
    State(office): S,
    Path((f, run)): Path<(String, String)>,
    Query(q): Query<GitQuery>,
) -> R<Json<Value>> {
    Ok(Json(
        super::git::blob(
            &super::run_dir(&office, &f, &run)?,
            q.repo.as_deref(),
            &q.r,
            &q.path,
        )
        .await?,
    ))
}

async fn git_commit(
    State(office): S,
    Path((f, run, id)): Path<(String, String, String)>,
    Query(q): Query<GitQuery>,
) -> R<Json<Value>> {
    Ok(Json(
        super::git::commit(&super::run_dir(&office, &f, &run)?, q.repo.as_deref(), &id).await?,
    ))
}

#[derive(Deserialize)]
struct InboxQuery {
    #[serde(rename = "for")]
    who: Option<String>,
    #[serde(default)]
    unread: bool,
}

async fn inbox(
    State(office): S,
    Path(f): Path<String>,
    Query(q): Query<InboxQuery>,
) -> R<Json<Value>> {
    let run = office.run(&f)?;
    let who = q.who.unwrap_or_else(|| BOARD.into());
    Ok(Json(json!(run
        .post
        .with(|po| po.inbox(&who, q.unread, 500)))))
}

async fn thread(State(office): S, Path((f, id)): Path<(String, String)>) -> R<Json<Value>> {
    let run = office.run(&f)?;
    Ok(Json(
        run.post.with(|po| po.thread(&run.journal, &id, BOARD))?,
    ))
}

async fn drive_list(State(office): S, Path(f): Path<String>) -> R<Json<Value>> {
    let run = office.run(&f)?;
    let list = run
        .drive
        .lock()
        .unwrap()
        .list(BOARD, &|_| true, None, None, None, None);
    Ok(Json(json!(list)))
}

async fn security_state(State(office): S, Path(f): Path<String>) -> R<Json<Value>> {
    let run = office.run(&f)?;
    let screenings: Vec<Value> = run
        .security
        .lock()
        .unwrap()
        .screenings
        .values()
        .map(|s| json!(s))
        .collect();
    Ok(Json(
        json!({ "requests": security::requests(&run, false), "standing": security::standing(&run), "screenings": screenings }),
    ))
}

/// Starts a run, with the board's parameters for it if given:
/// `{company?, ceo?, seed?}`.
async fn up(State(office): S, Path(f): Path<String>, text: String) -> R<Json<Value>> {
    let opts: super::StartOptions = if text.trim().is_empty() {
        Default::default()
    } else {
        serde_json::from_str(&text).map_err(|e| bad(format!("body: {e}")))?
    };
    let run = super::up_with(&office, &f, &opts).await?;
    Ok(Json(run.status()))
}

async fn down(Active(run): Active) -> R<Json<Value>> {
    super::down(&run, "firm down").await;
    Ok(Json(
        json!({ "run": run.id, "state": "ended", "dir": run.dir }),
    ))
}

async fn pause(Active(run): Active, text: String) -> R<Json<Value>> {
    let v = body(&text)?;
    super::pause(&run, v["paused"].as_bool().unwrap_or(true))?;
    Ok(Json(json!({ "state": *run.state.lock().unwrap() })))
}

async fn send(Active(run): Active, text: String) -> R<Json<Value>> {
    let v = body(&text)?;
    let sent = super::board_send(&run, &v)?;
    Ok(Json(json!(sent)))
}

/// Files from the board's own computer, stored as a shared drive artifact
/// by the board to attach to mail: `{title?, thread?, files: [{name, data}]}`
/// with `data` in base64.
async fn upload(Active(run): Active, text: String) -> R<Json<Value>> {
    use base64::Engine;
    let v: Value = serde_json::from_str(&text).map_err(|e| bad(format!("body: {e}")))?;
    let mut files = Vec::new();
    for x in v["files"].as_array().into_iter().flatten() {
        let name = x["name"]
            .as_str()
            .ok_or_else(|| bad("each file needs a name"))?;
        let data = base64::engine::general_purpose::STANDARD
            .decode(x["data"].as_str().unwrap_or(""))
            .map_err(|_| bad(format!("{name}: data is not base64")))?;
        files.push((name.to_string(), data));
    }
    let thread = v["thread"].as_str().map(str::to_string);
    let out = drive::publish_board(&run, files, v["title"].as_str().unwrap_or(""), thread)
        .map_err(|e| bad(e.to_string()))?;
    Ok(Json(out))
}

/// The board clears a new hire or objects: `{clear: bool, note?}`.
/// Every grant and credential request of the running firm.
async fn credentials_state(Active(run): Active) -> R<Json<Value>> {
    Ok(Json(super::access::all(&run)))
}

/// The board decides a credential request: `{grant, level?, expires_in?, note}`.
async fn credentials_decide(
    State(office): S,
    Path((f, id)): Path<(String, String)>,
    text: String,
) -> R<Json<Value>> {
    let v = body(&text)?;
    let run = office.active(&f)?;
    let grant = v["grant"]
        .as_bool()
        .ok_or_else(|| bad("grant: true or false"))?;
    let level = v["level"].as_str();
    Ok(Json(super::access::decide(
        &run,
        BOARD,
        &id,
        grant,
        level,
        v["expires_in"].as_str(),
        v["note"].as_str().unwrap_or(""),
    )?))
}

/// The board grants directly: `{worker, resource, level, expires_in?, note?}`.
async fn credentials_grant(Active(run): Active, text: String) -> R<Json<Value>> {
    let v = body(&text)?;
    let s = |k: &str| v[k].as_str().ok_or_else(|| bad(format!("{k} is required")));
    let level = s("level")?;
    Ok(Json(super::access::board_grant(
        &run,
        s("worker")?,
        s("resource")?,
        level,
        v["expires_in"].as_str(),
        v["note"].as_str().unwrap_or(""),
    )?))
}

/// The board revokes a grant: `?reason=`.
async fn credentials_revoke(
    State(office): S,
    Path((f, worker, resource)): Path<(String, String, String)>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> R<Json<Value>> {
    let run = office.active(&f)?;
    let reason = q
        .get("reason")
        .cloned()
        .unwrap_or_else(|| "revoked by the board".into());
    Ok(Json(super::access::revoke(
        &run, BOARD, &worker, &resource, &reason,
    )?))
}

async fn screening_decide(
    State(office): S,
    Path((f, id)): Path<(String, String)>,
    text: String,
) -> R<Json<Value>> {
    let v = body(&text)?;
    let run = office.active(&f)?;
    let clear = v["clear"]
        .as_bool()
        .ok_or_else(|| bad("clear: true or false"))?;
    Ok(Json(super::security::decide_screening(
        &run,
        BOARD,
        &id,
        clear,
        v["note"].as_str().unwrap_or(""),
        &v["grants"],
    )?))
}

/// The board approves or declines the CEO's budget request:
/// `{approve: bool, note?}`.
async fn budget_decide(
    State(office): S,
    Path((f, id)): Path<(String, String)>,
    text: String,
) -> R<Json<Value>> {
    let v = body(&text)?;
    let run = office.active(&f)?;
    let approve = v["approve"]
        .as_bool()
        .ok_or_else(|| bad("approve: true or false"))?;
    let note = v["note"].as_str().unwrap_or("");
    Ok(Json(super::workforce::decide(&run, &id, approve, note)?))
}

/// The board gives a worker tokens: `{worker, tokens, reason}`.
async fn board_fund(Active(run): Active, text: String) -> R<Json<Value>> {
    let v = body(&text)?;
    let worker = v["worker"].as_str().ok_or_else(|| bad("worker: a name"))?;
    let tokens = super::workforce::tokens_arg(&v["tokens"], "tokens")?;
    let reason = v["reason"].as_str().unwrap_or("");
    Ok(Json(super::workforce::fund(
        &run, BOARD, worker, tokens, reason,
    )?))
}

/// The board lets a worker go: `{worker, reason}`.
async fn board_release(Active(run): Active, text: String) -> R<Json<Value>> {
    let v = body(&text)?;
    let worker = v["worker"].as_str().ok_or_else(|| bad("worker: a name"))?;
    let reason = v["reason"].as_str().unwrap_or("");
    Ok(Json(
        super::workforce::release(&run, BOARD, worker, reason).await?,
    ))
}

async fn mark_read(State(office): S, Path(f): Path<String>, text: String) -> R<Json<Value>> {
    let v = body(&text)?;
    let run = office.run(&f)?;
    let ids: Vec<String> = v["ids"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|x| x.as_str().map(str::to_string))
        .collect();
    run.post.mark_read(BOARD, &ids);
    Ok(Json(json!({ "ok": true })))
}

async fn decide(
    State(office): S,
    Path((f, id)): Path<(String, String)>,
    text: String,
) -> R<Json<Value>> {
    let v = body(&text)?;
    let run = office.active(&f)?;
    let strings = |k: &str| {
        v[k].as_array().map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
    };
    let a = security::DecideArgs {
        allow: v["allow"]
            .as_bool()
            .ok_or_else(|| bad("allow is required"))?,
        scope: v["scope"].as_str().unwrap_or("once").to_string(),
        reason: v["reason"]
            .as_str()
            .unwrap_or("decided by the board")
            .to_string(),
        methods: strings("methods"),
        path: v["path"].as_str().map(str::to_string),
        expires_in: v["expires_in"].as_str().map(str::to_string),
    };
    Ok(Json(security::decide(&run, BOARD, &id, a)?))
}

async fn revoke(State(office): S, Path((f, id)): Path<(String, String)>) -> R<Json<Value>> {
    let run = office.active(&f)?;
    Ok(Json(security::revoke(
        &run,
        BOARD,
        &id,
        "revoked by the board",
    )?))
}

/// Copies a standing approval into the running config, at the profile of the
/// role it applies to; `zapper config save` keeps it.
async fn promote(State(office): S, Path((f, id)): Path<(String, String)>) -> R<Json<Value>> {
    let run = office.active(&f)?;
    let (scope, rule) = security::promote_target(&run, &id)?;
    let added = office.client().promote_rule(
        &format!("approval {id} of firm {f} promoted to {scope}"),
        &scope,
        rule,
    )?;
    run.journal.append("access", json!({ "event": "promoted", "rule": id, "to": scope.to_string(), "config_rule": added.id() }));
    Ok(Json(
        json!({ "promoted": id, "scope": scope.to_string(), "rule": added }),
    ))
}

/// The board's note on a run: `{note}`, empty to remove it.
async fn run_note(
    State(office): S,
    Path((f, run)): Path<(String, String)>,
    text: String,
) -> R<Json<Value>> {
    let v = body(&text)?;
    let note = v["note"].as_str().ok_or_else(|| bad("note: text"))?;
    super::set_note(&office, &f, &run, note)?;
    Ok(Json(json!({ "run": run, "note": note.trim() })))
}

async fn prune(State(office): S, Path((f, run)): Path<(String, String)>) -> R<Json<Value>> {
    super::prune(&office, &f, &run)?;
    Ok(Json(json!({ "removed": run })))
}
