//! The drive: files in a host directory, which the user reaches through
//! the workbench and sessions reach as `drive.zapper:443`.
//!
//! For sessions it is a socket service: the workbench serves a socket in
//! zapper's `state/services/`, zapper terminates the guest's TLS with the
//! session's CA, names the caller in the first line, and decides each
//! request against `net` rules with methods and paths before passing it on
//! (for example: read `/inbox/**`, write `/outbox/{session}/**`), so the
//! drive decides no access itself. What it does decide is that every path
//! stays in its directory (`files`), and how much a request may be.
//!
//! For the user (the owner's token): listing, uploading, downloading and
//! removing, a download always as an attachment, so a file a session wrote
//! can't run in the workbench's page.

pub mod files;

use std::path::PathBuf;

use axum::body::Body;
use axum::extract::{Extension, Path as UrlPath, Request};
use axum::http::{header, Method, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use zapper_proto::facility::ServiceCaller;

use crate::facility::{Facility, Panel};
use crate::gateway::{Ctx, Error};
use crate::tokens::Grant;
use files::{Files, Found};

/// The name sessions reach it by, and its socket's in `state/services/`.
pub const NAME: &str = "drive.zapper";
pub const SOCKET: &str = "workbench-drive.sock";
/// Largest file one write takes.
pub const MAX_PUT: u64 = 256 << 20;

/// The rules that let the workbench's sessions use it: read the inbox,
/// and write their own outbox.
pub fn usual_rules() -> [Value; 2] {
    [
        json!({ "id": "drive-inbox", "kind": "net", "action": "allow", "host": NAME, "ports": [443], "methods": ["GET", "HEAD"], "path": "/inbox/**" }),
        json!({ "id": "drive-outbox", "kind": "net", "action": "allow", "host": NAME, "ports": [443], "methods": ["GET", "HEAD", "PUT", "DELETE"], "path": "/outbox/{session}/**" }),
    ]
}

pub struct Drive {
    files: Files,
}

impl Drive {
    /// The drive at `root`, made (0700, with `inbox/` and `outbox/`) if it
    /// isn't there.
    pub fn new(root: PathBuf) -> anyhow::Result<Drive> {
        use std::os::unix::fs::DirBuilderExt as _;
        let mut b = std::fs::DirBuilder::new();
        b.recursive(true).mode(0o700).create(&root)?;
        for d in ["inbox", "outbox"] {
            b.create(root.join(d))?;
        }
        Ok(Drive {
            files: Files::new(&root),
        })
    }
}

fn status_of(e: &std::io::Error) -> StatusCode {
    match e.kind() {
        std::io::ErrorKind::NotFound => StatusCode::NOT_FOUND,
        std::io::ErrorKind::InvalidInput => StatusCode::BAD_REQUEST,
        std::io::ErrorKind::AlreadyExists => StatusCode::CONFLICT,
        _ if e.raw_os_error() == Some(libc::ELOOP) || e.raw_os_error() == Some(libc::ENOTDIR) => {
            StatusCode::NOT_FOUND
        }
        _ if e.raw_os_error() == Some(libc::ENOTEMPTY)
            || e.raw_os_error() == Some(libc::EISDIR) =>
        {
            StatusCode::CONFLICT
        }
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

fn answer(status: StatusCode, v: Value) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(v.to_string()))
        .unwrap()
}

fn failed(e: std::io::Error) -> Response {
    answer(status_of(&e), json!({ "error": e.to_string() }))
}

/// A file's bytes as a body, read as they go.
fn file_body(file: std::fs::File) -> Body {
    let file = tokio::fs::File::from_std(file);
    let chunks = futures_util::stream::unfold(file, |mut f| async move {
        let mut buf = vec![0u8; 64 * 1024];
        match f.read(&mut buf).await {
            Ok(0) => None,
            Ok(n) => {
                buf.truncate(n);
                Some((Ok::<_, std::io::Error>(bytes::Bytes::from(buf)), f))
            }
            Err(e) => Some((Err(e), f)),
        }
    });
    Body::from_stream(chunks)
}

/// How a file is handed over: to a session, typed by its name (text as
/// text); to the user's browser, always as bytes to save.
#[derive(Clone, Copy)]
enum For {
    Session,
    Browser,
}

/// Answers one request on the drive at `path`.
async fn serve<B>(files: Files, method: &Method, path: &str, body: B, to: For) -> Response
where
    B: hyper::body::Body<Data = bytes::Bytes> + Send + Unpin + 'static,
    B::Error: std::fmt::Display,
{
    let parts = match files::components(path) {
        Ok(p) => p,
        Err(e) => return failed(e),
    };
    match *method {
        Method::GET | Method::HEAD => {
            let found = {
                let (f, p) = (files.clone(), parts.clone());
                tokio::task::spawn_blocking(move || f.find(&p)).await
            };
            match found {
                Ok(Ok(Found::Dir(entries))) => answer(
                    StatusCode::OK,
                    json!({ "path": format!("/{}", parts.join("/")), "entries": entries }),
                ),
                Ok(Ok(Found::File { file, size })) => {
                    let name = parts.last().map(String::as_str).unwrap_or("");
                    let mut r = Response::builder()
                        .header(header::CONTENT_LENGTH, size)
                        .header("x-content-type-options", "nosniff");
                    r = match to {
                        For::Session => r.header(header::CONTENT_TYPE, files::content_type(name)),
                        For::Browser => r
                            .header(header::CONTENT_TYPE, "application/octet-stream")
                            .header(header::CONTENT_DISPOSITION, "attachment")
                            .header("content-security-policy", "sandbox"),
                    };
                    let body = if *method == Method::HEAD {
                        Body::empty()
                    } else {
                        file_body(file)
                    };
                    r.body(body).unwrap()
                }
                Ok(Err(e)) => failed(e),
                Err(e) => answer(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    json!({ "error": e.to_string() }),
                ),
            }
        }
        Method::PUT => put(files, parts, body).await,
        Method::DELETE => {
            let r = tokio::task::spawn_blocking(move || files.remove(&parts)).await;
            match r {
                Ok(Ok(())) => answer(StatusCode::OK, json!({ "removed": path })),
                Ok(Err(e)) => failed(e),
                Err(e) => answer(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    json!({ "error": e.to_string() }),
                ),
            }
        }
        _ => answer(
            StatusCode::METHOD_NOT_ALLOWED,
            json!({ "error": "GET, HEAD, PUT or DELETE" }),
        ),
    }
}

/// Writes the body to `parts`, whole or not at all, at most `MAX_PUT`.
async fn put<B>(files: Files, parts: Vec<String>, mut body: B) -> Response
where
    B: hyper::body::Body<Data = bytes::Bytes> + Send + Unpin + 'static,
    B::Error: std::fmt::Display,
{
    let started = {
        let p = parts.clone();
        tokio::task::spawn_blocking(move || files.begin_write(&p)).await
    };
    let mut write = match started {
        Ok(Ok(w)) => w,
        Ok(Err(e)) => return failed(e),
        Err(e) => {
            return answer(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({ "error": e.to_string() }),
            )
        }
    };
    let mut out = tokio::fs::File::from_std(write.file().try_clone().unwrap());
    let mut size = 0u64;
    while let Some(frame) = body.frame().await {
        let frame = match frame {
            Ok(f) => f,
            Err(e) => return answer(StatusCode::BAD_REQUEST, json!({ "error": e.to_string() })),
        };
        let Ok(data) = frame.into_data() else {
            continue;
        };
        size += data.len() as u64;
        if size > MAX_PUT {
            return answer(
                StatusCode::PAYLOAD_TOO_LARGE,
                json!({ "error": format!("a file is at most {} MiB", MAX_PUT >> 20) }),
            );
        }
        if let Err(e) = out.write_all(&data).await {
            return failed(e);
        }
    }
    if let Err(e) = out.flush().await {
        return failed(e);
    }
    drop(out);
    match tokio::task::spawn_blocking(move || write.finish()).await {
        Ok(Ok(())) => answer(
            StatusCode::CREATED,
            json!({ "path": format!("/{}", parts.join("/")), "size": size }),
        ),
        Ok(Err(e)) => failed(e),
        Err(e) => answer(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "error": e.to_string() }),
        ),
    }
}

/// Reads the caller line zapper sends first, a byte at a time so the HTTP
/// that follows stays in the stream.
async fn caller(stream: &mut tokio::net::UnixStream) -> Option<ServiceCaller> {
    let mut line = Vec::new();
    let mut b = [0u8; 1];
    while line.len() < 4096 {
        match stream.read(&mut b).await {
            Ok(1) if b[0] == b'\n' => return serde_json::from_slice(&line).ok(),
            Ok(1) => line.push(b[0]),
            _ => return None,
        }
    }
    None
}

/// Serves the sessions' side on `listener`: each connection's caller named
/// by zapper, then HTTP/1.1 from zapper's proxy, already decided.
pub async fn serve_sessions(files: Files, listener: tokio::net::UnixListener) {
    while let Ok((mut stream, _)) = listener.accept().await {
        let files = files.clone();
        tokio::spawn(async move {
            let Some(who) = caller(&mut stream).await else {
                return;
            };
            let svc = hyper::service::service_fn(
                move |req: hyper::Request<hyper::body::Incoming>| {
                    let (files, who) = (files.clone(), who.clone());
                    async move {
                        let (method, path) = (req.method().clone(), req.uri().path().to_string());
                        let r = serve(files, &method, &path, req.into_body(), For::Session).await;
                        tracing::info!(session = %who.session, %method, %path, status = r.status().as_u16(), "drive");
                        Ok::<_, std::convert::Infallible>(r)
                    }
                },
            );
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(hyper_util::rt::TokioIo::new(stream), svc)
                .await;
        });
    }
}

fn owner(g: &Grant) -> Result<(), Error> {
    if *g == Grant::owner() {
        Ok(())
    } else {
        Err(Error(
            StatusCode::FORBIDDEN,
            "only the owner's token reaches the drive".into(),
        ))
    }
}

/// The user's side: `/w/v1/drive/files/PATH`.
async fn user_files(
    Extension(g): Extension<Grant>,
    Extension(files): Extension<Files>,
    path: Option<UrlPath<String>>,
    req: Request,
) -> Result<Response, Error> {
    owner(&g)?;
    let path = path.map(|p| p.0).unwrap_or_default();
    let method = req.method().clone();
    Ok(serve(files, &method, &path, req.into_body(), For::Browser).await)
}

/// Adds the usual rules, in zapper's global layer, if they aren't there:
/// the drive's name resolves only in the workbench's sessions, so they
/// reach no other.
async fn add_rules(
    axum::extract::State(ctx): axum::extract::State<Ctx>,
    Extension(g): Extension<Grant>,
) -> Result<Json<Value>, Error> {
    owner(&g)?;
    let have = ctx
        .zapper
        .call(Method::GET, "/v1/rules?scope=global", None)
        .await?
        .ok()?;
    let mut added = vec![];
    for rule in usual_rules() {
        let present = have
            .as_array()
            .into_iter()
            .flatten()
            .any(|r| r["id"] == rule["id"]);
        if !present {
            let body = json!({ "scope": "global", "rule": rule });
            ctx.zapper
                .call(Method::POST, "/v1/rules", Some(&body))
                .await?
                .ok()?;
            added.push(rule["id"].clone());
        }
    }
    Ok(Json(json!({ "added": added, "saved": false })))
}

const PANEL: &str = include_str!("drive.js");

impl Facility for Drive {
    fn name(&self) -> &'static str {
        "drive"
    }

    fn routes(&self) -> Router<Ctx> {
        Router::new()
            .route("/files", axum::routing::any(user_files))
            .route("/files/{*path}", axum::routing::any(user_files))
            .route("/rules", post(add_rules))
            .route(
                "/rules/usual",
                get(|| async { Json(Value::Array(usual_rules().to_vec())) }),
            )
            .layer(Extension(self.files.clone()))
    }

    fn panel(&self) -> Option<Panel> {
        Some(Panel {
            title: "Drive",
            script: PANEL,
        })
    }

    fn services(&self) -> Vec<Value> {
        vec![json!({
            "name": NAME,
            "port": 443,
            "sessions": { crate::gateway::LABEL.0: crate::gateway::LABEL.1 },
            "to": { "socket": SOCKET, "tls": true },
        })]
    }

    fn start(&self, ctx: &Ctx) {
        let (files, ctx) = (self.files.clone(), ctx.clone());
        tokio::spawn(async move {
            // zapper says where its sockets go; until it can, try again.
            let dir = loop {
                if let Ok(a) = ctx.zapper.call(Method::GET, "/v1/status", None).await {
                    if let Some(d) = a.body["socket_dir"].as_str() {
                        break PathBuf::from(d);
                    }
                }
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            };
            let path = dir.join(SOCKET);
            let _ = std::fs::remove_file(&path);
            match tokio::net::UnixListener::bind(&path) {
                Ok(l) => serve_sessions(files, l).await,
                Err(e) => tracing::warn!("the drive's socket {}: {e}", path.display()),
            }
        });
    }
}
