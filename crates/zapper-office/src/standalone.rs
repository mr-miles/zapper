//! The office served as its own process: its [`Office`] over the
//! [`crate::openshell::OpenShell`] client of a gateway, the office edge its
//! workers reach, and the firm routes, the office page and the board's
//! actions on its own listener, behind the UI gateway with a token of its
//! own. `zapper-office serve` calls [`serve`].

use std::path::PathBuf;
use std::sync::Arc;

use crate::gateway;
use anyhow::{Context, Result};
use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::Router;
use zapper_util::random_id;

use crate::{Office, ASSETS, PAGE};

/// The header a write from the office's own page carries (`X-Zapper-UI: 1`),
/// which another site's page cannot send without a CORS preflight that is
/// never granted.
const UI_MARKER: &str = "x-zapper-ui";

/// The dashboard's URL as `serve` printed it, under the state directory.
pub const URL_FILE: &str = "office-url";

/// The office's logo, the page's favicon.
const FAVICON: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><g fill="none" stroke="#D97757" stroke-width="5" stroke-linejoin="round" stroke-linecap="round"><circle cx="32" cy="32" r="28.5"/><path d="M42.16 36.21L36.21 42.16L27.79 42.16L21.84 36.21L21.84 27.79L27.79 21.84L36.21 21.84L42.16 27.79Z"/><path d="M42.16 36.21L42.16 60.23M36.21 42.16L19.23 59.15M27.79 42.16L3.77 42.16M21.84 36.21L4.85 19.23M21.84 27.79L21.84 3.77M27.79 21.84L44.77 4.85M36.21 21.84L60.23 21.84M42.16 27.79L59.15 44.77"/></g></svg>"##;

/// Where the office listens for its own CLI (`zapper-office firm/mail/drive`):
/// `$ZAPPER_OFFICE_SOCKET`, else `office.sock` under `$XDG_RUNTIME_DIR`
/// (`/tmp/zapper-<uid>` without it). A unix socket, gated by filesystem permissions to the
/// local user; the browser
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

/// The office's own web app: its page and assets, the firm routes and
/// `extra` (the OpenShell sandboxes the VMs view lists) nested at `/api`
/// (where the page's scripts call them), all behind the UI gateway (`token`,
/// `Host`/`Origin` and the `X-Zapper-UI` marker). `remote`: the listener is
/// not loopback (served to a network), which loosens the `Host` check.
pub fn app(office: Arc<Office>, extra: Router, token: String, port: u16, remote: bool) -> Router {
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

/// The office page's gateway, for the office's own token: every request needs the token (in the query once, then a strict
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

/// Serves the office on `listener` as a client of the OpenShell gateway `os`
/// connects to, until a shutdown signal, then takes its running firms down.
/// Prints the dashboard's URL (with its one-time token) to stderr.
pub async fn serve(
    listener: tokio::net::TcpListener,
    os: Arc<crate::openshell::OpenShell>,
    state_dir: PathBuf,
    office_config: PathBuf,
    remote: bool,
) -> Result<()> {
    let office = Arc::new(Office::default());
    let _ = office.client.set(os.clone());
    let _ = office.state_dir.set(state_dir);
    // Runs a previous office left running end, as it can't resume them.
    crate::close_orphans(office.state_dir());

    // The office edge: where the sandboxes' supervisors reach the office's
    // tools and git, for the workers, with their tokens.
    let edge_addr = os.edge_settings().listen;
    let edge_listener = tokio::net::TcpListener::bind(edge_addr)
        .await
        .with_context(|| format!("binding the office edge on {edge_addr}"))?;
    let edge = Arc::new(crate::edge::for_office(office.clone(), os.clone()));
    let edge_task = tokio::spawn(edge.serve(edge_listener));
    eprintln!(
        "zapper-office: the office edge on http://{edge_addr} (workers: {})",
        os.edge_settings().url
    );

    // The sandboxes' rules and drafted rules, read and applied beside it.
    let policy_task = os.start();

    let extra = crate::openshell::routes(os);
    let served = serve_office(listener, office, extra, office_config, remote).await;
    edge_task.abort();
    policy_task.abort();
    served
}

/// The firm config, the CLI socket, the dashboard (the firm routes and
/// `extra`), and taking firms down at the end.
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

    // The office's own firm config (`office.yaml`), validated against
    // `zapper.yaml`'s profiles; a bad firm config stops the office starting.
    // `zapper-office config reload` re-reads it from the same path.
    let _ = office.config_path.set(office_config.clone());
    let n = office.reload_firms()?;
    eprintln!(
        "zapper-office: {n} firm(s) from {}",
        office_config.display()
    );

    // The office's own CLI socket (no gateway: a local unix socket, gated
    // by filesystem permissions). It serves the bare firm routes that the
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
    let url = format!("http://{local}/?token={token}");
    eprintln!("zapper-office: dashboard on {url}");
    // For `zapper-office firm replay`; private, as the token is in it.
    if let Err(e) = zapper_util::write_private(&office.state_dir().join(URL_FILE), url.as_bytes()) {
        tracing::warn!("writing the dashboard's URL for `firm replay`: {e:#}");
    }
    let app = app(office.clone(), extra, token, local.port(), remote);
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

    // The office's page and firm routes, served behind the UI gateway.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn standalone_app_serves_behind_the_gateway() {
        let t = test_core(FIRM);
        let office = t.core.office();
        let app = || {
            super::app(
                office.clone(),
                axum::Router::new(),
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

        // A firm read over the socket: the configured firm is listed.
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
    }

    // The office's own CLI surface: the bare firm routes (what the CLI socket
    // serves `zapper-office firm/mail/drive`) answer with no token, marker or
    // Origin — a read and a write alike. The socket is gated by filesystem
    // permissions, so the browser
    // gateway (token, `Host`/`Origin`, the UI marker) does not apply here.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cli_routes_need_no_gateway() {
        let t = test_core(FIRM);
        let office = t.core.office();
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
