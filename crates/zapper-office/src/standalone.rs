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
//! real host stays the final pre-merge gate (P4-e). Two things this build
//! does not yet do, tracked in `docs/office-process-step4-plan.md`:
//!   - **Rule-following:** the co-host office follows the core's `rule`
//!     events (`lib::follow_rules`) to keep its record of officers' standing
//!     approvals in step with the operator's edits; standalone it would
//!     follow the daemon's `/v1/events` SSE stream. A run starts correctly
//!     without it; the two can drift only on an operator edit mid-run.
//!   - **Daemon-backed reads:** the office page's VM/session views read the
//!     daemon's session routes, which this listener does not serve; they are
//!     proxied to the daemon in a follow-up.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::Router;
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

/// The office's own web app: its page and assets, the firm routes nested at
/// `/api` (where the page's scripts call them), all behind the UI gateway
/// (`token`, `Host`/`Origin` and the `X-Zapper-UI` marker). `remote`: the
/// listener is not loopback (served to a network), which loosens the `Host`
/// check as `daemon.ui` does.
pub fn app(office: Arc<Office>, token: String, port: u16, remote: bool) -> Router {
    let mut page = Router::new()
        .route("/", get(serve_page))
        .route("/favicon.ico", get(serve_favicon));
    for &(name, body) in ASSETS {
        page = page.route(
            &format!("/mount/{name}"),
            get(move || serve_asset(name, body)),
        );
    }
    page.nest("/api", crate::routes(office))
        .layer(middleware::from_fn_with_state(
            (token, port, remote),
            ui_guard,
        ))
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
    remote: bool,
) -> Result<()> {
    let local = listener
        .local_addr()
        .context("the office listener's address")?;
    let office = bootstrap(zapper, state_dir, socket_dir);
    let token = random_id(32);
    eprintln!("zapper-office: dashboard on http://{local}/?token={token}");
    let app = app(office.clone(), token, local.port(), remote);
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{header, Request, StatusCode};
    use tower::ServiceExt;

    use crate::test_core;

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
        let app = || super::app(office.clone(), "test-token".into(), 7999, false);

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
}
