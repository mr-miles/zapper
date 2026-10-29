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

/// Calls one of the office's tools and returns its answer.
pub fn call(op: &str, args: &Value) -> Result<Value> {
    let mut conn = connect()?;
    let request = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": op, "arguments": args } });
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

#[cfg(test)]
mod tests {
    use serde_json::json;

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
