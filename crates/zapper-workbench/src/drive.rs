//! The drive: files in a host directory that the workbench's sandboxes
//! reach at `/drive/` on its edge, and the user reaches on the host as the
//! directory itself.
//!
//! What a sandbox may do is the drive's to decide, as nothing in between
//! inspects the paths: read the inbox (`GET`/`HEAD` under `/inbox/`), and
//! read and write its own outbox (`GET`, `HEAD`, `PUT`, `DELETE` under
//! `/outbox/<sandbox>/`), the sandbox being the one the edge's token names.
//! Every path stays in the drive's directory (`files`), and a file one
//! sandbox wrote is handed to another as typed by its name, never run.

pub mod files;

use std::path::PathBuf;

use bytes::Bytes;
use http_body_util::{BodyExt, StreamBody};
use hyper::body::Frame as BodyFrame;
use hyper::header;
use hyper::{Method, Response, StatusCode};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use zapper_openshell::edge::full;

use files::{Files, Found};

/// Largest file one write takes.
pub const MAX_PUT: u64 = 256 << 20;

pub use zapper_openshell::edge::Body;

#[derive(Clone)]
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

    /// Answers sandbox `who`'s `method` on `path` (under `/drive`), with
    /// `body` for a write.
    pub async fn serve<B>(&self, who: &str, method: &Method, path: &str, body: B) -> Response<Body>
    where
        B: hyper::body::Body<Data = Bytes> + Send + Unpin + 'static,
        B::Error: std::fmt::Display,
    {
        match may(who, method, path) {
            Ok(()) => serve(self.files.clone(), method, path, body).await,
            Err(why) => answer(StatusCode::FORBIDDEN, json!({ "error": why })),
        }
    }
}

/// Whether sandbox `who` may `method` on `path`: read the inbox, and read
/// and write its own outbox.
fn may(who: &str, method: &Method, path: &str) -> Result<(), String> {
    let parts = files::components(path).map_err(|e| e.to_string())?;
    let read = matches!(*method, Method::GET | Method::HEAD);
    let write = matches!(*method, Method::PUT | Method::DELETE);
    match parts.first().map(String::as_str) {
        Some("inbox") if read => Ok(()),
        Some("inbox") => Err("the inbox is read-only to sandboxes".into()),
        Some("outbox") if (read || write) && parts.get(1).map(String::as_str) == Some(who) => {
            Ok(())
        }
        Some("outbox") if parts.len() == 1 && read => Err(format!("your outbox is /outbox/{who}/")),
        Some("outbox") => Err(format!("only your own outbox: /outbox/{who}/")),
        _ => Err(format!("the drive is /inbox/ and /outbox/{who}/")),
    }
}

fn answer(status: StatusCode, v: Value) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(full(v.to_string()))
        .unwrap()
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
                Some((Ok(BodyFrame::data(Bytes::from(buf))), f))
            }
            Err(e) => Some((Err(e), f)),
        }
    });
    StreamBody::new(chunks).boxed()
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

fn failed(e: std::io::Error) -> Response<Body> {
    answer(status_of(&e), json!({ "error": e.to_string() }))
}

fn crashed(e: tokio::task::JoinError) -> Response<Body> {
    answer(
        StatusCode::INTERNAL_SERVER_ERROR,
        json!({ "error": e.to_string() }),
    )
}

/// Answers one request on the drive at `path`: a directory's listing or a
/// file (`GET`, `HEAD`), a write (`PUT`), a removal (`DELETE`).
async fn serve<B>(files: Files, method: &Method, path: &str, body: B) -> Response<Body>
where
    B: hyper::body::Body<Data = Bytes> + Send + Unpin + 'static,
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
                    let body = if *method == Method::HEAD {
                        full(Bytes::new())
                    } else {
                        file_body(file)
                    };
                    Response::builder()
                        .header(header::CONTENT_LENGTH, size)
                        .header("x-content-type-options", "nosniff")
                        .header(header::CONTENT_TYPE, files::content_type(name))
                        .body(body)
                        .unwrap()
                }
                Ok(Err(e)) => failed(e),
                Err(e) => crashed(e),
            }
        }
        Method::PUT => put(files, parts, body).await,
        Method::DELETE => match tokio::task::spawn_blocking(move || files.remove(&parts)).await {
            Ok(Ok(())) => answer(StatusCode::OK, json!({ "removed": path })),
            Ok(Err(e)) => failed(e),
            Err(e) => crashed(e),
        },
        _ => answer(
            StatusCode::METHOD_NOT_ALLOWED,
            json!({ "error": "GET, HEAD, PUT or DELETE" }),
        ),
    }
}

/// Writes the body to `parts`, whole or not at all, at most `MAX_PUT`.
async fn put<B>(files: Files, parts: Vec<String>, mut body: B) -> Response<Body>
where
    B: hyper::body::Body<Data = Bytes> + Send + Unpin + 'static,
    B::Error: std::fmt::Display,
{
    let started = {
        let p = parts.clone();
        tokio::task::spawn_blocking(move || files.begin_write(&p)).await
    };
    let mut write = match started {
        Ok(Ok(w)) => w,
        Ok(Err(e)) => return failed(e),
        Err(e) => return crashed(e),
    };
    let mut out = match write.file().try_clone() {
        Ok(f) => tokio::fs::File::from_std(f),
        Err(e) => return failed(e),
    };
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
        Err(e) => crashed(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::Full;

    #[test]
    fn a_sandbox_reads_the_inbox_and_writes_only_its_own_outbox() {
        let (get, put, del) = (Method::GET, Method::PUT, Method::DELETE);
        assert!(may("sb-1", &get, "/inbox/brief.md").is_ok());
        assert!(may("sb-1", &put, "/inbox/brief.md").is_err(), "read-only");
        assert!(may("sb-1", &put, "/outbox/sb-1/report.md").is_ok());
        assert!(may("sb-1", &del, "/outbox/sb-1/report.md").is_ok());
        assert!(
            may("sb-1", &get, "/outbox/sb-2/report.md").is_err(),
            "another's"
        );
        assert!(may("sb-1", &get, "/outbox").is_err());
        assert!(may("sb-1", &get, "/").is_err());
        assert!(
            may("sb-1", &get, "/outbox/sb-1/../sb-2/x").is_err(),
            "no way out"
        );
        assert!(may("sb-1", &Method::POST, "/outbox/sb-1/x").is_err());
    }

    #[tokio::test]
    async fn a_write_lands_whole_and_reads_back() {
        let root = std::env::temp_dir().join(format!("zapper-wb-drive-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let d = Drive::new(root.clone()).unwrap();
        let r = d
            .serve(
                "sb-1",
                &Method::PUT,
                "/outbox/sb-1/a.txt",
                Full::new(Bytes::from("hi")),
            )
            .await;
        assert_eq!(r.status(), StatusCode::CREATED);
        assert_eq!(
            std::fs::read_to_string(root.join("outbox/sb-1/a.txt")).unwrap(),
            "hi"
        );
        let r = d
            .serve(
                "sb-1",
                &Method::GET,
                "/outbox/sb-1/a.txt",
                Full::new(Bytes::new()),
            )
            .await;
        assert_eq!(r.status(), StatusCode::OK);
        let body = r.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b"hi");
        let r = d
            .serve(
                "sb-2",
                &Method::GET,
                "/outbox/sb-1/a.txt",
                Full::new(Bytes::new()),
            )
            .await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        let _ = std::fs::remove_dir_all(root);
    }
}
