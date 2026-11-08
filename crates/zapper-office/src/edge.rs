//! The office edge (docs/openshell-migration.md, phase 2): how the firm's
//! workers reach the office from OpenShell sandboxes, which have no channel
//! to the office but their supervisor's proxy.
//!
//! One HTTP service on host loopback, which the supervisors dial for
//! `host.openshell.internal`:
//!
//! - `POST /oauth/token`: the `office-edge` provider's token grant. A
//!   supervisor posts its sandbox's JWT-SVID (from the Firecracker driver,
//!   ES256, `sub` = `spiffe://<trust domain>/openshell/sandbox/<uuid>`) as a
//!   `client_credentials` client assertion; the edge checks it against the
//!   driver's JWKS and answers a short-lived token naming that sandbox.
//! - `/mcp`: the office's tools over MCP Streamable HTTP (JSON-RPC in a POST,
//!   one JSON answer), as the daemon's MCP gateway answered them.
//! - `/git/<repo>.git/...`: the firm's git server.
//!
//! The supervisor adds the token to the workers' requests outside the VM, so
//! no credential enters it. Every request but the grant must carry a token
//! the edge issued, for a sandbox the office still runs: the caller is the
//! sandbox the token names, never what the request says (a request the
//! supervisor didn't add a token to arrives with whatever the guest sent,
//! which the edge doesn't know).

use std::collections::{BTreeMap, HashMap};
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use bytes::Bytes;
use futures_util::future::BoxFuture;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use zapper_policy::facility::{McpCall, McpMethod};

use crate::openshell::{EdgeSettings, OpenShell};

/// How long a token the edge issues lasts.
const TOKEN_TTL: Duration = Duration::from_secs(300);
/// The most a grant request's body may be (a form with one JWT).
const MAX_FORM: usize = 64 << 10;
/// The most an MCP message may be.
const MAX_MCP: usize = 1 << 20;
/// How long a tool list and a tool call may take, as the daemon's gateway
/// allowed them.
const LIST_TIMEOUT: Duration = Duration::from_secs(10);
const CALL_TIMEOUT: Duration = Duration::from_secs(120);
/// The JWKS is fetched again for an unknown key at most this often.
const JWKS_REFETCH: Duration = Duration::from_secs(30);
/// Clock skew allowed on an SVID's `iat`.
const SKEW: u64 = 60;
/// The client assertion types a supervisor may name.
const ASSERTION_TYPES: [&str; 2] = [
    "urn:ietf:params:oauth:client-assertion-type:jwt-bearer",
    "urn:ietf:params:oauth:client-assertion-type:jwt-spiffe",
];

type Body = Full<Bytes>;

/// A worker as the edge knows them: the office's sandbox and its labels.
#[derive(Debug, Clone, PartialEq)]
pub struct Caller {
    pub session: String,
    pub labels: BTreeMap<String, String>,
}

/// What the edge serves, behind the identity it checks.
pub trait Backend: Send + Sync {
    /// The sandbox the gateway knows as `uuid`, if the office runs it.
    fn caller(&self, uuid: &str) -> Option<Caller>;
    /// The office's tool list (`op` "tools") or a tool call, for `caller`.
    fn tools<'a>(
        &'a self,
        caller: &'a Caller,
        op: &'a str,
        args: Value,
    ) -> BoxFuture<'a, Result<Value>>;
    /// The MCP server's instructions to the harness.
    fn instructions(&self) -> Option<String>;
    /// A git request (its path without `/git`), for `caller`.
    fn git<'a>(
        &'a self,
        caller: &'a Caller,
        req: Request<Incoming>,
    ) -> BoxFuture<'a, Response<Body>>;
}

/// The edge: its settings, the driver's keys, and the tokens it issued.
pub struct Edge {
    settings: EdgeSettings,
    backend: Arc<dyn Backend>,
    keys: Mutex<Keys>,
    /// Token → the sandbox it names and when it expires.
    tokens: Mutex<HashMap<String, Grant>>,
    /// SVIDs already used (`jti` → `exp`), so one can't be replayed.
    used: Mutex<HashMap<String, u64>>,
}

#[derive(Default)]
struct Keys {
    /// `kid` → the P-256 public key, uncompressed (`04 || x || y`).
    by_kid: HashMap<String, Vec<u8>>,
    fetched: Option<Instant>,
}

struct Grant {
    uuid: String,
    expires: Instant,
}

impl Edge {
    pub fn new(settings: EdgeSettings, backend: Arc<dyn Backend>) -> Edge {
        Edge {
            settings,
            backend,
            keys: Mutex::default(),
            tokens: Mutex::default(),
            used: Mutex::default(),
        }
    }

    /// The edge for the office's sandboxes on `os`.
    pub fn for_office(office: Arc<crate::Office>, os: Arc<OpenShell>) -> Edge {
        let settings = os.edge_settings().clone();
        Edge::new(settings, Arc::new(OfficeBackend { office, os }))
    }

    /// Serves the edge on `listener` until the task is dropped.
    pub async fn serve(self: Arc<Self>, listener: tokio::net::TcpListener) {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("office edge: accepting: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            let edge = self.clone();
            tokio::spawn(async move {
                let service = hyper::service::service_fn(move |req| {
                    let edge = edge.clone();
                    async move { Ok::<_, Infallible>(edge.handle(req).await) }
                });
                if let Err(e) = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await
                {
                    tracing::debug!("office edge: connection: {e}");
                }
            });
        }
    }

    /// One request.
    pub async fn handle(&self, mut req: Request<Incoming>) -> Response<Body> {
        let path = req.uri().path().to_string();
        if path == "/oauth/token" {
            if req.method() != Method::POST {
                return not_allowed("POST");
            }
            return match read_body(req, MAX_FORM).await {
                Some(body) => self.grant(&body).await,
                None => plain(StatusCode::PAYLOAD_TOO_LARGE, "request too large"),
            };
        }
        let mcp = path == "/mcp";
        let git = path.starts_with("/git/");
        if !mcp && !git {
            return plain(
                StatusCode::NOT_FOUND,
                "the office edge serves /mcp and /git/",
            );
        }
        // Not for browsers: a page can't reach it through a worker's
        // supervisor, and one that names an origin is refused (MCP's
        // Streamable HTTP asks servers to check it).
        if req.headers().contains_key(hyper::header::ORIGIN) {
            return plain(StatusCode::FORBIDDEN, "no cross-origin requests");
        }
        let caller = match self.authenticate(req.headers()) {
            Some(c) => c,
            None => {
                let mut r = plain(
                    StatusCode::UNAUTHORIZED,
                    "no token for a worker of the office",
                );
                r.headers_mut().insert(
                    hyper::header::WWW_AUTHENTICATE,
                    "Bearer realm=\"office\"".parse().unwrap(),
                );
                return r;
            }
        };
        if mcp {
            if req.method() != Method::POST {
                // No server-sent stream: every answer comes in the POST's.
                return not_allowed("POST");
            }
            return match read_body(req, MAX_MCP).await {
                Some(body) => self.mcp(&caller, &body).await,
                None => plain(StatusCode::PAYLOAD_TOO_LARGE, "request too large"),
            };
        }
        // `/git/<repo>.git/...` is `/<repo>.git/...` to the git server.
        let rest = path.strip_prefix("/git").unwrap_or(&path).to_string();
        let uri = match req.uri().query() {
            Some(q) => format!("{rest}?{q}"),
            None => rest,
        };
        match uri.parse() {
            Ok(u) => *req.uri_mut() = u,
            Err(_) => return plain(StatusCode::BAD_REQUEST, "bad path"),
        }
        self.backend.git(&caller, req).await
    }

    /// The worker the request's bearer token names, if the edge issued it,
    /// it hasn't expired, and the office still runs the sandbox.
    fn authenticate(&self, headers: &hyper::HeaderMap) -> Option<Caller> {
        let value = headers.get(hyper::header::AUTHORIZATION)?.to_str().ok()?;
        let token = value
            .strip_prefix("Bearer ")
            .or_else(|| value.strip_prefix("bearer "))?
            .trim();
        let uuid = {
            let mut tokens = self.tokens.lock().unwrap();
            let grant = tokens.get(token)?;
            if grant.expires <= Instant::now() {
                tokens.remove(token);
                return None;
            }
            grant.uuid.clone()
        };
        self.backend.caller(&uuid)
    }

    /// The token grant: a form with the supervisor's SVID.
    async fn grant(&self, body: &[u8]) -> Response<Body> {
        let form = parse_form(body);
        let field = |k: &str| form.get(k).map(String::as_str).unwrap_or("");
        if field("grant_type") != "client_credentials" {
            return oauth_error(StatusCode::BAD_REQUEST, "unsupported_grant_type");
        }
        if !ASSERTION_TYPES.contains(&field("client_assertion_type")) {
            return oauth_error(StatusCode::BAD_REQUEST, "invalid_request");
        }
        let uuid = match self.verify_svid(field("client_assertion")).await {
            Ok(uuid) => uuid,
            Err(e) => {
                tracing::info!("office edge: refused a token grant: {e:#}");
                return oauth_error(StatusCode::UNAUTHORIZED, "invalid_client");
            }
        };
        if self.backend.caller(&uuid).is_none() {
            tracing::info!(sandbox = %uuid, "office edge: refused a token grant: not a sandbox of the office");
            return oauth_error(StatusCode::UNAUTHORIZED, "invalid_client");
        }
        let token = random_token();
        {
            let now = Instant::now();
            let mut tokens = self.tokens.lock().unwrap();
            tokens.retain(|_, g| g.expires > now);
            tokens.insert(
                token.clone(),
                Grant {
                    uuid,
                    expires: now + TOKEN_TTL,
                },
            );
        }
        json_response(
            StatusCode::OK,
            &json!({ "access_token": token, "token_type": "Bearer", "expires_in": TOKEN_TTL.as_secs() }),
        )
    }

    /// Checks a JWT-SVID: signed by a key of the driver's JWKS (ES256), from
    /// its issuer, for the token endpoint, current, not seen before, naming
    /// a sandbox in the trust domain. The sandbox's UUID.
    pub async fn verify_svid(&self, jwt: &str) -> Result<String> {
        let mut parts = jwt.split('.');
        let (Some(h), Some(c), Some(s), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            bail!("not a JWT");
        };
        let header: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(h)?)?;
        if header["alg"] != "ES256" {
            bail!("alg {} (only ES256)", header["alg"]);
        }
        let kid = header["kid"].as_str().unwrap_or("").to_string();
        let key = self.key(&kid).await?;
        let signature = URL_SAFE_NO_PAD.decode(s)?;
        ring::signature::UnparsedPublicKey::new(&ring::signature::ECDSA_P256_SHA256_FIXED, &key)
            .verify(format!("{h}.{c}").as_bytes(), &signature)
            .map_err(|_| anyhow!("bad signature"))?;

        let claims: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(c)?)?;
        if claims["iss"] != self.settings.issuer.as_str() {
            bail!("issuer {}", claims["iss"]);
        }
        let aud_ok = match &claims["aud"] {
            Value::String(a) => *a == self.settings.token_url,
            Value::Array(a) => a.iter().any(|a| *a == self.settings.token_url.as_str()),
            _ => false,
        };
        if !aud_ok {
            bail!(
                "audience {} (not {})",
                claims["aud"],
                self.settings.token_url
            );
        }
        let now = unix_now();
        let exp = claims["exp"].as_u64().ok_or_else(|| anyhow!("no exp"))?;
        if exp <= now {
            bail!("expired");
        }
        if claims["iat"].as_u64().is_some_and(|iat| iat > now + SKEW) {
            bail!("issued in the future");
        }
        let prefix = format!("spiffe://{}/openshell/sandbox/", self.settings.trust_domain);
        let uuid = claims["sub"]
            .as_str()
            .and_then(|s| s.strip_prefix(&prefix))
            .filter(|u| !u.is_empty() && !u.contains('/'))
            .ok_or_else(|| {
                anyhow!(
                    "subject {} isn't a sandbox of {}",
                    claims["sub"],
                    self.settings.trust_domain
                )
            })?
            .to_string();
        let jti = claims["jti"].as_str().ok_or_else(|| anyhow!("no jti"))?;
        {
            let mut used = self.used.lock().unwrap();
            used.retain(|_, e| *e > now);
            if used.insert(jti.to_string(), exp).is_some() {
                bail!("SVID {jti} used before");
            }
        }
        Ok(uuid)
    }

    /// The driver's key `kid`, fetching its JWKS when the key is new to the
    /// edge (at most every `JWKS_REFETCH`).
    async fn key(&self, kid: &str) -> Result<Vec<u8>> {
        {
            let keys = self.keys.lock().unwrap();
            if let Some(k) = keys.by_kid.get(kid) {
                return Ok(k.clone());
            }
            if keys.fetched.is_some_and(|t| t.elapsed() < JWKS_REFETCH) {
                bail!("no key {kid:?} in the driver's JWKS");
            }
        }
        let body = http_get(&self.settings.jwks)
            .await
            .with_context(|| format!("fetching the driver's JWKS from {}", self.settings.jwks))?;
        let by_kid = parse_jwks(&body)?;
        let mut keys = self.keys.lock().unwrap();
        keys.by_kid = by_kid;
        keys.fetched = Some(Instant::now());
        keys.by_kid
            .get(kid)
            .cloned()
            .ok_or_else(|| anyhow!("no key {kid:?} in the driver's JWKS"))
    }

    /// Sets the driver's keys (tests, which have no driver).
    #[cfg(test)]
    fn set_keys(&self, jwks: &[u8]) {
        let mut keys = self.keys.lock().unwrap();
        keys.by_kid = parse_jwks(jwks).unwrap();
        keys.fetched = Some(Instant::now());
    }

    /// One MCP message from `caller`: a request is answered in the
    /// response's body; a notification or a response is accepted.
    async fn mcp(&self, caller: &Caller, body: &[u8]) -> Response<Body> {
        let msg: Value = match serde_json::from_slice(body) {
            Ok(v) => v,
            Err(e) => {
                return json_response(
                    StatusCode::BAD_REQUEST,
                    &rpc_error(Value::Null, -32700, format!("parse error: {e}")),
                )
            }
        };
        if !msg.is_object() {
            return json_response(
                StatusCode::BAD_REQUEST,
                &rpc_error(
                    Value::Null,
                    -32600,
                    "one JSON-RPC message per request".into(),
                ),
            );
        }
        match self.answer(caller, &msg).await {
            Some(reply) => json_response(StatusCode::OK, &reply),
            None => Response::builder()
                .status(StatusCode::ACCEPTED)
                .body(Body::default())
                .unwrap(),
        }
    }

    /// The answer to MCP message `msg`, as the daemon's gateway gave it; none
    /// for a notification or a response.
    async fn answer(&self, caller: &Caller, msg: &Value) -> Option<Value> {
        let id = msg.get("id")?.clone();
        let method = msg["method"].as_str()?;
        let result: std::result::Result<Value, (i64, String)> = match method {
            "initialize" => {
                let mut r = json!({
                    "protocolVersion": msg["params"]["protocolVersion"].as_str().unwrap_or("2025-06-18"),
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": { "name": crate::ops::SERVER, "version": env!("CARGO_PKG_VERSION") },
                });
                if let Some(i) = self.backend.instructions() {
                    r["instructions"] = json!(i);
                }
                Ok(r)
            }
            "ping" => Ok(json!({})),
            "tools/list" => {
                match tokio::time::timeout(
                    LIST_TIMEOUT,
                    self.backend.tools(caller, "tools", json!({})),
                )
                .await
                {
                    Ok(Ok(t)) => Ok(json!({ "tools": t })),
                    Ok(Err(e)) => Err((-32603, format!("{e:#}"))),
                    Err(_) => Err((-32603, "the office took too long to list its tools".into())),
                }
            }
            "tools/call" => {
                let tool = msg["params"]["name"].as_str().unwrap_or("");
                let args = msg["params"].get("arguments").cloned().unwrap_or(json!({}));
                let answer =
                    tokio::time::timeout(CALL_TIMEOUT, self.backend.tools(caller, tool, args))
                        .await
                        .unwrap_or_else(|_| Err(anyhow!("{tool} took too long")));
                Ok(match answer {
                    Ok(v) => json!({
                        "content": [{ "type": "text", "text": serde_json::to_string_pretty(&v).unwrap_or_default() }],
                        "isError": false,
                    }),
                    Err(e) => json!({
                        "content": [{ "type": "text", "text": format!("{e:#}") }],
                        "isError": true,
                    }),
                })
            }
            _ => Err((-32601, format!("method {method} not found"))),
        };
        Some(match result {
            Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
            Err((code, message)) => rpc_error(id, code, message),
        })
    }
}

/// The edge's backend in the office: callers are the OpenShell client's
/// sandboxes, the tools are `ops`, git is the firm's git server.
struct OfficeBackend {
    office: Arc<crate::Office>,
    os: Arc<OpenShell>,
}

impl OfficeBackend {
    /// The firm's current run `caller` works in.
    fn run(&self, caller: &Caller) -> Option<Arc<crate::FirmRun>> {
        let firm = caller.labels.get("firm")?;
        let run = self.office.run(firm).ok()?;
        (caller.labels.get("run") == Some(&run.id)).then_some(run)
    }
}

impl Backend for OfficeBackend {
    fn caller(&self, uuid: &str) -> Option<Caller> {
        self.os
            .caller(uuid)
            .map(|(session, labels)| Caller { session, labels })
    }

    fn tools<'a>(
        &'a self,
        caller: &'a Caller,
        op: &'a str,
        args: Value,
    ) -> BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            let list = op == "tools";
            let call = McpCall {
                session: caller.session.clone(),
                labels: caller.labels.clone(),
                server: crate::ops::SERVER.into(),
                method: if list {
                    McpMethod::ToolsList
                } else {
                    McpMethod::ToolsCall
                },
                tool: (!list).then(|| op.to_string()),
                arguments: (!list).then(|| args.clone()),
            };
            crate::ops::answer_verified(&self.office, &call, op, args).await
        })
    }

    fn instructions(&self) -> Option<String> {
        crate::ops::instructions()
    }

    fn git<'a>(
        &'a self,
        caller: &'a Caller,
        req: Request<Incoming>,
    ) -> BoxFuture<'a, Response<Body>> {
        Box::pin(async move {
            let Some(run) = self.run(caller) else {
                return plain(StatusCode::NOT_FOUND, "your firm isn't running");
            };
            if !run.config.git_internal() {
                return plain(StatusCode::NOT_FOUND, "the firm has no git server");
            }
            let member = caller.labels.get("member").cloned().unwrap_or_default();
            crate::facility::git_request_as(&run, &member, true, req).await
        })
    }
}

/// Reads a request's body, up to `max` bytes; `None` past that.
async fn read_body(req: Request<Incoming>, max: usize) -> Option<Bytes> {
    http_body_util::Limited::new(req.into_body(), max)
        .collect()
        .await
        .ok()
        .map(|b| b.to_bytes())
}

/// An `application/x-www-form-urlencoded` body's fields.
fn parse_form(body: &[u8]) -> HashMap<String, String> {
    String::from_utf8_lossy(body)
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (url_decode(k), url_decode(v))
        })
        .collect()
}

/// Percent-decoding, with `+` as a space (as forms have it).
fn url_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                let hex = std::str::from_utf8(&b[i + 1..i + 3])
                    .ok()
                    .and_then(|h| u8::from_str_radix(h, 16).ok());
                match hex {
                    Some(v) => {
                        out.push(v);
                        i += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A JWKS's P-256 keys by `kid`, uncompressed.
fn parse_jwks(body: &[u8]) -> Result<HashMap<String, Vec<u8>>> {
    let jwks: Value = serde_json::from_slice(body).context("the JWKS isn't JSON")?;
    let mut keys = HashMap::new();
    for k in jwks["keys"].as_array().into_iter().flatten() {
        if k["kty"] != "EC" || k["crv"] != "P-256" {
            continue;
        }
        let (Some(kid), Some(x), Some(y)) = (k["kid"].as_str(), k["x"].as_str(), k["y"].as_str())
        else {
            continue;
        };
        let (x, y) = (URL_SAFE_NO_PAD.decode(x)?, URL_SAFE_NO_PAD.decode(y)?);
        if x.len() != 32 || y.len() != 32 {
            continue;
        }
        let mut point = vec![4u8];
        point.extend_from_slice(&x);
        point.extend_from_slice(&y);
        keys.insert(kid.to_string(), point);
    }
    Ok(keys)
}

/// GETs a plain `http://host:port/path` URL (the driver's JWKS, on
/// loopback): its body, if the answer is 2xx.
async fn http_get(url: &str) -> Result<Vec<u8>> {
    let rest = url.strip_prefix("http://").ok_or_else(|| {
        anyhow!("{url}: only http:// URLs (the driver serves its JWKS on loopback)")
    })?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let stream = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::net::TcpStream::connect(authority),
    )
    .await
    .map_err(|_| anyhow!("connecting to {authority}: timed out"))??;
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
    tokio::spawn(conn);
    let req = Request::get(path)
        .header(hyper::header::HOST, authority)
        .body(Body::default())?;
    let resp = tokio::time::timeout(Duration::from_secs(10), sender.send_request(req))
        .await
        .map_err(|_| anyhow!("{url}: timed out"))??;
    if !resp.status().is_success() {
        bail!("{url}: {}", resp.status());
    }
    let body = http_body_util::Limited::new(resp.into_body(), 1 << 20)
        .collect()
        .await
        .map_err(|e| anyhow!("{url}: {e}"))?;
    Ok(body.to_bytes().to_vec())
}

/// A new token: 32 random bytes, hex (token68, as the supervisor wants).
fn random_token() -> String {
    rand::random::<[u8; 32]>()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn rpc_error(id: Value, code: i64, message: String) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn json_response(status: StatusCode, v: &Value) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(hyper::header::CONTENT_TYPE, "application/json")
        .header(hyper::header::CACHE_CONTROL, "no-store")
        .body(Body::from(serde_json::to_vec(v).unwrap_or_default()))
        .unwrap()
}

fn oauth_error(status: StatusCode, error: &str) -> Response<Body> {
    json_response(status, &json!({ "error": error }))
}

fn plain(status: StatusCode, text: &str) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(hyper::header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Body::from(format!("{text}\n")))
        .unwrap()
}

fn not_allowed(allow: &str) -> Response<Body> {
    let mut r = plain(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
    r.headers_mut()
        .insert(hyper::header::ALLOW, allow.parse().unwrap());
    r
}

#[cfg(test)]
#[path = "edge_tests.rs"]
mod tests;
