//! A session's HTTP traffic, for inspecting it like a browser's network panel.
//!
//! Inspected requests (TLS terminated by the proxy, or plain HTTP) are kept
//! with their headers, the start of their bodies and their timing. Passed-
//! through connections are kept as one `CONNECT` record each, with bytes and
//! duration, since their content is encrypted end to end.
//!
//! Request headers are recorded as the guest sent them, before the proxy adds
//! secrets, so injected values never appear here. Records live in memory only.

use std::collections::VecDeque;
use std::io::Read;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Instant;

use bytes::Bytes;
use http_body::{Body as HttpBody, Frame, SizeHint};
use http_body_util::combinators::BoxBody;
use serde_json::{json, Value};
use zapper_policy::Action;

type Body = BoxBody<Bytes, hyper::Error>;

/// Bytes of each body kept per request.
const BODY_CAP: usize = 64 << 10;
/// Records kept per session.
const MAX_RECORDS: usize = 1000;
/// Body bytes kept per session; the oldest bodies are dropped first.
const MAX_BODY_BYTES: usize = 16 << 20;

#[derive(Default)]
pub struct Captured {
    pub data: Vec<u8>,
    pub total: u64,
}

impl Captured {
    fn push(&mut self, chunk: &[u8]) {
        self.total += chunk.len() as u64;
        let room = BODY_CAP.saturating_sub(self.data.len());
        self.data.extend_from_slice(&chunk[..chunk.len().min(room)]);
    }
}

pub struct Record {
    pub id: u64,
    pub ts: f64,
    pub scheme: &'static str,
    pub method: String,
    pub host: String,
    pub port: u16,
    pub path: Option<String>,
    pub action: Action,
    pub rule: Option<String>,
    pub scope: Option<String>,
    pub secrets: Vec<String>,
    /// Values the proxy added; shown as `***`. A destination that echoes
    /// request headers back returns them in its response.
    pub redact: Vec<String>,
    pub error: Option<String>,
    pub status: Option<u16>,
    pub req_headers: Vec<(String, String)>,
    pub resp_headers: Vec<(String, String)>,
    pub req_body: Captured,
    pub resp_body: Captured,
    start: Instant,
    pub ttfb_ms: Option<f64>,
    pub total_ms: Option<f64>,
    pub done: bool,
}

pub type Shared = Arc<Mutex<Record>>;

#[derive(Default)]
pub struct RequestLog {
    records: VecDeque<Shared>,
    next: u64,
    body_bytes: usize,
}

pub fn headers(h: &hyper::HeaderMap) -> Vec<(String, String)> {
    h.iter()
        .map(|(k, v)| {
            (
                k.to_string(),
                String::from_utf8_lossy(v.as_bytes()).into_owned(),
            )
        })
        .collect()
}

impl RequestLog {
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        &mut self,
        scheme: &'static str,
        method: &str,
        host: &str,
        port: u16,
        path: Option<&str>,
        req_headers: Vec<(String, String)>,
    ) -> Shared {
        self.next += 1;
        let rec = Arc::new(Mutex::new(Record {
            id: self.next,
            ts: super::now(),
            scheme,
            method: method.to_string(),
            host: host.to_string(),
            port,
            path: path.map(str::to_string),
            action: Action::Allow,
            rule: None,
            scope: None,
            secrets: vec![],
            redact: vec![],
            error: None,
            status: None,
            req_headers,
            resp_headers: vec![],
            req_body: Captured::default(),
            resp_body: Captured::default(),
            start: Instant::now(),
            ttfb_ms: None,
            total_ms: None,
            done: false,
        }));
        self.records.push_back(rec.clone());
        while self.records.len() > MAX_RECORDS {
            if let Some(old) = self.records.pop_front() {
                let old = old.lock().unwrap();
                self.body_bytes = self
                    .body_bytes
                    .saturating_sub(old.req_body.data.len() + old.resp_body.data.len());
            }
        }
        rec
    }

    /// Counts a finished record's bodies against the budget, dropping the
    /// oldest bodies when it's exceeded. Called without the record's lock:
    /// the log's lock comes first, then records' (see `finish`).
    fn account(&mut self, rec: &Shared, bytes: usize) {
        self.body_bytes += bytes;
        let mut i = 0;
        while self.body_bytes > MAX_BODY_BYTES && i < self.records.len() {
            if Arc::ptr_eq(&self.records[i], rec) {
                i += 1;
                continue;
            }
            let mut old = self.records[i].lock().unwrap();
            if old.done {
                let freed = old.req_body.data.len() + old.resp_body.data.len();
                old.req_body.data = Vec::new();
                old.resp_body.data = Vec::new();
                self.body_bytes = self.body_bytes.saturating_sub(freed);
            }
            i += 1;
        }
    }

    pub fn list(&self) -> Vec<Value> {
        self.records
            .iter()
            .map(|r| summary(&r.lock().unwrap()))
            .collect()
    }

    pub fn get(&self, id: u64) -> Option<Value> {
        self.records
            .iter()
            .find(|r| r.lock().unwrap().id == id)
            .map(|r| detail(&r.lock().unwrap()))
    }
}

/// Marks a record finished and counts it.
///
/// Lock order: the log, then a record. Readers of the log (`list`, `get`)
/// lock records while holding the log, so the record's lock is released
/// here before the log's is taken; holding both the other way round
/// deadlocked the proxy against the officers' digest.
pub fn finish(log: &Mutex<RequestLog>, rec: &Shared) {
    let bytes = {
        let mut r = rec.lock().unwrap();
        if r.done {
            return;
        }
        r.done = true;
        let elapsed = r.start.elapsed().as_secs_f64() * 1000.0;
        r.total_ms.get_or_insert(elapsed);
        r.req_body.data.len() + r.resp_body.data.len()
    };
    log.lock().unwrap().account(rec, bytes);
}

pub fn mark_ttfb(rec: &Shared) {
    let mut r = rec.lock().unwrap();
    r.ttfb_ms = Some(r.start.elapsed().as_secs_f64() * 1000.0);
}

fn header<'a>(h: &'a [(String, String)], name: &str) -> Option<&'a str> {
    h.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

fn url(r: &Record) -> String {
    match &r.path {
        Some(p) => format!("{}:{}{}", r.host, r.port, p),
        None => format!("{}:{}", r.host, r.port),
    }
}

fn summary(r: &Record) -> Value {
    json!({
        "id": r.id, "ts": r.ts, "scheme": r.scheme, "method": r.method, "url": url(r),
        "status": r.status, "action": r.action, "rule": r.rule, "error": r.error,
        "type": header(&r.resp_headers, "content-type"),
        "req_size": r.req_body.total, "resp_size": r.resp_body.total,
        "ttfb_ms": r.ttfb_ms, "total_ms": r.total_ms.or_else(|| (!r.done).then(|| r.start.elapsed().as_secs_f64() * 1000.0)),
        "done": r.done, "inspected": r.path.is_some(),
    })
}

/// Replaces each added secret value with `***`.
fn redact(text: &str, values: &[String]) -> String {
    let mut out = text.to_string();
    for v in values.iter().filter(|v| v.len() >= 4) {
        out = out.replace(v.as_str(), "***");
    }
    out
}

fn redact_headers(h: &[(String, String)], values: &[String]) -> Vec<(String, String)> {
    h.iter()
        .map(|(k, v)| (k.clone(), redact(v, values)))
        .collect()
}

/// A body for display: decoded per `content-encoding`, as text when it is
/// UTF-8, base64 otherwise. Added secret values are shown as `***`; binary
/// bodies holding one are not shown.
fn body_json(c: &Captured, h: &[(String, String)], values: &[String]) -> Value {
    use base64::Engine as _;
    let encoding = header(h, "content-encoding")
        .unwrap_or("")
        .to_ascii_lowercase();
    let decoded = decode(&c.data, &encoding);
    let truncated = (c.data.len() as u64) < c.total;
    let shown = decoded.as_deref().unwrap_or(&c.data);
    let mut v = json!({ "total": c.total, "kept": c.data.len(), "truncated": truncated, "encoding": (!encoding.is_empty()).then_some(encoding) });
    match std::str::from_utf8(shown) {
        Ok(t) => v["text"] = json!(redact(t, values)),
        Err(e) if e.valid_up_to() > shown.len().saturating_sub(4) && truncated => {
            v["text"] = json!(redact(&String::from_utf8_lossy(shown), values))
        }
        Err(_)
            if values
                .iter()
                .any(|s| shown.windows(s.len()).any(|w| w == s.as_bytes())) =>
        {
            v["withheld"] = json!("contains an added secret")
        }
        Err(_) => v["base64"] = json!(base64::engine::general_purpose::STANDARD.encode(shown)),
    }
    v
}

/// Decodes as much of a (possibly truncated) compressed body as possible.
fn decode(data: &[u8], encoding: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let reader: Box<dyn Read> = match encoding {
        "gzip" | "x-gzip" => Box::new(flate2::read::MultiGzDecoder::new(data)),
        "deflate" => Box::new(flate2::read::ZlibDecoder::new(data)),
        "br" => Box::new(brotli::Decompressor::new(data, 4096)),
        _ => return None,
    };
    let _ = reader.take(4 * BODY_CAP as u64).read_to_end(&mut out);
    (!out.is_empty()).then_some(out)
}

fn detail(r: &Record) -> Value {
    let mut v = summary(r);
    v["host"] = json!(r.host);
    v["port"] = json!(r.port);
    v["path"] = json!(r.path);
    v["scope"] = json!(r.scope);
    v["secrets"] = json!(r.secrets);
    v["request_headers"] = json!(redact_headers(&r.req_headers, &r.redact));
    v["response_headers"] = json!(redact_headers(&r.resp_headers, &r.redact));
    v["request_body"] = body_json(&r.req_body, &r.req_headers, &r.redact);
    v["response_body"] = body_json(&r.resp_body, &r.resp_headers, &r.redact);
    v
}

#[derive(Clone, Copy)]
pub enum Side {
    Request,
    Response,
}

/// Passes a body through unchanged, keeping its start and counting it.
pub struct Tee {
    inner: Body,
    rec: Shared,
    log: Arc<Mutex<RequestLog>>,
    side: Side,
    ended: bool,
}

impl Tee {
    pub fn wrap(inner: Body, rec: Shared, log: Arc<Mutex<RequestLog>>, side: Side) -> Body {
        use http_body_util::BodyExt as _;
        Tee {
            inner,
            rec,
            log,
            side,
            ended: false,
        }
        .boxed()
    }

    fn end(&mut self) {
        if self.ended {
            return;
        }
        self.ended = true;
        if let Side::Response = self.side {
            finish(&self.log, &self.rec);
        }
    }
}

impl HttpBody for Tee {
    type Data = Bytes;
    type Error = hyper::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, hyper::Error>>> {
        let polled = Pin::new(&mut self.inner).poll_frame(cx);
        match &polled {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    let mut r = self.rec.lock().unwrap();
                    match self.side {
                        Side::Request => r.req_body.push(data),
                        Side::Response => r.resp_body.push(data),
                    }
                }
            }
            Poll::Ready(Some(Err(e))) => {
                self.rec
                    .lock()
                    .unwrap()
                    .error
                    .get_or_insert_with(|| e.to_string());
                self.end();
            }
            Poll::Ready(None) => self.end(),
            Poll::Pending => {}
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

impl Drop for Tee {
    /// A body dropped before its end (the client went away) still finishes
    /// the record.
    fn drop(&mut self) {
        self.end();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_and_budgets() {
        let mut log = RequestLog::default();
        let rec = log.start(
            "https",
            "GET",
            "a.example",
            443,
            Some("/x"),
            vec![("accept".into(), "*/*".into())],
        );
        rec.lock()
            .unwrap()
            .resp_body
            .push(&vec![b'a'; BODY_CAP + 10]);
        let log = Mutex::new(log);
        finish(&log, &rec);
        let d = log.lock().unwrap().get(1).unwrap();
        assert_eq!(d["url"], "a.example:443/x");
        assert_eq!(d["response_body"]["total"], BODY_CAP as u64 + 10);
        assert_eq!(d["response_body"]["kept"], BODY_CAP);
        assert_eq!(d["response_body"]["truncated"], true);
        assert!(d["total_ms"].is_number());
    }

    /// Runs `f` on a thread and fails the test if it doesn't end in time.
    fn within(secs: u64, f: impl FnOnce() + Send + 'static) {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            f();
            let _ = tx.send(());
        });
        rx.recv_timeout(std::time::Duration::from_secs(secs))
            .expect("deadlock: did not finish in time");
    }

    #[test]
    fn over_budget_finish_does_not_lock_itself() {
        within(10, || {
            let log = Mutex::new(RequestLog::default());
            // Enough kept bodies to pass the budget, so `account` walks the records.
            for _ in 0..(MAX_BODY_BYTES / BODY_CAP + 4) {
                let rec =
                    log.lock()
                        .unwrap()
                        .start("https", "GET", "a.example", 443, Some("/"), vec![]);
                rec.lock().unwrap().resp_body.push(&vec![b'a'; BODY_CAP]);
                finish(&log, &rec);
            }
            assert!(log.lock().unwrap().body_bytes <= MAX_BODY_BYTES + BODY_CAP);
        });
    }

    #[test]
    fn finishing_while_listing_does_not_deadlock() {
        within(20, || {
            let log = Arc::new(Mutex::new(RequestLog::default()));
            let reader = {
                let log = log.clone();
                std::thread::spawn(move || {
                    for _ in 0..2000 {
                        let _ = log.lock().unwrap().list();
                    }
                })
            };
            for _ in 0..2000 {
                let rec =
                    log.lock()
                        .unwrap()
                        .start("https", "GET", "a.example", 443, Some("/"), vec![]);
                finish(&log, &rec);
            }
            reader.join().unwrap();
        });
    }

    #[test]
    fn decodes_gzip() {
        use std::io::Write;
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(b"{\"ok\":true}").unwrap();
        let data = gz.finish().unwrap();
        let c = Captured {
            total: data.len() as u64,
            data,
        };
        let v = body_json(&c, &[("content-encoding".into(), "gzip".into())], &[]);
        assert_eq!(v["text"], "{\"ok\":true}");
        let v = body_json(
            &c,
            &[("content-encoding".into(), "gzip".into())],
            &["true".into()],
        );
        assert_eq!(v["text"], "{\"ok\":***}");
    }
}
