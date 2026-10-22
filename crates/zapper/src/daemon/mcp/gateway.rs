//! The gateway proper: one MCP server for the guest, assembled from the
//! servers behind it. Tools and prompts are namespaced `server__name`,
//! resources get a gateway URI, and every call, read and prompt is decided by
//! the session's `mcp` rules before it reaches a server.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::sync::{Mutex, Semaphore};
use zapper_policy::{Action, Decision, McpKind, McpRequest, McpServer};

use super::upstream::{RpcError, Upstream, PROTOCOL};

/// Separates the server from the tool in a namespaced name.
pub const SEP: &str = "__";
/// The gateway's own tool: the servers behind it and how they are doing.
pub const STATUS_TOOL: &str = "zapper-status";
/// Prefix of the URIs the gateway gives resources.
const RESOURCE_SCHEME: &str = "zapper-mcp://";
const SUPPORTED: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];
const CALL_TIMEOUT: Duration = Duration::from_secs(300);
const LIST_TIMEOUT: Duration = Duration::from_secs(30);
/// Most list pages followed per server.
const MAX_PAGES: usize = 20;

/// What the gateway needs from the daemon.
pub trait Backend: Send + Sync {
    /// The registered server, from the running config.
    fn server(&self, name: &str) -> Option<McpServer>;
    /// The servers this session may reach.
    fn attached(&self) -> Vec<String>;
    /// The value of a secret, for a server's headers or environment.
    fn secret(&self, name: &str) -> Result<String, String>;
    /// What the session's rules say about a request.
    fn decide(&self, req: &McpRequest) -> Decision;
    /// Waits for an operator to answer an `ask`.
    fn ask(&self, req: &McpRequest, args: &Value) -> impl Future<Output = bool> + Send;
    /// Records a decided request.
    fn record(&self, call: Call);
}

/// A request through the gateway and what became of it.
#[derive(Debug, Clone)]
pub struct Call {
    pub server: String,
    pub kind: McpKind,
    pub name: String,
    pub allowed: bool,
    pub rule: Option<String>,
    pub scope: Option<String>,
    /// Time spent waiting for the decision.
    pub wait_ms: f64,
    pub error: Option<String>,
    /// The arguments, cut short.
    pub args: Option<String>,
}

type Slot = Arc<Mutex<Option<(McpServer, Arc<Upstream>)>>>;

#[derive(Default)]
struct Status {
    /// Why a server was left out of the last listing.
    skipped: HashMap<String, String>,
}

pub struct Gateway {
    slots: std::sync::Mutex<HashMap<String, Slot>>,
    status: std::sync::Mutex<Status>,
    /// Bounds the calls in flight.
    calls: Arc<Semaphore>,
}

impl Default for Gateway {
    fn default() -> Self {
        Gateway {
            slots: Default::default(),
            status: Default::default(),
            calls: Arc::new(Semaphore::new(32)),
        }
    }
}

fn tool_error(text: impl Into<String>) -> Value {
    json!({ "content": [{ "type": "text", "text": text.into() }], "isError": true })
}

fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// The gateway's URI for a server's resource.
pub fn resource_uri(server: &str, uri: &str) -> String {
    format!("{RESOURCE_SCHEME}{server}/{}", encode(uri))
}

/// The server and its own URI for a gateway resource URI.
pub fn split_resource(uri: &str) -> Option<(String, String)> {
    let rest = uri.strip_prefix(RESOURCE_SCHEME)?;
    let (server, enc) = rest.split_once('/')?;
    Some((server.to_string(), decode(enc)?))
}

fn namespaced(server: &str, name: &str) -> String {
    format!("{server}{SEP}{name}")
}

fn split_name(name: &str) -> Option<(&str, &str)> {
    name.split_once(SEP)
        .filter(|(s, n)| !s.is_empty() && !n.is_empty())
}

fn cut(v: &Value) -> String {
    let s = v.to_string();
    if s.len() <= 1024 {
        return s;
    }
    let mut end = 1024;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

impl Gateway {
    /// Drops the connections, which ends servers the daemon started.
    pub fn shutdown(&self) {
        self.slots.lock().unwrap().clear();
    }

    /// Answers one JSON-RPC message from the guest; `None` for those that
    /// take no answer.
    pub async fn handle<B: Backend>(&self, b: &B, msg: Value) -> Option<Value> {
        let Some(obj) = msg.as_object() else {
            return Some(error_response(
                Value::Null,
                RpcError::new(-32600, "invalid request"),
            ));
        };
        let method = obj.get("method").and_then(Value::as_str)?;
        // Without an id it is a notification.
        let id = obj.get("id")?.clone();
        let params = obj.get("params").cloned().unwrap_or(Value::Null);
        let r = match method {
            "initialize" => Ok(self.initialize(b, &params)),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(self.list(b, "tools", "tools").await),
            "tools/call" => self.call_tool(b, params).await,
            "resources/list" => Ok(self.list(b, "resources", "resources").await),
            "resources/templates/list" => Ok(json!({ "resourceTemplates": [] })),
            "resources/read" => self.read_resource(b, params).await,
            "prompts/list" => Ok(self.list(b, "prompts", "prompts").await),
            "prompts/get" => self.get_prompt(b, params).await,
            "logging/setLevel" => Ok(json!({})),
            _ => Err(RpcError::new(-32601, format!("method not found: {method}"))),
        };
        Some(match r {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err(e) => error_response(id, e),
        })
    }

    fn initialize<B: Backend>(&self, b: &B, params: &Value) -> Value {
        let asked = params["protocolVersion"].as_str().unwrap_or("");
        let version = SUPPORTED.iter().find(|v| **v == asked).unwrap_or(&PROTOCOL);
        let mut about = String::from(
            "zapper's MCP gateway. Tools are named server__tool; each call is checked against the session's rules before it reaches the server.",
        );
        for name in b.attached() {
            match b.server(&name).and_then(|s| s.description) {
                Some(d) => about.push_str(&format!("\n- {name}: {d}")),
                None => about.push_str(&format!("\n- {name}")),
            }
        }
        json!({
            "protocolVersion": version,
            "capabilities": {
                "tools": { "listChanged": false },
                "resources": { "listChanged": false },
                "prompts": { "listChanged": false },
            },
            "serverInfo": { "name": "zapper-gateway", "version": env!("CARGO_PKG_VERSION") },
            "instructions": about,
        })
    }

    /// Fails for a server this session doesn't have, before anything is
    /// decided or recorded about it.
    fn attached<B: Backend>(&self, b: &B, server: &str) -> Result<(), RpcError> {
        if b.attached().iter().any(|a| a == server) {
            Ok(())
        } else {
            Err(RpcError::new(
                -32602,
                format!("no MCP server {server} in this session"),
            ))
        }
    }

    /// The connection to a server, made on first use and again if its
    /// program has exited or its entry in the config changed.
    async fn upstream<B: Backend>(&self, b: &B, name: &str) -> Result<Arc<Upstream>, RpcError> {
        self.attached(b, name)?;
        let def = b
            .server(name)
            .ok_or_else(|| RpcError::internal(format!("{name}: not in the config any more")))?;
        let slot = self
            .slots
            .lock()
            .unwrap()
            .entry(name.to_string())
            .or_default()
            .clone();
        let mut slot = slot.lock().await;
        if let Some((have, up)) = slot.as_ref() {
            if *have == def && up.alive() {
                return Ok(up.clone());
            }
        }
        *slot = None;
        let secret = |n: &str| b.secret(n);
        let up = Arc::new(
            Upstream::connect(&def, &secret)
                .await
                .map_err(|e| RpcError::internal(format!("{name}: {e}")))?,
        );
        *slot = Some((def, up.clone()));
        Ok(up)
    }

    /// Lists one kind of thing from every server, namespaced; servers that
    /// fail are left out and noted for `zapper-status`.
    async fn list<B: Backend>(&self, b: &B, method: &str, key: &str) -> Value {
        let attached = b.attached();
        let kind = match key {
            "tools" => McpKind::Tool,
            "resources" => McpKind::Resource,
            _ => McpKind::Prompt,
        };
        let fetched = futures_util::future::join_all(
            attached
                .iter()
                .map(|name| async move { (name.clone(), self.fetch(b, name, method, key).await) }),
        )
        .await;
        let mut out = Vec::new();
        for (server, r) in fetched {
            let items = match r {
                Ok(items) => {
                    self.status.lock().unwrap().skipped.remove(&server);
                    items
                }
                Err(e) => {
                    tracing::warn!(server, "mcp {method}: {e}");
                    self.status
                        .lock()
                        .unwrap()
                        .skipped
                        .insert(server, e.message);
                    continue;
                }
            };
            for mut item in items {
                let own = match kind {
                    McpKind::Resource => item["uri"].as_str(),
                    _ => item["name"].as_str(),
                }
                .unwrap_or("")
                .to_string();
                // Hidden: a request for it would be denied anyway.
                let req = McpRequest {
                    server: server.clone(),
                    kind,
                    name: own.clone(),
                };
                if b.decide(&req).action == Action::Deny {
                    continue;
                }
                match kind {
                    McpKind::Resource => item["uri"] = json!(resource_uri(&server, &own)),
                    _ => item["name"] = json!(namespaced(&server, &own)),
                }
                out.push(item);
            }
        }
        if kind == McpKind::Tool {
            out.push(json!({
                "name": STATUS_TOOL,
                "description": "The MCP servers behind this gateway: which are connected, and why one is missing.",
                "inputSchema": { "type": "object", "properties": {} },
                "annotations": { "readOnlyHint": true },
            }));
        }
        json!({ key: out })
    }

    async fn fetch<B: Backend>(
        &self,
        b: &B,
        server: &str,
        method: &str,
        key: &str,
    ) -> Result<Vec<Value>, RpcError> {
        let up = self.upstream(b, server).await?;
        let mut items = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let params = cursor
                .as_ref()
                .map_or(json!({}), |c| json!({ "cursor": c }));
            let page = match tokio::time::timeout(
                LIST_TIMEOUT,
                up.request(&format!("{method}/list"), params),
            )
            .await
            {
                Ok(Ok(p)) => p,
                // A server without prompts or resources is not a failure.
                Ok(Err(e)) if e.code == -32601 => return Ok(vec![]),
                Ok(Err(e)) => return Err(e),
                Err(_) => return Err(RpcError::internal("timed out")),
            };
            if let Some(a) = page[key].as_array() {
                items.extend(a.iter().cloned());
            }
            match page["nextCursor"].as_str() {
                Some(c) if !c.is_empty() => cursor = Some(c.to_string()),
                _ => break,
            }
        }
        up.take_stale();
        Ok(items)
    }

    /// Decides a request: `Ok` when it may go ahead, the reason when not.
    async fn authorize<B: Backend>(
        &self,
        b: &B,
        req: &McpRequest,
        args: &Value,
    ) -> Result<(), String> {
        let started = Instant::now();
        let decision = b.decide(req);
        let allowed = match decision.action {
            Action::Allow => true,
            Action::Deny => false,
            Action::Ask => b.ask(req, args).await,
        };
        let why = match (allowed, decision.action) {
            (true, _) => None,
            (false, Action::Ask) => Some("not approved".to_string()),
            (false, _) => Some(format!(
                "denied by rule {}",
                decision.rule.as_deref().unwrap_or("default")
            )),
        };
        b.record(Call {
            server: req.server.clone(),
            kind: req.kind,
            name: req.name.clone(),
            allowed,
            rule: decision.rule,
            scope: decision.scope.map(|s| s.to_string()),
            wait_ms: started.elapsed().as_secs_f64() * 1000.0,
            error: None,
            args: (!args.is_null()).then(|| cut(args)),
        });
        match why {
            None => Ok(()),
            Some(w) => Err(w),
        }
    }

    async fn call_tool<B: Backend>(&self, b: &B, params: Value) -> Result<Value, RpcError> {
        let name = params["name"]
            .as_str()
            .ok_or_else(|| RpcError::new(-32602, "tools/call needs a name"))?;
        if name == STATUS_TOOL {
            return Ok(self.status(b));
        }
        let (server, tool) = split_name(name)
            .ok_or_else(|| RpcError::new(-32602, format!("unknown tool {name}")))?;
        self.attached(b, server)?;
        let args = params.get("arguments").cloned().unwrap_or(Value::Null);
        let req = McpRequest {
            server: server.into(),
            kind: McpKind::Tool,
            name: tool.into(),
        };
        if let Err(why) = self.authorize(b, &req, &args).await {
            return Ok(tool_error(format!("{name}: {why}")));
        }
        let _permit = self
            .calls
            .acquire()
            .await
            .map_err(|_| RpcError::internal("shutting down"))?;
        let up = match self.upstream(b, server).await {
            Ok(u) => u,
            Err(e) if e.code == -32602 => return Err(e),
            Err(e) => return Ok(tool_error(e.message)),
        };
        let mut forward = json!({ "name": tool, "arguments": args });
        if let Some(meta) = params.get("_meta") {
            forward["_meta"] = meta.clone();
        }
        match tokio::time::timeout(CALL_TIMEOUT, up.request("tools/call", forward)).await {
            Ok(Ok(r)) => Ok(r),
            // The server's own protocol errors reach the client as they are.
            Ok(Err(e)) if e.code != -32603 => Err(e),
            Ok(Err(e)) => Ok(tool_error(format!("{server}: {}", e.message))),
            Err(_) => Ok(tool_error(format!("{server}: the call timed out"))),
        }
    }

    async fn read_resource<B: Backend>(&self, b: &B, params: Value) -> Result<Value, RpcError> {
        let uri = params["uri"]
            .as_str()
            .ok_or_else(|| RpcError::new(-32602, "resources/read needs a uri"))?;
        let (server, own) = split_resource(uri)
            .ok_or_else(|| RpcError::new(-32002, format!("unknown resource {uri}")))?;
        self.attached(b, &server)?;
        let req = McpRequest {
            server: server.clone(),
            kind: McpKind::Resource,
            name: own.clone(),
        };
        self.authorize(b, &req, &Value::Null)
            .await
            .map_err(|why| RpcError::new(-32002, format!("{uri}: {why}")))?;
        let up = self.upstream(b, &server).await?;
        let mut r = tokio::time::timeout(
            CALL_TIMEOUT,
            up.request("resources/read", json!({ "uri": own })),
        )
        .await
        .map_err(|_| RpcError::internal("the read timed out"))??;
        if let Some(contents) = r["contents"].as_array_mut() {
            for c in contents {
                if let Some(u) = c["uri"].as_str() {
                    c["uri"] = json!(resource_uri(&server, u));
                }
            }
        }
        Ok(r)
    }

    async fn get_prompt<B: Backend>(&self, b: &B, params: Value) -> Result<Value, RpcError> {
        let name = params["name"]
            .as_str()
            .ok_or_else(|| RpcError::new(-32602, "prompts/get needs a name"))?;
        let (server, prompt) = split_name(name)
            .ok_or_else(|| RpcError::new(-32602, format!("unknown prompt {name}")))?;
        self.attached(b, server)?;
        let args = params.get("arguments").cloned().unwrap_or(Value::Null);
        let req = McpRequest {
            server: server.into(),
            kind: McpKind::Prompt,
            name: prompt.into(),
        };
        self.authorize(b, &req, &args)
            .await
            .map_err(|why| RpcError::new(-32602, format!("{name}: {why}")))?;
        let up = self.upstream(b, server).await?;
        let mut forward = json!({ "name": prompt });
        if !args.is_null() {
            forward["arguments"] = args;
        }
        tokio::time::timeout(CALL_TIMEOUT, up.request("prompts/get", forward))
            .await
            .map_err(|_| RpcError::internal("the prompt timed out"))?
    }

    fn status<B: Backend>(&self, b: &B) -> Value {
        let status = self.status.lock().unwrap();
        let slots = self.slots.lock().unwrap();
        let lines: Vec<String> = b
            .attached()
            .iter()
            .map(|name| {
                let connected = slots
                    .get(name)
                    .and_then(|s| s.try_lock().ok().map(|g| g.is_some()))
                    .unwrap_or(false);
                match (status.skipped.get(name), connected) {
                    (Some(why), _) => format!("{name}: unavailable ({why})"),
                    (None, true) => format!("{name}: connected"),
                    (None, false) => format!("{name}: not used yet"),
                }
            })
            .collect();
        json!({ "content": [{ "type": "text", "text": lines.join("\n") }] })
    }
}

fn error_response(id: Value, e: RpcError) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": e.to_json() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_uris() {
        assert_eq!(split_name("gh__create_issue"), Some(("gh", "create_issue")));
        // Only the first separator counts: tools may contain it.
        assert_eq!(split_name("gh__a__b"), Some(("gh", "a__b")));
        assert_eq!(split_name("nosep"), None);
        assert_eq!(split_name("__x"), None);
        let u = resource_uri("fs", "file:///srv/a b/ü.txt?x=1");
        assert!(u.starts_with("zapper-mcp://fs/"), "{u}");
        assert!(!u[16..].contains(['/', ' ', '?']), "{u}");
        assert_eq!(
            split_resource(&u),
            Some(("fs".into(), "file:///srv/a b/ü.txt?x=1".into()))
        );
        assert_eq!(split_resource("file:///x"), None);
        assert_eq!(split_resource("zapper-mcp://fs/%zz"), None);
    }

    #[test]
    fn arguments_are_cut_on_a_char_boundary() {
        let v = json!({ "a": "é".repeat(2000) });
        let c = cut(&v);
        assert!(c.ends_with('…') && c.len() < 1100);
    }
}
