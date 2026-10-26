//! The MCP gateway: servers registered in the daemon (the office's tools),
//! served to the sessions their labels pick over the guest's MCP vsock port.
//! A connection names a server on its first line, then carries MCP as on
//! stdio, one JSON-RPC message per line each way, so the agent's bridge
//! only copies lines. Who calls is the session the connection's VM is;
//! nothing in a message can say otherwise. Each tool call is an `mcp` event
//! of the session's.

use std::sync::Arc;
use std::time::Instant;

use anyhow::{anyhow, bail, Result};
use futures_util::future::BoxFuture;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::Semaphore;
use zapper_policy::proto::MAX_MCP_LINE;

use super::registry::{Entry, Registry, Selector};
use super::session::Session;
use super::EventBody;

/// An MCP server in the daemon: its tools, for each session that calls.
pub trait McpServer: Send + Sync {
    /// What the server tells the model about itself.
    fn instructions(&self) -> Option<String> {
        None
    }
    /// Its tools for `s`, as MCP tool definitions.
    fn tools<'a>(&'a self, s: &'a Arc<Session>) -> BoxFuture<'a, Result<Value>>;
    /// Calls `tool` for `s`. Its result goes to the model as JSON text; an
    /// error as the text of an error result.
    fn call<'a>(
        &'a self,
        s: &'a Arc<Session>,
        tool: &'a str,
        args: Value,
    ) -> BoxFuture<'a, Result<Value>>;
}

/// A server by its name.
#[derive(Clone)]
pub struct Named {
    name: String,
    server: Arc<dyn McpServer>,
}

#[derive(Default)]
pub struct Gateway(Registry<Named>);

/// Connections a session may hold open to the gateway at once.
const MAX_CONNECTIONS: usize = 8;

impl Gateway {
    pub fn add(&self, name: &str, sessions: Selector, server: Arc<dyn McpServer>) -> Result<u64> {
        if !zapper_policy::valid_worker_name(name) {
            bail!("{name:?} is not an MCP server name");
        }
        let named = Named {
            name: name.to_string(),
            server,
        };
        self.0
            .add_unless(sessions.clone(), named, |e| {
                e.item.name == name && e.sessions == sessions
            })
            .ok_or_else(|| anyhow!("an MCP server {name} is already registered for these sessions"))
    }

    pub fn remove(&self, id: u64) -> Option<Entry<Named>> {
        self.0.remove(id)
    }

    /// The server `name` for `s`: the earliest registered that picks it.
    pub fn server(&self, s: &Session, name: &str) -> Option<Arc<dyn McpServer>> {
        self.0.first(s, |n| n.name == name).map(|n| n.server)
    }
}

/// Accepts the session's connections to the gateway.
pub async fn serve(d: Arc<super::Daemon>, s: Arc<Session>, listener: UnixListener) {
    let limit = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            break;
        };
        let Ok(permit) = limit.clone().try_acquire_owned() else {
            continue;
        };
        let (d, s) = (d.clone(), s.clone());
        tokio::spawn(async move {
            let _ = connection(&d, &s, stream).await;
            drop(permit);
        });
    }
}

/// Reads one line, at most `MAX_MCP_LINE` bytes: `None` at the end, `Err`
/// for a longer one.
pub(crate) async fn line<R: AsyncRead + Unpin>(
    r: &mut BufReader<R>,
) -> std::io::Result<Option<Vec<u8>>> {
    let mut buf = Vec::new();
    let n = r
        .take(MAX_MCP_LINE as u64 + 1)
        .read_until(b'\n', &mut buf)
        .await?;
    if n == 0 {
        return Ok(None);
    }
    if buf.len() > MAX_MCP_LINE && !buf.ends_with(b"\n") {
        return Err(std::io::Error::other(format!(
            "request longer than {MAX_MCP_LINE} bytes"
        )));
    }
    Ok(Some(buf))
}

fn error(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message.into() } })
}

/// One connection: the server's name, then MCP until the guest hangs up.
pub async fn connection<S>(d: &super::Daemon, s: &Arc<Session>, stream: S) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (r, mut w) = tokio::io::split(stream);
    let mut r = BufReader::new(r);
    let Some(first) = line(&mut r).await? else {
        return Ok(());
    };
    let name = String::from_utf8_lossy(&first).trim().to_string();
    loop {
        let msg = match line(&mut r).await {
            Ok(Some(l)) => l,
            Ok(None) => return Ok(()),
            Err(e) => {
                // The rest of the stream can't be read as lines: say why
                // and hang up.
                write(&mut w, &error(Value::Null, -32600, e.to_string())).await?;
                return Ok(());
            }
        };
        if msg.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        // Resolved per message, not once: a server removed mid-connection
        // stops serving at once, rather than lingering until the guest
        // reconnects.
        let server = d.gateway.server(s, &name);
        let reply = match serde_json::from_slice::<Value>(&msg) {
            Err(e) => Some(error(Value::Null, -32700, format!("bad request: {e}"))),
            Ok(m) => {
                let call = (m["method"] == "tools/call").then(|| {
                    (
                        clip(m["params"]["name"].as_str().unwrap_or("")),
                        Instant::now(),
                    )
                });
                let reply = match &server {
                    Some(server) => answer(server.as_ref(), &name, s, m).await,
                    None => m.get("id").map(|id| {
                        error(
                            id.clone(),
                            -32601,
                            format!("no MCP server {name} for this session"),
                        )
                    }),
                };
                if let Some((tool, start)) = call {
                    d.emit(Some(s), called(&name, tool, start, reply.as_ref()));
                }
                reply
            }
        };
        if let Some(reply) = reply {
            write(&mut w, &reply).await?;
        }
    }
}

/// A guest's name for a server or tool, as far as an event keeps it.
fn clip(s: &str) -> String {
    s.chars().take(MAX_NAME).collect()
}

const MAX_NAME: usize = 128;

/// The event of a tool call: what the reply said of it, or that it had none
/// (a notification).
fn called(server: &str, tool: String, start: Instant, reply: Option<&Value>) -> EventBody {
    let error = match reply {
        None => Some("no id, no answer".to_string()),
        Some(r) if r["result"]["isError"] == false => None,
        Some(r) => Some(
            r["error"]["message"]
                .as_str()
                .or(r["result"]["content"][0]["text"].as_str())
                .unwrap_or("error")
                .chars()
                .take(MAX_ERROR)
                .collect(),
        ),
    };
    EventBody::Mcp {
        server: clip(server),
        tool,
        ok: error.is_none(),
        ms: start.elapsed().as_secs_f64() * 1000.0,
        error,
    }
}

const MAX_ERROR: usize = 512;

async fn write<W: AsyncWrite + Unpin>(w: &mut W, v: &Value) -> Result<()> {
    let mut b = serde_json::to_vec(v)?;
    b.push(b'\n');
    w.write_all(&b).await?;
    w.flush().await?;
    Ok(())
}

/// The answer to one message; none for a notification.
async fn answer(server: &dyn McpServer, name: &str, s: &Arc<Session>, msg: Value) -> Option<Value> {
    let id = msg.get("id")?.clone();
    let method = msg["method"].as_str().unwrap_or("");
    let result = match method {
        "initialize" => {
            let mut r = json!({
                "protocolVersion": msg["params"]["protocolVersion"].as_str().unwrap_or("2025-06-18"),
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": { "name": name, "version": env!("CARGO_PKG_VERSION") },
            });
            if let Some(i) = server.instructions() {
                r["instructions"] = json!(i);
            }
            Ok(r)
        }
        "ping" => Ok(json!({})),
        "tools/list" => match server.tools(s).await {
            Ok(t) => Ok(json!({ "tools": t })),
            Err(e) => Err((-32603, format!("{e:#}"))),
        },
        "tools/call" => {
            let tool = msg["params"]["name"].as_str().unwrap_or("");
            let args = msg["params"].get("arguments").cloned().unwrap_or(json!({}));
            Ok(match server.call(s, tool, args).await {
                Ok(v) => {
                    json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&v).unwrap_or_default() }], "isError": false })
                }
                Err(e) => {
                    json!({ "content": [{ "type": "text", "text": format!("{e:#}") }], "isError": true })
                }
            })
        }
        _ => Err((-32601, format!("method {method} not found"))),
    };
    Some(match result {
        Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
        Err((code, message)) => error(id, code, message),
    })
}

#[cfg(test)]
mod tests;
