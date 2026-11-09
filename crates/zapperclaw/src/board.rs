//! The board: you, at the board port the warden forwards to
//! (`127.0.0.1:8974` on the host). Messages, workers, the drive and
//! transcripts, as JSON under `/api/`, and a page at `/` that shows them.
//!
//! Every request needs the board's token: `Authorization: Bearer …`, or
//! the cookie `/?token=…` sets. A write also needs `X-Zapper-UI: 1`, a
//! loopback `Host`, and, if it has one, an `Origin` naming that host.
//! Whatever came from a sandbox is shown as text.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::{header, Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use serde_json::{json, Value};
use zapper_openshell::edge::{full, json_response, plain, Body};

use crate::mail::{Draft, BOARD};
use crate::Office;

const COOKIE: &str = "zc_board";
const MAX_BODY: usize = 128 * 1024;
const PAGE: &str = include_str!("board.html");

pub struct Board {
    office: Arc<Office>,
    token: String,
}

/// A new board token: 32 random bytes, hex.
pub fn new_token() -> String {
    rand::random::<[u8; 32]>()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

impl Board {
    pub fn new(office: Arc<Office>, token: String) -> Arc<Board> {
        Arc::new(Board { office, token })
    }

    pub async fn serve(self: Arc<Self>, listener: tokio::net::TcpListener) {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("board: accepting: {e}");
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    continue;
                }
            };
            let board = self.clone();
            tokio::spawn(async move {
                let service = hyper::service::service_fn(move |req| {
                    let board = board.clone();
                    async move { Ok::<_, Infallible>(board.handle(req).await) }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    }

    fn token_ok(&self, req: &Request<Incoming>) -> bool {
        let given = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(str::to_string)
            .or_else(|| cookie(req, COOKIE));
        given.is_some_and(|g| same(g.as_bytes(), self.token.as_bytes()))
    }

    pub async fn handle(&self, req: Request<Incoming>) -> Response<Body> {
        let path = req.uri().path().to_string();
        let query = query(req.uri().query().unwrap_or(""));
        if req.method() == Method::GET && path == "/" {
            if let Some(t) = query.get("token") {
                if !same(t.as_bytes(), self.token.as_bytes()) {
                    return plain(StatusCode::UNAUTHORIZED, "not the board's token");
                }
                return Response::builder()
                    .status(StatusCode::SEE_OTHER)
                    .header(header::LOCATION, "/")
                    .header(
                        header::SET_COOKIE,
                        format!("{COOKIE}={t}; Path=/; HttpOnly; SameSite=Strict"),
                    )
                    .body(full(""))
                    .unwrap();
            }
        }
        if !self.token_ok(&req) {
            return plain(
                StatusCode::UNAUTHORIZED,
                "the board's token, please: open /?token=… once",
            );
        }
        if req.method() == Method::POST {
            if let Err(why) = write_allowed(&req) {
                return plain(StatusCode::FORBIDDEN, &why);
            }
        }
        match self.route(req, &path, &query).await {
            Ok(r) => r,
            Err(e) => json_response(
                StatusCode::BAD_REQUEST,
                &json!({ "error": format!("{e:#}") }),
            ),
        }
    }

    async fn route(
        &self,
        req: Request<Incoming>,
        path: &str,
        q: &HashMap<String, String>,
    ) -> Result<Response<Body>> {
        let o = &self.office;
        let ok = |v: Value| Ok(json_response(StatusCode::OK, &v));
        let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
        match (req.method().clone(), parts.as_slice()) {
            (Method::GET, [""]) => Ok(Response::builder()
                .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
                .header("Content-Security-Policy", "default-src 'self'; script-src 'unsafe-inline' 'self'; style-src 'unsafe-inline'")
                .body(full(PAGE))
                .unwrap()),
            (Method::GET, ["api", "workers"]) => ok(o.roster()),
            (Method::GET, ["api", "inbox"]) => {
                let unread = q.get("unread").is_some_and(|v| v == "1");
                let po = o.post.lock().unwrap();
                ok(json!({ "unread": po.unread(BOARD).len(), "messages": po.inbox(BOARD, unread, 200) }))
            }
            (Method::GET, ["api", "threads"]) => {
                ok(json!(o.post.lock().unwrap().threads()))
            }
            (Method::GET, ["api", "thread", id]) => {
                let mut po = o.post.lock().unwrap();
                let t = po.thread(id, BOARD)?;
                let ids: Vec<String> = t["messages"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|m| m["id"].as_str().map(str::to_string))
                    .collect();
                po.mark_read(&o.journal, BOARD, &ids)?;
                ok(t)
            }
            (Method::GET, ["api", "desk"]) => {
                ok(serde_json::to_value(o.post.lock().unwrap().desk(BOARD))?)
            }
            (Method::GET, ["api", "transcript", name]) => {
                let w = o.worker(name).ok_or_else(|| anyhow!("no worker {name}"))?;
                let lines = q.get("lines").and_then(|n| n.parse().ok()).unwrap_or(200usize).min(2000);
                ok(json!({ "worker": w.name, "lines": tail(&o.dir.join("transcripts").join(format!("{}.jsonl", w.name)), lines) }))
            }
            (Method::GET, ["api", "drive", rest @ ..]) => {
                let mut parts = vec!["inbox".to_string()];
                parts.extend(zapper_workbench::drive::files::components(&rest.join("/"))?);
                match o.files.find(&parts)? {
                    zapper_workbench::drive::files::Found::Dir(e) => ok(json!({ "entries": e })),
                    zapper_workbench::drive::files::Found::File { size, .. } => ok(json!({ "size": size })),
                }
            }
            (Method::POST, ["api", "mail"]) => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Mail {
                    to: Vec<String>,
                    #[serde(default)]
                    subject: String,
                    body: String,
                    #[serde(default)]
                    in_reply_to: Option<String>,
                }
                let body = read(req).await?;
                let m: Mail = serde_json::from_slice(&body)?;
                let s = o.send(Draft {
                    from: BOARD.into(),
                    to: m.to,
                    subject: m.subject,
                    body: m.body,
                    in_reply_to: m.in_reply_to,
                    due: None,
                })?;
                ok(json!({ "sent": s.id, "thread": s.thread }))
            }
            (m, _) if m != Method::GET && m != Method::POST => {
                Ok(zapper_openshell::edge::not_allowed("GET, POST"))
            }
            _ => Ok(plain(StatusCode::NOT_FOUND, "no such page")),
        }
    }
}

/// Whether a write may come from where it came: `X-Zapper-UI: 1`, a
/// loopback `Host`, and an `Origin`, if any, naming that host.
fn write_allowed(req: &Request<Incoming>) -> std::result::Result<(), String> {
    let h = |k: &str| req.headers().get(k).and_then(|v| v.to_str().ok());
    if h("x-zapper-ui") != Some("1") {
        return Err("a write needs X-Zapper-UI: 1".into());
    }
    let host = h("host").unwrap_or("");
    if !is_loopback_host(host) {
        return Err(format!("a write comes to a loopback host, not {host:?}"));
    }
    if let Some(origin) = h("origin") {
        let named = origin
            .strip_prefix("http://")
            .is_some_and(|o| o.trim_end_matches('/') == host);
        if !named {
            return Err(format!(
                "a write's Origin is the board's own, not {origin:?}"
            ));
        }
    }
    Ok(())
}

fn is_loopback_host(host: &str) -> bool {
    let name = match host.rsplit_once(':') {
        Some((n, p)) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => n,
        _ => host,
    };
    matches!(name, "127.0.0.1" | "localhost" | "[::1]")
}

/// Compares in time independent of where they differ.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn cookie(req: &Request<Incoming>, name: &str) -> Option<String> {
    req.headers()
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.to_string())
}

fn query(q: &str) -> HashMap<String, String> {
    q.split('&')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

async fn read(req: Request<Incoming>) -> Result<Bytes> {
    let b = http_body_util::Limited::new(req.into_body(), MAX_BODY)
        .collect()
        .await
        .map_err(|e| anyhow!("the request's body: {e}"))?;
    Ok(b.to_bytes())
}

/// How much of a transcript's end the board reads.
const TAIL_BYTES: u64 = 4 << 20;

/// The last `n` lines of a JSON-lines file, parsed (unparsable ones as
/// text), from no more than its last `TAIL_BYTES`.
fn tail(path: &std::path::Path, n: usize) -> Vec<Value> {
    let bytes = crate::journal::read_tail(path, TAIL_BYTES).unwrap_or_default();
    let whole = std::fs::metadata(path).is_ok_and(|m| m.len() <= TAIL_BYTES);
    let text = String::from_utf8_lossy(&bytes);
    let mut lines: Vec<&str> = text.lines().collect();
    if !whole && !lines.is_empty() {
        // The first line was cut where the read began.
        lines.remove(0);
    }
    lines[lines.len().saturating_sub(n)..]
        .iter()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|_| json!(l)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_hosts() {
        for ok in ["127.0.0.1:8974", "localhost:8974", "[::1]:80", "localhost"] {
            assert!(is_loopback_host(ok), "{ok}");
        }
        for bad in ["evil.example:8974", "127.0.0.1.evil:1", "", "10.0.0.1:8974"] {
            assert!(!is_loopback_host(bad), "{bad}");
        }
    }

    #[test]
    fn tokens_compare_whole() {
        assert!(same(b"abc", b"abc"));
        assert!(!same(b"abc", b"abd"));
        assert!(!same(b"abc", b"abcd"));
        assert_eq!(new_token().len(), 64);
    }
}

#[cfg(test)]
mod tail_tests {
    #[test]
    fn a_transcripts_tail_is_its_last_lines() {
        let p = std::env::temp_dir().join(format!("zc-tail-{}", zapper_util::random_id(8)));
        std::fs::write(&p, "{\"a\":1}\nnot json\n{\"a\":3}\n").unwrap();
        let t = super::tail(&p, 2);
        assert_eq!(
            t,
            vec![serde_json::json!("not json"), serde_json::json!({ "a": 3 })]
        );
        assert!(super::tail(&p.with_extension("none"), 5).is_empty());
        let _ = std::fs::remove_file(p);
    }
}
