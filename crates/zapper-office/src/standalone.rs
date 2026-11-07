//! The office served as its own process (proposal 13, step 4; P4-d): it
//! builds its [`Office`] over a [`client::Remote`] client of the zapper
//! daemon and serves the firm routes, the office page and the board's actions
//! on its own listener, behind the UI gateway with a token of its own — the
//! way `zapper-workbench` serves its UI. `zapper-office serve` calls
//! [`serve`]; `zapper serve` spawns it under `OFFICE=1` (P4-d-3).
//!
//! The office never crosses the VM boundary — office↔core is the daemon's
//! API alone — so this serve path is unit-tested against a real `zapper-api`
//! router backed by a `TestCore` over a unix socket (see `tests` below and
//! `client::remote_over_the_socket`), not KVM. `make test-e2e-office` on a
//! real host stays the final pre-merge gate (P4-e).
//!
//! Two things the co-host office did in process this build now does over the
//! daemon's API:
//!   - **Rule-following** ([`follow_rules`]): follows the daemon's
//!     `/v1/events` SSE stream and drops a firm's standing approval when its
//!     role-layer rule is removed through the daemon (the operator, or
//!     `zapper` without the office), through the same
//!     [`crate::forget_removed_role_rule`] the co-host `follow_rules` uses.
//!   - **Daemon-backed reads** ([`proxy_routes`]): the office page's VMs view
//!     and the officers' traffic view read the daemon's session routes, which
//!     the office does not serve itself; it proxies the handful of reads they
//!     need to the daemon over the socket.
//!
//! Terminal/VM deep links in the page still point at the daemon UI's own
//! pages (`/terminal`, `/sessions`), which this listener does not serve — the
//! VMs tab lists and the security tab reads populate, but those click-throughs
//! need the daemon UI's URL; a small follow-up tracked in the plan.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::{RawQuery, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::{Json, Router};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use zapper_client::gateway;
use zapper_util::random_id;

use crate::client::{self, Zapper};
use crate::{Office, ASSETS, PAGE};

/// The header a write from the office's own page carries (`X-Zapper-UI: 1`),
/// which another site's page cannot send without a CORS preflight that is
/// never granted.
const UI_MARKER: &str = "x-zapper-ui";

/// The firm's logo, as in the daemon UI's header (kept in step with
/// `zapper-api`'s `ui::favicon`).
const FAVICON: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><g fill="none" stroke="#D97757" stroke-width="5" stroke-linejoin="round" stroke-linecap="round"><circle cx="32" cy="32" r="28.5"/><path d="M42.16 36.21L36.21 42.16L27.79 42.16L21.84 36.21L21.84 27.79L27.79 21.84L36.21 21.84L42.16 27.79Z"/><path d="M42.16 36.21L42.16 60.23M36.21 42.16L19.23 59.15M27.79 42.16L3.77 42.16M21.84 36.21L4.85 19.23M21.84 27.79L21.84 3.77M27.79 21.84L44.77 4.85M36.21 21.84L60.23 21.84M42.16 27.79L59.15 44.77"/></g></svg>"##;

/// Where the office listens for its own CLI (`zapper-office firm/mail/drive`):
/// `$ZAPPER_OFFICE_SOCKET`, else `office.sock` beside the daemon's `api.sock`
/// under `$XDG_RUNTIME_DIR`. A unix socket, gated by filesystem permissions to
/// the local user, the way the daemon serves the `zapper` CLI — the browser
/// gateway (token, `Host`/`Origin`, the UI marker) is for the page alone.
pub fn office_socket_path() -> PathBuf {
    if let Ok(p) = std::env::var("ZAPPER_OFFICE_SOCKET") {
        return p.into();
    }
    let base = std::env::var("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(format!("/tmp/zapper-{}", unsafe { libc::getuid() })));
    base.join("zapper").join("office.sock")
}

async fn serve_page() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], PAGE)
}

async fn serve_favicon() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "image/svg+xml")], FAVICON)
}

async fn serve_asset(name: &'static str, body: &'static str) -> impl IntoResponse {
    let kind = if name.ends_with(".css") {
        "text/css; charset=utf-8"
    } else {
        "text/javascript; charset=utf-8"
    };
    ([(header::CONTENT_TYPE, kind)], body)
}

/// The value of the query parameter `name`, if the raw `query` carries it.
fn query_value(query: &str, name: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == name).then(|| v.replace('+', " "))
    })
}

/// Builds the office state over a [`client::Remote`] client of the daemon.
/// `state_dir`/`socket_dir` are the daemon's, shared by convention (the
/// office and daemon are configured to the same paths; P1-a).
fn bootstrap(zapper: Zapper, state_dir: PathBuf, socket_dir: PathBuf) -> Arc<Office> {
    let office = Arc::new(Office::default());
    let _ = office.client.set(Arc::new(client::Remote(zapper)));
    let _ = office.state_dir.set(state_dir);
    let _ = office.socket_dir.set(socket_dir);
    office
}

/// Forwards a GET to the daemon over the socket and returns its answer as the
/// office's own response (status and JSON preserved). The office page's VM and
/// security views read the daemon's session routes, which the office does not
/// serve itself; it proxies the handful it needs (reads only) to the daemon.
async fn forward(zapper: &Zapper, path: &str) -> Response {
    match zapper.get(path).await {
        Ok(answer) => (answer.status, Json(answer.body)).into_response(),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn proxy_sessions(State(zapper): State<Zapper>, RawQuery(query): RawQuery) -> Response {
    let path = match query.filter(|q| !q.is_empty()) {
        Some(q) => format!("/v1/sessions?{q}"),
        None => "/v1/sessions".to_string(),
    };
    forward(&zapper, &path).await
}

/// The one daemon-backed read the office page needs but the office does not
/// serve itself: the VMs view's session list (with `?terminals=true`, the
/// terminals embedded — one call, not a `/terminals` fetch per session). The
/// officers' traffic view is served firm-scoped by the office itself
/// (`/firms/{f}/requests`), not proxied. Reads only, over the daemon's socket.
fn proxy_routes(zapper: Zapper) -> Router {
    Router::new()
        .route("/sessions", get(proxy_sessions))
        .with_state(zapper)
}

/// The office's own web app: its page and assets, the firm routes and the
/// proxied daemon reads nested at `/api` (where the page's scripts call them),
/// all behind the UI gateway (`token`, `Host`/`Origin` and the `X-Zapper-UI`
/// marker). `remote`: the listener is not loopback (served to a network),
/// which loosens the `Host` check as `daemon.ui` does.
pub fn app(office: Arc<Office>, zapper: Zapper, token: String, port: u16, remote: bool) -> Router {
    app_with(office, proxy_routes(zapper), token, port, remote)
}

/// As [`app`], with `extra` (the reads the office's backend serves: the
/// daemon's proxied, or OpenShell's sessions) beside the firm routes.
fn app_with(office: Arc<Office>, extra: Router, token: String, port: u16, remote: bool) -> Router {
    let mut page = Router::new()
        .route("/", get(serve_page))
        .route("/favicon.ico", get(serve_favicon));
    for &(name, body) in ASSETS {
        page = page.route(
            &format!("/mount/{name}"),
            get(move || serve_asset(name, body)),
        );
    }
    page.nest("/api", crate::routes(office).merge(extra))
        .layer(middleware::from_fn_with_state(
            (token, port, remote),
            ui_guard,
        ))
}

/// Follows the daemon's `/v1/events` SSE stream and keeps the office's record
/// of officers' standing approvals in step with the operator's edits: a rule
/// removed from a role's layer through the daemon (the operator, or `zapper`
/// without the office) is dropped from the firm's run
/// ([`crate::forget_removed_role_rule`]). This is the standalone counterpart
/// of the co-host `follow_rules`, which reads the same events in process.
/// Reconnects if the stream drops, resuming after the last event it saw so an
/// edit is not missed.
async fn follow_rules(zapper: Zapper, office: Arc<Office>) {
    let mut after: Option<String> = None;
    loop {
        if let Err(e) = follow_once(&zapper, &office, &mut after).await {
            tracing::debug!("office rule-follower: {e}; reconnecting");
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}

/// One connection to `/v1/events`: applies rule events until the stream ends,
/// advancing `after` to the last event seen so a reconnect resumes after it.
/// `Err` only means the connection dropped (the caller reconnects).
async fn follow_once(
    zapper: &Zapper,
    office: &Arc<Office>,
    after: &mut Option<String>,
) -> Result<()> {
    let path = match after.as_deref() {
        Some(c) => format!("/v1/events?after={c}"),
        None => "/v1/events".to_string(),
    };
    let resp = zapper.stream(&path, &HeaderMap::new()).await?;
    if !resp.status().is_success() {
        anyhow::bail!("the daemon answered {} for {path}", resp.status());
    }
    let mut body = resp.into_body();
    let mut buf = String::new();
    while let Some(frame) = body.frame().await {
        let frame = frame?;
        let Some(data) = frame.data_ref() else {
            continue;
        };
        buf.push_str(&String::from_utf8_lossy(data));
        while let Some(i) = buf.find('\n') {
            let line: String = buf.drain(..=i).collect();
            let Some(data) = line.trim().strip_prefix("data:") else {
                continue;
            };
            let Ok(v) = serde_json::from_str::<Value>(data.trim()) else {
                continue;
            };
            if let (Some(epoch), Some(seq)) = (v["epoch"].as_str(), v["seq"].as_u64()) {
                *after = Some(format!("{epoch}:{seq}"));
            }
            if v["type"] == "rule" {
                crate::forget_removed_role_rule(
                    office,
                    v["state"].as_str().unwrap_or(""),
                    v["id"].as_str().unwrap_or(""),
                    v["scope"].as_str().unwrap_or(""),
                );
            }
        }
    }
    Ok(())
}

/// The office page's gateway, the daemon UI's `ui_guard` for the office's own
/// token: every request needs the token (in the query once, then a strict
/// cookie) and an acceptable `Host`; a write also needs the `X-Zapper-UI`
/// marker and an `Origin` naming the office itself.
async fn ui_guard(
    State((token, port, remote)): State<(String, u16, bool)>,
    req: Request,
    next: Next,
) -> Response {
    // A token in the query signs the browser in: set the cookie and redirect
    // to the same path without it.
    let query_token = req.uri().query().and_then(|q| query_value(q, "token"));
    if query_token.as_deref() == Some(token.as_str()) {
        let mut resp = Redirect::to(req.uri().path()).into_response();
        resp.headers_mut().insert(
            header::SET_COOKIE,
            gateway::set_cookie("zapper_office_token", &token)
                .parse()
                .unwrap(),
        );
        return resp;
    }
    let offered = gateway::cookie_token(req.headers(), "zapper_office_token")
        .into_iter()
        .chain(query_token);
    if !offered.into_iter().any(|t| t == token) {
        return (
            StatusCode::UNAUTHORIZED,
            "No token found matching this office.  See the URL printed by zapper-office serve",
        )
            .into_response();
    }
    if let Err((status, why)) =
        gateway::from_the_ui(req.method(), req.headers(), port, remote, UI_MARKER)
    {
        return (status, why).into_response();
    }
    next.run(req).await
}

/// Serves the office on `listener` as a client of the daemon at `zapper`,
/// until a shutdown signal, then takes its running firms down. Prints the
/// dashboard's URL (with the one-time token) to stderr.
pub async fn serve(
    listener: tokio::net::TcpListener,
    zapper: Zapper,
    state_dir: PathBuf,
    socket_dir: PathBuf,
    office_config: PathBuf,
    remote: bool,
) -> Result<()> {
    let office = bootstrap(zapper.clone(), state_dir, socket_dir);
    // Keep the office's standing-approval record in step with the operator's
    // edits by following the daemon's `/v1/events` SSE (the standalone
    // counterpart of the co-host in-process `follow_rules`).
    let follower = {
        let (zapper, office) = (zapper.clone(), office.clone());
        tokio::spawn(async move { follow_rules(zapper, office).await })
    };
    let served = serve_office(
        listener,
        office,
        proxy_routes(zapper),
        office_config,
        remote,
    )
    .await;
    follower.abort();
    served
}

/// Serves the office on `listener` as a client of the OpenShell gateway `os`
/// connects to (feature `openshell`), until a shutdown signal, then takes its
/// running firms down. `state_dir` and `socket_dir` are the office's own: no
/// daemon shares them.
#[cfg(feature = "openshell")]
pub async fn serve_openshell(
    listener: tokio::net::TcpListener,
    os: Arc<crate::openshell::OpenShell>,
    state_dir: PathBuf,
    socket_dir: PathBuf,
    office_config: PathBuf,
    remote: bool,
) -> Result<()> {
    std::fs::create_dir_all(&socket_dir)
        .with_context(|| format!("creating {}", socket_dir.display()))?;
    let office = Arc::new(Office::default());
    let _ = office.client.set(os.clone());
    let _ = office.state_dir.set(state_dir);
    let _ = office.socket_dir.set(socket_dir);

    // The office edge: where the sandboxes' supervisors reach the office's
    // tools and git, for the workers, with their tokens.
    let edge_addr = os.edge_settings().listen;
    let edge_listener = tokio::net::TcpListener::bind(edge_addr)
        .await
        .with_context(|| format!("binding the office edge on {edge_addr}"))?;
    let edge = Arc::new(crate::edge::Edge::for_office(office.clone(), os.clone()));
    let edge_task = tokio::spawn(edge.serve(edge_listener));
    eprintln!(
        "zapper-office: the office edge on http://{edge_addr} (workers: {})",
        os.edge_settings().url
    );

    let extra = crate::openshell::routes(os);
    let served = serve_office(listener, office, extra, office_config, remote).await;
    edge_task.abort();
    served
}

/// The serving both backends share: the firm config, the CLI socket, the
/// dashboard (the firm routes and `extra`), and taking firms down at the end.
async fn serve_office(
    listener: tokio::net::TcpListener,
    office: Arc<Office>,
    extra: Router,
    office_config: PathBuf,
    remote: bool,
) -> Result<()> {
    let local = listener
        .local_addr()
        .context("the office listener's address")?;

    // The office's own firm config (`office.yaml`), validated against the
    // daemon's profiles; a bad firm config stops the office starting.
    // `zapper-office config reload` re-reads it from the same path.
    let _ = office.config_path.set(office_config.clone());
    let n = office.reload_firms()?;
    eprintln!(
        "zapper-office: {n} firm(s) from {}",
        office_config.display()
    );

    // The office's own CLI socket (no gateway: a local unix socket, as the
    // daemon serves the `zapper` CLI). It serves the bare firm routes that the
    // page reaches at `/api`; `zapper-office firm/mail/drive` call them here.
    let sock = office_socket_path();
    if let Some(parent) = sock.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let _ = std::fs::remove_file(&sock);
    let cli_listener = tokio::net::UnixListener::bind(&sock)
        .with_context(|| format!("binding the office CLI socket at {}", sock.display()))?;
    eprintln!("zapper-office: CLI socket at {}", sock.display());
    let cli = {
        let app = crate::routes(office.clone());
        tokio::spawn(async move {
            let _ = axum::serve(cli_listener, app).await;
        })
    };

    let token = random_id(32);
    eprintln!("zapper-office: dashboard on http://{local}/?token={token}");
    let app = app_with(office.clone(), extra, token, local.port(), remote);
    let shutdown = {
        let office = office.clone();
        async move {
            let _ = tokio::signal::ctrl_c().await;
            let runs: Vec<_> = office.runs.read().unwrap().values().cloned().collect();
            for r in runs {
                crate::down(&r, "office shutdown").await;
            }
        }
    };
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await
        .context("serving the office")?;
    cli.abort();
    let _ = std::fs::remove_file(&sock);
    Ok(())
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{header, Request, StatusCode};
    use tower::ServiceExt;

    use crate::test_core;
    use crate::OfficeOf;

    const FIRM: &str = "
profiles:
  w: { agent: { kind: claude-code } }
firms:
  acme:
    roles:
      ceo: { profile: w, agent: ceo }
      eng: { profile: w, agent: software-engineer }
    workers:
      - { name: ceo, role: ceo }
      - { name: pat, role: eng, reports_to: ceo }
";

    const HOST: &str = "127.0.0.1:7999";
    const COOKIE: &str = "zapper_office_token=test-token";

    async fn body_string(resp: axum::response::Response) -> String {
        let bytes = http_body_util::BodyExt::collect(resp.into_body())
            .await
            .unwrap()
            .to_bytes();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    // The office served as its own process: a `Remote` client of a real
    // `zapper-api` router (TestCore-backed) over a unix socket, its page and
    // firm routes behind the UI gateway. The office↔core boundary is API
    // only, so this is unit-testable without KVM.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn standalone_app_serves_behind_the_gateway() {
        let t = test_core(FIRM);

        // Serve the real API on a temp socket, exactly as the daemon does.
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let sock =
            std::env::temp_dir().join(format!("zo-standalone-{}-{nonce}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        let api = zapper_api::api(t.core.clone(), &zapper_api::Mount::default());
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let serving = tokio::spawn(async move {
            let _ = axum::serve(listener, api).await;
        });

        let office = super::bootstrap(
            super::Zapper::new(sock.clone()),
            t.core.state_dir().to_path_buf(),
            std::env::temp_dir(),
        );
        // `serve` loads firms from `office.yaml`; here the bootstrap'd office
        // takes the same firms `test_core` loaded onto the daemon's office.
        office.load_firms(t.core.office().firms());
        let app = || {
            super::app(
                office.clone(),
                super::Zapper::new(sock.clone()),
                "test-token".into(),
                7999,
                false,
            )
        };

        // No token: refused before anything else.
        let r = app()
            .oneshot(Request::get("/api/firms").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);

        // The token in the query signs in: a redirect that sets the cookie.
        let r = app()
            .oneshot(
                Request::get("/?token=test-token")
                    .header(header::HOST, HOST)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(r.status().is_redirection());
        assert!(r
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .contains("zapper_office_token=test-token"));

        // A firm read over the socket: the configured firm is listed (the
        // config comes from the daemon through the `Remote` client).
        let r = app()
            .oneshot(
                Request::get("/api/firms")
                    .header(header::HOST, HOST)
                    .header(header::COOKIE, COOKIE)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        assert!(body_string(r).await.contains("acme"));

        // A daemon-backed read (the VMs view's session list) is proxied to the
        // daemon over the socket — the office does not serve `/sessions`
        // itself, so this reaching 200 proves the proxy.
        let r = app()
            .oneshot(
                Request::get("/api/sessions")
                    .header(header::HOST, HOST)
                    .header(header::COOKIE, COOKIE)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);

        // The page's assets are served (behind the gateway).
        let r = app()
            .oneshot(
                Request::get("/mount/office.css")
                    .header(header::HOST, HOST)
                    .header(header::COOKIE, COOKIE)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);

        // A write without the `X-Zapper-UI` marker is refused by the gateway.
        let r = app()
            .oneshot(
                Request::post("/api/firms/acme/pause")
                    .header(header::HOST, HOST)
                    .header(header::COOKIE, COOKIE)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN);

        // With the marker and the UI's Origin it passes the gateway and
        // reaches the handler (which 4xxs: no such firm is running).
        let r = app()
            .oneshot(
                Request::post("/api/firms/nope/pause")
                    .header(header::HOST, HOST)
                    .header(header::COOKIE, COOKIE)
                    .header("x-zapper-ui", "1")
                    .header(header::ORIGIN, "http://127.0.0.1:7999")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_ne!(r.status(), StatusCode::FORBIDDEN);
        assert_ne!(r.status(), StatusCode::UNAUTHORIZED);

        serving.abort();
        let _ = std::fs::remove_file(&sock);
    }

    // The office's own CLI surface: the bare firm routes (what the CLI socket
    // serves `zapper-office firm/mail/drive`) answer with no token, marker or
    // Origin — a read and a write alike. The socket is gated by filesystem
    // permissions, as the daemon serves the `zapper` CLI, so the browser
    // gateway (token, `Host`/`Origin`, the UI marker) does not apply here.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cli_routes_need_no_gateway() {
        let t = test_core(FIRM);
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let sock = std::env::temp_dir().join(format!("zo-cli-{}-{nonce}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        let api = zapper_api::api(t.core.clone(), &zapper_api::Mount::default());
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let serving = tokio::spawn(async move {
            let _ = axum::serve(listener, api).await;
        });

        let office = super::bootstrap(
            super::Zapper::new(sock.clone()),
            t.core.state_dir().to_path_buf(),
            std::env::temp_dir(),
        );
        office.load_firms(t.core.office().firms());
        let routes = || crate::routes(office.clone());

        // A read with no headers at all: the firm is listed.
        let r = routes()
            .oneshot(Request::get("/firms").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        assert!(body_string(r).await.contains("acme"));

        // A write with no token/marker/Origin reaches the handler (it 4xxs:
        // no such firm is running), never a gateway's 401/403 — there is none.
        let r = routes()
            .oneshot(
                Request::post("/firms/nope/pause")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_ne!(r.status(), StatusCode::FORBIDDEN);
        assert_ne!(r.status(), StatusCode::UNAUTHORIZED);

        serving.abort();
        let _ = std::fs::remove_file(&sock);
    }

    #[test]
    fn office_socket_path_honours_the_env_override() {
        // (Process-global env: this is the only test that reads it.)
        std::env::set_var("ZAPPER_OFFICE_SOCKET", "/tmp/zo-test.sock");
        assert_eq!(
            super::office_socket_path(),
            std::path::PathBuf::from("/tmp/zo-test.sock")
        );
        std::env::remove_var("ZAPPER_OFFICE_SOCKET");
    }
}
