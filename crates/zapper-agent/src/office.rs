//! `office`: a firm worker's way to the office's tools, at the office edge
//! (`ZAPPER_OFFICE_URL`, its MCP endpoint over Streamable HTTP). As `office
//! mcp` it bridges a harness's stdio to it line by line; otherwise it is a
//! command line for scripts and people, and Claude Code's stop hook.
//!
//! The sandbox's supervisor adds the worker's token to each request on its
//! way out, so nothing here carries a credential; the office checks who is
//! asking, and their role, on every call.

use std::io::{BufRead, BufReader, Read, Write};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

/// The longest MCP message a harness may send, as the office takes them.
const MAX_MCP_LINE: usize = 1 << 20;

/// The office edge's MCP endpoint, where this worker reaches the office over
/// HTTP (a sandbox on OpenShell): its supervisor adds the worker's token on
/// the way, so nothing here carries a credential.
fn edge_url() -> Result<String> {
    std::env::var("ZAPPER_OFFICE_URL")
        .ok()
        .filter(|u| !u.trim().is_empty())
        .ok_or_else(|| anyhow!("ZAPPER_OFFICE_URL isn't set: is this a firm worker's sandbox?"))
}

/// Calls one of the office's tools and returns its answer.
pub fn call(op: &str, args: &Value) -> Result<Value> {
    let request = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": op, "arguments": args } });
    let body = serde_json::to_vec(&request)?;
    if body.len() > MAX_MCP_LINE {
        bail!("request longer than {MAX_MCP_LINE} bytes");
    }
    answer(&http::post(&edge_url()?, &body)?)
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
       office instructions  (yours, and who works here: Claude Code's SessionStart hook)
       office drive put|publish|get|ls ...  (the drive at the edge; office drive for more)
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
            Some("instructions") => {
                instructions();
                Ok(Value::Null)
            }
            Some("drive") => drive(&args[1..]),
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

/// Claude Code's SessionStart hook (and a command): the worker's own
/// instructions and who else works here, from the office, printed for the
/// harness to take as context. Not on a resumed session, which has them;
/// any failure prints nothing.
fn instructions() {
    let mut input = String::new();
    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        let _ = std::io::stdin().take(1 << 20).read_to_string(&mut input);
    }
    let source = serde_json::from_str::<Value>(&input)
        .ok()
        .and_then(|v| v["source"].as_str().map(str::to_string));
    if source.as_deref() == Some("resume") {
        return;
    }
    if let Ok(v) = call("instructions", &json!({})) {
        if let Some(t) = v["text"].as_str() {
            println!("{t}");
        }
    }
}

/// The edge's base URL: `ZAPPER_OFFICE_URL` without its `/mcp`.
fn edge_base() -> Result<String> {
    let url = edge_url()?;
    Ok(url
        .trim_end_matches('/')
        .trim_end_matches("/mcp")
        .to_string())
}

/// A drive path as a URL path: each part percent-encoded.
fn url_path(path: &str) -> String {
    path.split('/')
        .filter(|p| !p.is_empty())
        .map(|p| {
            p.bytes()
                .map(|b| match b {
                    b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                        (b as char).to_string()
                    }
                    _ => format!("%{b:02X}"),
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// A local file's name, for a drive path not given.
fn file_name(local: &str) -> Result<String> {
    std::path::Path::new(local)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| anyhow!("{local}: not a file's path"))
}

const DRIVE_USAGE: &str = "usage: office drive put LOCAL [PATH]      (to your outbox)
       office drive publish LOCAL [PATH]  (to your outbox, then to the shared drive)
       office drive get PATH [LOCAL]      (from the shared drive)
       office drive ls [PATH]";

/// The drive at the office's edge: your outbox, and the shared drive.
fn drive(args: &[String]) -> Result<Value> {
    let arg = |i: usize| args.get(i).map(String::as_str);
    match (arg(0), arg(1)) {
        (Some("put"), Some(local)) => {
            let path = match arg(2) {
                Some(p) => p.to_string(),
                None => file_name(local)?,
            };
            put(local, &path)?;
            Ok(json!({ "outbox": path }))
        }
        (Some("publish"), Some(local)) => {
            let path = match arg(2) {
                Some(p) => p.to_string(),
                None => file_name(local)?,
            };
            put(local, &path)?;
            call("drive_publish", &json!({ "path": path }))
        }
        (Some("get"), Some(path)) => {
            let local = match arg(2) {
                Some(l) => l.to_string(),
                None => file_name(path)?,
            };
            let url = format!("{}/drive/inbox/{}", edge_base()?, url_path(path));
            let mut f =
                std::fs::File::create(&local).with_context(|| format!("writing {local}"))?;
            let n = http::get_to(&url, &mut f).inspect_err(|_| {
                let _ = std::fs::remove_file(&local);
            })?;
            Ok(json!({ "fetched": path, "to": local, "bytes": n }))
        }
        (Some("ls"), path) => call("drive_list", &json!({ "path": path.unwrap_or("") })),
        _ => bail!("{DRIVE_USAGE}"),
    }
}

/// Uploads `local` to `path` in this worker's outbox (`/drive/mine/` at the
/// edge: the office knows whose it is).
fn put(local: &str, path: &str) -> Result<()> {
    let mut f = std::fs::File::open(local).with_context(|| format!("reading {local}"))?;
    let len = f.metadata()?.len();
    let url = format!("{}/drive/mine/{}", edge_base()?, url_path(path));
    http::put(&url, &mut f, len).map(|_| ())
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

/// The MCP server a harness runs as a command: its stdio bridged to the
/// office edge.
fn mcp() -> Result<()> {
    mcp_over_http(&edge_url()?)
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

/// Just enough HTTP for the office edge: a `POST`, `PUT` or `GET` over plain
/// HTTP/1.0 (so the answer is never chunked: it ends when the connection
/// does), on `http://host[:port]/path`.
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

    /// PUTs `len` bytes of `body` to `url`: the answer's body.
    pub(super) fn put(url: &str, body: &mut dyn Read, len: u64) -> Result<Vec<u8>> {
        let (host, port, path) = parse_url(url)?;
        let mut conn = TcpStream::connect((host.as_str(), port))
            .with_context(|| format!("connecting to the office at {host}:{port}"))?;
        conn.set_read_timeout(Some(Duration::from_secs(150)))?;
        conn.set_write_timeout(Some(Duration::from_secs(60)))?;
        let head = format!(
            "PUT {path} HTTP/1.0\r\nHost: {host}:{port}\r\nContent-Type: application/octet-stream\r\nContent-Length: {len}\r\n\r\n"
        );
        conn.write_all(head.as_bytes())?;
        let sent = std::io::copy(&mut body.take(len), &mut conn)?;
        if sent != len {
            bail!("the file changed while it was sent ({sent} of {len} bytes)");
        }
        let mut raw = Vec::new();
        conn.take(MAX_ANSWER).read_to_end(&mut raw)?;
        response_body(&raw)
    }

    /// GETs `url` into `out`, as it comes: the bytes written.
    pub(super) fn get_to(url: &str, out: &mut dyn Write) -> Result<u64> {
        let (host, port, path) = parse_url(url)?;
        let conn = TcpStream::connect((host.as_str(), port))
            .with_context(|| format!("connecting to the office at {host}:{port}"))?;
        conn.set_read_timeout(Some(Duration::from_secs(150)))?;
        let mut w = &conn;
        w.write_all(format!("GET {path} HTTP/1.0\r\nHost: {host}:{port}\r\n\r\n").as_bytes())?;
        let mut r = std::io::BufReader::new(&conn);
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            let mut b = [0u8; 1];
            if r.read(&mut b)? == 0 || head.len() > 64 * 1024 {
                bail!("the office's answer has no end of headers");
            }
            head.push(b[0]);
        }
        let status: u16 = String::from_utf8_lossy(&head)
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| anyhow!("the office's answer has no status"))?;
        if !(200..300).contains(&status) {
            let mut why = String::new();
            let _ = r.take(64 * 1024).read_to_string(&mut why);
            bail!("the office answered {status}: {}", why.trim());
        }
        Ok(std::io::copy(&mut r, out)?)
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

    /// A one-request HTTP server on loopback: answers `reply`, and hands back
    /// what it was sent.
    fn serve_once(reply: &'static [u8]) -> (String, std::thread::JoinHandle<Vec<u8>>) {
        use std::io::{Read, Write};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        let h = std::thread::spawn(move || {
            let (mut c, _) = l.accept().unwrap();
            c.set_read_timeout(Some(std::time::Duration::from_millis(500)))
                .unwrap();
            let mut got = Vec::new();
            let mut buf = [0u8; 4096];
            while let Ok(n) = c.read(&mut buf) {
                if n == 0 {
                    break;
                }
                got.extend_from_slice(&buf[..n]);
            }
            c.write_all(reply).unwrap();
            got
        });
        (url, h)
    }

    #[test]
    fn a_put_sends_the_file_and_a_get_writes_the_body() {
        let (url, h) = serve_once(b"HTTP/1.1 201 Created\r\n\r\n{}");
        let mut body: &[u8] = b"hello";
        super::http::put(&format!("{url}/drive/mine/a.txt"), &mut body, 5).unwrap();
        let sent = String::from_utf8(h.join().unwrap()).unwrap();
        assert!(
            sent.starts_with("PUT /drive/mine/a.txt HTTP/1.0\r\n"),
            "{sent}"
        );
        assert!(sent.contains("Content-Length: 5\r\n"), "{sent}");
        assert!(sent.ends_with("\r\n\r\nhello"), "{sent}");

        let (url, h) =
            serve_once(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\r\nfrom echo\n");
        let mut out = Vec::new();
        let n = super::http::get_to(&format!("{url}/drive/inbox/echo/a.txt"), &mut out).unwrap();
        assert_eq!((n, out.as_slice()), (10, b"from echo\n".as_slice()));
        assert!(String::from_utf8(h.join().unwrap())
            .unwrap()
            .starts_with("GET /drive/inbox/echo/a.txt HTTP/1.0"));

        let (url, h) = serve_once(b"HTTP/1.1 404 Not Found\r\n\r\n{\"error\":\"no such file\"}");
        let e = super::http::get_to(&format!("{url}/x"), &mut Vec::new()).unwrap_err();
        assert!(e.to_string().contains("404"), "{e}");
        h.join().unwrap();
    }

    #[test]
    fn drive_paths_are_encoded_part_by_part() {
        assert_eq!(
            super::url_path("/notes/q3 report.md"),
            "notes/q3%20report.md"
        );
        assert_eq!(super::url_path("a//b/"), "a/b");
        assert_eq!(super::url_path("caf\u{e9}"), "caf%C3%A9");
        assert_eq!(super::file_name("/tmp/x.txt").unwrap(), "x.txt");
        assert!(super::file_name("/").is_err());
    }

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
