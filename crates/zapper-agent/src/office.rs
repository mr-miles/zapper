//! `office`: a firm worker's way to the office's tools, the MCP server
//! `office` in the daemon's gateway, over the VM's MCP port. As `office mcp`
//! it bridges the harness's stdio to it line by line; otherwise it is a
//! command line for scripts and people, and Claude Code's stop hook.
//!
//! The host knows which worker is asking from the VM the connection comes
//! from, and checks the role on every call; this side only carries lines.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use zapper_proto::proto::{HOST_CID, MAX_MCP_LINE, MCP_PORT};

use crate::sys;

/// The office's server in the gateway.
const SERVER: &str = "office";

/// A connection to the office's tools: the server named, ready for MCP.
fn connect() -> Result<File> {
    let mut conn = sys::vsock_connect(HOST_CID, MCP_PORT)
        .context("connecting to the daemon's MCP gateway (is this a firm worker?)")?;
    conn.write_all(format!("{SERVER}\n").as_bytes())?;
    Ok(conn)
}

/// The office edge's MCP endpoint, where this worker reaches the office over
/// HTTP (a sandbox on OpenShell): its supervisor adds the worker's token on
/// the way, so nothing here carries a credential.
fn edge_url() -> Option<String> {
    std::env::var("ZAPPER_OFFICE_URL")
        .ok()
        .filter(|u| !u.trim().is_empty())
}

/// Calls one of the office's tools and returns its answer.
pub fn call(op: &str, args: &Value) -> Result<Value> {
    let request = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": op, "arguments": args } });
    if let Some(url) = edge_url() {
        let reply = http::post(&url, &serde_json::to_vec(&request)?)?;
        return answer(&reply);
    }
    let mut conn = connect()?;
    let mut line = serde_json::to_vec(&request)?;
    line.push(b'\n');
    if line.len() > MAX_MCP_LINE {
        bail!("request longer than {MAX_MCP_LINE} bytes");
    }
    conn.write_all(&line)?;
    let mut reply = Vec::new();
    BufReader::new(conn)
        .take(16 << 20)
        .read_until(b'\n', &mut reply)?;
    answer(&reply)
}

/// A tool call's answer: the tool's result, or why it failed.
fn answer(reply: &[u8]) -> Result<Value> {
    let v: Value = serde_json::from_slice(reply).context("reading the office's answer")?;
    if let Some(e) = v.get("error") {
        bail!("{}", e["message"].as_str().unwrap_or("error"));
    }
    let r = &v["result"];
    let text = r["content"][0]["text"].as_str().unwrap_or("");
    if r["isError"] == json!(true) {
        bail!("{text}");
    }
    Ok(serde_json::from_str(text).unwrap_or_else(|_| json!(text)))
}

const USAGE: &str = "usage: office tools | office OP [JSON] | office OP --key value ...
       office mcp     (the MCP server Claude Code runs)
       office stop-hook  (Claude Code's Stop hook: mail that arrived during the turn)
examples:
  office desk
  office mail_send --to cto --subject 'Plan' --body 'Draft attached' --kind fyi
  office mail_reply '{\"message\": \"m-3\", \"body\": \"done\", \"kind\": \"report\", \"close\": true}'";

/// `--key value` pairs into a JSON object; values that parse as JSON are
/// taken as such, comma lists for `to`, `cc`, `paths`, `attachments`.
fn flags(args: &[String]) -> Result<Value> {
    let mut obj = serde_json::Map::new();
    let mut i = 0;
    while i < args.len() {
        let Some(k) = args[i].strip_prefix("--") else {
            bail!("unexpected {:?}\n{USAGE}", args[i])
        };
        let v = args.get(i + 1).cloned().unwrap_or_else(|| "true".into());
        let value = if ["to", "cc", "paths", "attachments", "methods"].contains(&k) {
            json!(v
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>())
        } else {
            serde_json::from_str::<Value>(&v)
                .ok()
                .filter(|x| !x.is_string())
                .unwrap_or(json!(v))
        };
        obj.insert(k.to_string(), value);
        i += 2;
    }
    Ok(Value::Object(obj))
}

pub fn cli(args: &[String]) -> i32 {
    let result = (|| -> Result<Value> {
        match args.first().map(String::as_str) {
            None | Some("-h") | Some("--help") => {
                println!("{USAGE}");
                Ok(Value::Null)
            }
            Some("mcp") => {
                mcp()?;
                Ok(Value::Null)
            }
            Some("stop-hook") => {
                stop_hook();
                Ok(Value::Null)
            }
            Some(op) => {
                let rest = &args[1..];
                let a = match rest.first() {
                    Some(j) if j.trim_start().starts_with('{') => {
                        serde_json::from_str(j).context("JSON arguments")?
                    }
                    _ => flags(rest)?,
                };
                call(op, &a)
            }
        }
    })();
    match result {
        Ok(Value::Null) => 0,
        Ok(v) => {
            println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
            0
        }
        Err(e) => {
            eprintln!("office: {e:#}");
            1
        }
    }
}

/// Claude Code's Stop hook: if mail arrived during the turn, block the stop
/// and hand it over as the reason. Any failure lets Claude stop.
fn stop_hook() {
    let mut input = String::new();
    let _ = std::io::stdin().take(1 << 20).read_to_string(&mut input);
    if let Ok(v) = call("stop_check", &json!({})) {
        if v["block"] == json!(true) {
            println!("{}", json!({ "decision": "block", "reason": v["reason"] }));
        }
    }
}

/// The MCP server the harness runs: stdin and stdout bridged to the office's
/// server in the gateway, line by line; MCP's stdio framing is the
/// gateway's too.
fn mcp() -> Result<()> {
    if let Some(url) = edge_url() {
        return mcp_over_http(&url);
    }
    let conn = connect()?;
    let mut up = conn.try_clone()?;
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut std::io::stdin().lock(), &mut up);
        // The harness hung up: so does the bridge.
        std::process::exit(0);
    });
    let mut down = conn;
    let mut out = std::io::stdout().lock();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = down.read(&mut buf)?;
        if n == 0 {
            return Ok(());
        }
        out.write_all(&buf[..n])?;
        out.flush()?;
    }
}

/// The stdio bridge to the office edge, for a harness that runs MCP servers
/// as commands: each line from the harness is POSTed to the edge (MCP's
/// Streamable HTTP), each answer written back as a line. Messages are
/// answered concurrently, as a tool call may take a while.
fn mcp_over_http(url: &str) -> Result<()> {
    let out = std::sync::Arc::new(std::sync::Mutex::new(std::io::stdout()));
    let stdin = std::io::stdin();
    let mut lines = BufReader::new(stdin.lock());
    loop {
        let mut line = Vec::new();
        let n = lines
            .by_ref()
            .take(MAX_MCP_LINE as u64 + 1)
            .read_until(b'\n', &mut line)?;
        if n == 0 {
            // The harness hung up: so does the bridge.
            return Ok(());
        }
        if line.len() > MAX_MCP_LINE {
            bail!("a message longer than {MAX_MCP_LINE} bytes from the harness");
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let (url, out) = (url.to_string(), out.clone());
        std::thread::spawn(move || {
            let reply = match http::post(&url, &line) {
                Ok(r) => r,
                Err(e) => {
                    // A request is told why; a notification has no answer.
                    match serde_json::from_slice::<Value>(&line) {
                        Ok(m) if m.get("id").is_some() => serde_json::to_vec(&json!({
                            "jsonrpc": "2.0", "id": m["id"],
                            "error": { "code": -32603, "message": format!("reaching the office: {e:#}") },
                        }))
                        .unwrap_or_default(),
                        _ => return,
                    }
                }
            };
            if reply.iter().all(u8::is_ascii_whitespace) {
                return;
            }
            let mut out = out.lock().unwrap();
            let _ = out.write_all(&reply);
            let _ = out.write_all(b"\n");
            let _ = out.flush();
        });
    }
}

/// Just enough HTTP for the office edge: one `POST` over plain HTTP/1.0 (so
/// the answer is never chunked: it ends when the connection does), on
/// `http://host[:port]/path`.
mod http {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::time::Duration;

    use anyhow::{anyhow, bail, Context, Result};

    /// The most an answer may be.
    const MAX_ANSWER: u64 = 16 << 20;

    /// `http://host[:port]/path` as (host, port, path).
    pub(super) fn parse_url(url: &str) -> Result<(String, u16, String)> {
        let rest = url
            .strip_prefix("http://")
            .ok_or_else(|| anyhow!("{url}: the office edge is an http:// URL"))?;
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) => (h, p.parse().with_context(|| format!("{url}: port"))?),
            None => (authority, 80),
        };
        if host.is_empty() {
            bail!("{url}: no host");
        }
        Ok((host.to_string(), port, path.to_string()))
    }

    /// POSTs `body` (JSON) to `url`: the answer's body (empty for 202).
    pub(super) fn post(url: &str, body: &[u8]) -> Result<Vec<u8>> {
        let (host, port, path) = parse_url(url)?;
        let mut conn = TcpStream::connect((host.as_str(), port))
            .with_context(|| format!("connecting to the office at {host}:{port}"))?;
        // A tool call may take up to two minutes on the office's side.
        conn.set_read_timeout(Some(Duration::from_secs(150)))?;
        conn.set_write_timeout(Some(Duration::from_secs(30)))?;
        let head = format!(
            "POST {path} HTTP/1.0\r\nHost: {host}:{port}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        conn.write_all(head.as_bytes())?;
        conn.write_all(body)?;
        let mut raw = Vec::new();
        conn.take(MAX_ANSWER).read_to_end(&mut raw)?;
        response_body(&raw)
    }

    /// An HTTP answer's body, if its status is 2xx; why not, if not.
    pub(super) fn response_body(raw: &[u8]) -> Result<Vec<u8>> {
        let split = raw
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .ok_or_else(|| anyhow!("the office's answer has no end of headers"))?;
        let head = String::from_utf8_lossy(&raw[..split]);
        let body = &raw[split + 4..];
        let status: u16 = head
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| anyhow!("the office's answer has no status"))?;
        match status {
            202 | 204 => Ok(Vec::new()),
            200..=299 => Ok(body.to_vec()),
            _ => bail!(
                "the office answered {status}: {}",
                String::from_utf8_lossy(body).trim()
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    #[test]
    fn the_edge_is_an_http_url() {
        assert_eq!(
            super::http::parse_url("http://host.openshell.internal:8970/mcp").unwrap(),
            ("host.openshell.internal".into(), 8970, "/mcp".into())
        );
        assert_eq!(
            super::http::parse_url("http://office").unwrap(),
            ("office".into(), 80, "/".into())
        );
        assert!(super::http::parse_url("https://office/mcp").is_err());
        assert!(super::http::parse_url("http://:80/mcp").is_err());
    }

    #[test]
    fn an_answer_is_its_body_when_it_succeeds() {
        let ok = b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\r\n{\"a\":1}";
        assert_eq!(super::http::response_body(ok).unwrap(), b"{\"a\":1}");
        let accepted = b"HTTP/1.1 202 Accepted\r\ncontent-length: 0\r\n\r\n";
        assert!(super::http::response_body(accepted).unwrap().is_empty());
        let refused = b"HTTP/1.1 401 Unauthorized\r\n\r\nno token for a worker of the office\n";
        assert_eq!(
            super::http::response_body(refused).unwrap_err().to_string(),
            "the office answered 401: no token for a worker of the office"
        );
        assert!(super::http::response_body(b"garbage").is_err());
    }

    fn reply(v: serde_json::Value) -> Vec<u8> {
        serde_json::to_vec(&v).unwrap()
    }

    #[test]
    fn a_calls_answer_is_its_result_or_why_not() {
        let ok = reply(
            json!({ "jsonrpc": "2.0", "id": 1, "result": { "content": [{ "type": "text", "text": "{\"unread\": 2}" }], "isError": false } }),
        );
        assert_eq!(super::answer(&ok).unwrap(), json!({ "unread": 2 }));
        let text = reply(
            json!({ "jsonrpc": "2.0", "id": 1, "result": { "content": [{ "type": "text", "text": "plain" }], "isError": false } }),
        );
        assert_eq!(super::answer(&text).unwrap(), json!("plain"));
        let failed = reply(
            json!({ "jsonrpc": "2.0", "id": 1, "result": { "content": [{ "type": "text", "text": "the firm isn't running" }], "isError": true } }),
        );
        assert_eq!(
            super::answer(&failed).unwrap_err().to_string(),
            "the firm isn't running"
        );
        let refused = reply(
            json!({ "jsonrpc": "2.0", "id": 1, "error": { "code": -32601, "message": "no MCP server office for this session" } }),
        );
        assert_eq!(
            super::answer(&refused).unwrap_err().to_string(),
            "no MCP server office for this session"
        );
        assert!(super::answer(b"").is_err());
    }
}
