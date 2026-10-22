//! The MCP gateway.
//!
//! A session whose profile lists `mcp` servers gets one MCP endpoint,
//! `http://mcp.zapper.internal/mcp`, served by the daemon: the name resolves
//! in the session's DNS to a synthetic address and is answered here, never
//! looked up or connected to. Behind it the daemon talks to the registered
//! servers (`mcp.servers`) from the host, so what they hold (credentials,
//! network reach, files) never enters the guest. Every tool call, resource
//! read and prompt is decided by the session's `mcp` rules, `ask` waits for an
//! operator like any other approval, and each is recorded in the audit log.

mod gateway;
#[cfg(test)]
mod tests;
mod upstream;

use std::convert::Infallible;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncWrite};
use zapper_policy::{
    evaluate_mcp, Action, AnyRule, Decision, Kind, McpRequest, McpRule, McpServer,
};

pub use gateway::Gateway;
use gateway::{Backend, Call};

use super::session::Session;
use super::{spec, Daemon, EventBody};

/// The name the guest reaches the gateway by.
pub const GATEWAY_HOST: &str = "mcp.zapper.internal";
/// Where the guest finds it.
pub const ENDPOINT: &str = "http://mcp.zapper.internal/mcp";
const PATH: &str = "/mcp";
/// Largest message accepted from the guest.
const MAX_MESSAGE: usize = 4 * 1024 * 1024;

type Body = BoxBody<Bytes, hyper::Error>;

/// Whether `host:port` is the session's gateway.
pub fn serves(s: &Session, host: &str, port: u16) -> bool {
    s.mcp.is_some() && port == 80 && host.eq_ignore_ascii_case(GATEWAY_HOST)
}

/// Serves the gateway on a connection from the guest.
pub async fn serve<S>(d: Arc<Daemon>, s: Arc<Session>, io: S)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let svc = service_fn(move |req| {
        let (d, s) = (d.clone(), s.clone());
        async move { Ok::<_, Infallible>(handle(d, s, req).await) }
    });
    let _ = hyper::server::conn::http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(Duration::from_secs(60))
        .serve_connection(TokioIo::new(io), svc)
        .await;
}

fn reply(status: StatusCode, body: Option<Value>) -> Response<Body> {
    let (body, ctype) = match body {
        Some(v) => (Bytes::from(v.to_string()), "application/json"),
        None => (Bytes::new(), "text/plain"),
    };
    Response::builder()
        .status(status)
        .header("content-type", ctype)
        .body(Full::new(body).map_err(|n: Infallible| match n {}).boxed())
        .unwrap()
}

/// Handles one HTTP request to the gateway endpoint.
pub async fn handle(d: Arc<Daemon>, s: Arc<Session>, req: Request<Incoming>) -> Response<Body> {
    let Some(gw) = s.mcp.clone() else {
        return reply(StatusCode::NOT_FOUND, None);
    };
    process(&gw, &Ctx { d, s }, req).await
}

/// Streamable HTTP, answered with plain JSON: a POST carries a message or a
/// batch and gets the answer, or 202 when nothing in it needs one.
async fn process<B: Backend>(gw: &Gateway, b: &B, req: Request<Incoming>) -> Response<Body> {
    if req.uri().path().trim_end_matches('/') != PATH {
        return reply(StatusCode::NOT_FOUND, None);
    }
    match *req.method() {
        Method::POST => {}
        // No server-initiated stream: the lists don't change under a session.
        Method::GET => {
            return Response::builder()
                .status(StatusCode::METHOD_NOT_ALLOWED)
                .header("allow", "POST")
                .body(
                    Full::new(Bytes::new())
                        .map_err(|n: Infallible| match n {})
                        .boxed(),
                )
                .unwrap()
        }
        Method::DELETE => return reply(StatusCode::OK, None),
        _ => return reply(StatusCode::METHOD_NOT_ALLOWED, None),
    }
    let body = match Limited::new(req.into_body(), MAX_MESSAGE).collect().await {
        Ok(b) => b.to_bytes(),
        Err(_) => return reply(StatusCode::PAYLOAD_TOO_LARGE, None),
    };
    let msg: Value = match serde_json::from_slice(&body) {
        Ok(m) => m,
        Err(e) => {
            return reply(
                StatusCode::BAD_REQUEST,
                Some(json!({
                    "jsonrpc": "2.0", "id": null,
                    "error": { "code": -32700, "message": format!("parse error: {e}") },
                })),
            )
        }
    };
    let answer = match msg {
        Value::Array(batch) => {
            let all =
                futures_util::future::join_all(batch.into_iter().map(|m| gw.handle(b, m))).await;
            let all: Vec<Value> = all.into_iter().flatten().collect();
            (!all.is_empty()).then_some(Value::Array(all))
        }
        m => gw.handle(b, m).await,
    };
    match answer {
        Some(v) => reply(StatusCode::OK, Some(v)),
        None => reply(StatusCode::ACCEPTED, None),
    }
}

/// The daemon behind one session's gateway.
struct Ctx {
    d: Arc<Daemon>,
    s: Arc<Session>,
}

/// Escapes glob metacharacters, so a rule made from a name matches that name.
fn glob_escape(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if "*?[]{}\\!".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

impl Backend for Ctx {
    fn server(&self, name: &str) -> Option<McpServer> {
        self.d.config.read().unwrap().mcp.servers.get(name).cloned()
    }

    fn attached(&self) -> Vec<String> {
        let c = self.d.config.read().unwrap();
        self.s
            .resolved
            .mcp
            .iter()
            .filter(|n| c.mcp.servers.contains_key(*n))
            .cloned()
            .collect()
    }

    fn secret(&self, name: &str) -> Result<String, String> {
        let c = self.d.config.read().unwrap();
        let secret = c
            .secrets
            .get(name)
            .ok_or_else(|| format!("unknown secret {name}"))?;
        secret
            .source
            .resolve()
            .map_err(|e| format!("secret {name}: {e:#}"))
    }

    fn decide(&self, req: &McpRequest) -> Decision {
        let c = self.d.config.read().unwrap();
        let layers = self.s.layers(&c);
        evaluate_mcp(&spec::layers_of(&layers), req)
    }

    async fn ask(&self, req: &McpRequest, args: &Value) -> bool {
        // In a firm, officers take minutes: a held tool call is no good to a
        // worker, and access requests are for the network.
        if self.s.firm.is_some() {
            return false;
        }
        let mut subject = format!("{} {}:{}", req.kind, req.server, req.name);
        if !args.is_null() {
            let a = args.to_string();
            let a: String = a.chars().take(200).collect();
            subject.push_str(&format!(" {a}"));
        }
        let suggestion = AnyRule::Mcp(McpRule {
            id: None,
            action: Action::Allow,
            server: glob_escape(&req.server),
            kind: Some(req.kind),
            name: glob_escape(&req.name),
            final_: false,
            comment: Some("approved".into()),
        });
        self.d.ask(&self.s, Kind::Mcp, subject, suggestion).await
    }

    fn record(&self, call: Call) {
        let counter = if call.allowed {
            &self.s.counters.mcp_allowed
        } else {
            &self.s.counters.mcp_denied
        };
        counter.fetch_add(1, Ordering::Relaxed);
        self.d.emit(
            Some(&self.s),
            EventBody::Mcp {
                server: call.server,
                kind: call.kind,
                name: call.name,
                action: if call.allowed {
                    Action::Allow
                } else {
                    Action::Deny
                },
                rule: call.rule,
                scope: call.scope,
                wait_ms: call.wait_ms,
                args: call.args,
                error: call.error,
            },
        );
    }
}
