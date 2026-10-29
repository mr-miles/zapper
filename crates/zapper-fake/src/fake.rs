//! A fake agent for tests: speaks the same stream-json on stdin and stdout
//! as `claude -p --input-format stream-json --output-format stream-json`
//! (run as `claude`), or pi's `--mode rpc` (run as `pi`), and plays a script
//! instead of thinking. Tools are called through the office MCP server
//! named in `~/.claude.json` or `~/.pi/agent/mcp.json`, as the agent would.
//!
//! The script is YAML:
//!
//! ```yaml
//! rules:                      # the first rule that matches a wake runs
//!   - when: { from: board, kind: assignment }   # also: subject, contains, once
//!     do:
//!       - mail_send: { to: [ada], subject: Build it, body: "...", kind: assignment }
//!       - shell: "echo done > /work/out.md"
//!       - drive_publish: { paths: [/work/out.md], title: Out, kind: document, summary: s }
//!       - mail_reply: { message: $message, body: "Done: $artifact", kind: report, close: true }
//!       - say: some text
//!       - sleep: 1
//! ```
//!
//! `$message`, `$thread`, `$from`, `$subject` come from the wake's first
//! message, `$request` from its first access request, `$screening` from its
//! first background check, `$artifact` from the
//! last publish, `$last` is the last tool result's text.

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc;
use std::time::Instant;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

struct Mcp {
    _child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next: u64,
}

impl Mcp {
    fn start() -> Result<Mcp> {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
        // Where Claude Code or pi keeps its MCP servers, in the same shape.
        let state: Value = [".claude.json", ".pi/agent/mcp.json"]
            .iter()
            .find_map(|f| std::fs::read(format!("{home}/{f}")).ok())
            .map(|b| serde_json::from_slice(&b))
            .transpose()?
            .unwrap_or_default();
        let server = &state["mcpServers"]["office"];
        let cmd = server["command"].as_str().ok_or_else(|| {
            anyhow!("no office MCP server in ~/.claude.json or ~/.pi/agent/mcp.json")
        })?;
        let args: Vec<String> = server["args"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|a| a.as_str().map(str::to_string))
            .collect();
        let mut child = Command::new(cmd)
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .context("starting the office MCP server")?;
        let mut m = Mcp {
            stdin: child.stdin.take().unwrap(),
            stdout: BufReader::new(child.stdout.take().unwrap()),
            _child: child,
            next: 1,
        };
        m.request("initialize", json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "fake-claude", "version": "0" } }))?;
        writeln!(
            m.stdin,
            "{}",
            json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })
        )?;
        Ok(m)
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next;
        self.next += 1;
        writeln!(
            self.stdin,
            "{}",
            json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
        )?;
        self.stdin.flush()?;
        loop {
            let mut line = String::new();
            if self.stdout.read_line(&mut line)? == 0 {
                bail!("the MCP server exited");
            }
            let v: Value = serde_json::from_str(&line)?;
            if v["id"] == json!(id) {
                if let Some(e) = v.get("error") {
                    bail!("{e}");
                }
                return Ok(v["result"].clone());
            }
        }
    }
}

/// Fields of the wake prompt the script can use.
#[derive(Default, Debug)]
struct Wake {
    text: String,
    message: String,
    thread: String,
    from: String,
    kind: String,
    subject: String,
    request: String,
    screening: String,
    contract: String,
    credential: String,
}

fn after<'a>(text: &'a str, marker: &str) -> Option<&'a str> {
    text.find(marker).map(|i| &text[i + marker.len()..])
}

fn word(s: &str) -> String {
    s.chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect()
}

fn parse_wake(text: &str) -> Wake {
    let mut w = Wake {
        text: text.to_string(),
        ..Default::default()
    };
    if let Some(rest) = after(text, "\nMessage: ").or_else(|| after(text, "Message: ")) {
        w.message = word(rest);
    }
    if let Some(rest) = after(text, "Thread: \"") {
        if let Some(end) = rest.find("\" (") {
            w.subject = rest[..end].to_string();
            w.thread = word(&rest[end + 3..]);
        }
    }
    if let Some(rest) = after(text, "\nFrom: ") {
        w.from = word(rest);
    }
    if let Some(rest) = after(text, "\nKind: ") {
        w.kind = word(rest);
    }
    if let Some(rest) = after(text, "Access request ") {
        w.request = word(rest);
    }
    if let Some(rest) = after(text, "Background check ") {
        w.screening = word(rest);
    }
    if let Some(rest) = after(text, "Credential request ") {
        w.credential = word(rest);
    }
    // The first contract id: "Contract offer k-1", "contract k-1".
    let mut rest = text;
    while let Some(i) = rest.find("ontract ") {
        rest = &rest[i + 8..];
        let id = word(rest.strip_prefix("offer ").unwrap_or(rest));
        if id.starts_with("k-") {
            w.contract = id;
            break;
        }
    }
    w
}

fn matches(when: &Value, w: &Wake) -> bool {
    let field = |k: &str| when[k].as_str();
    field("from").is_none_or(|f| f == w.from)
        && field("kind").is_none_or(|k| k == w.kind)
        && field("subject").is_none_or(|s| w.subject.contains(s))
        && field("contains").is_none_or(|c| w.text.contains(c))
        && field("request").is_none_or(|_| !w.request.is_empty())
}

fn substitute(v: &Value, vars: &[(&str, String)]) -> Value {
    match v {
        Value::String(s) => {
            let mut s = s.clone();
            for (k, val) in vars {
                s = s.replace(k, val);
            }
            json!(s)
        }
        Value::Array(a) => Value::Array(a.iter().map(|x| substitute(x, vars)).collect()),
        Value::Object(o) => Value::Object(
            o.iter()
                .map(|(k, x)| (k.clone(), substitute(x, vars)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Runs the Stop hook from the settings, as Claude Code does: the reason
/// when it blocks the stop.
fn stop_hook() -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let settings: Value = serde_json::from_str(
        &std::fs::read_to_string(format!("{home}/.claude/settings.json")).ok()?,
    )
    .ok()?;
    let cmd = settings["hooks"]["Stop"][0]["hooks"][0]["command"]
        .as_str()?
        .to_string();
    let mut child = Command::new("sh")
        .args(["-c", &cmd])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .ok()?;
    let _ = child
        .stdin
        .take()?
        .write_all(br#"{"hook_event_name":"Stop","stop_hook_active":false}"#);
    let out = child.wait_with_output().ok()?;
    let v: Value = serde_json::from_slice(&out.stdout).ok()?;
    (v["decision"] == "block").then(|| v["reason"].as_str().unwrap_or("").to_string())
}

fn emit(v: Value) {
    let mut out = std::io::stdout();
    let _ = writeln!(out, "{v}");
    let _ = out.flush();
}

pub fn run(script: &str, proto: Proto) -> i32 {
    match play(script, proto) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("fake agent: {e:#}");
            1
        }
    }
}

/// The protocol the fake speaks: Claude Code's stream-json, or pi's RPC.
#[derive(Clone, Copy, PartialEq)]
pub enum Proto {
    Claude,
    Pi,
}

/// What the fake writes, in the protocol's shapes.
struct Out {
    proto: Proto,
    session: String,
}

impl Out {
    /// The text of a wake, or None for other input. pi's commands are
    /// answered first.
    fn wake(&self, msg: &Value) -> Option<String> {
        match self.proto {
            Proto::Claude => {
                if msg["type"] != "user" {
                    return None;
                }
                Some(match &msg["message"]["content"] {
                    Value::String(s) => s.clone(),
                    Value::Array(a) => a
                        .iter()
                        .filter_map(|c| c["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("\n"),
                    _ => String::new(),
                })
            }
            Proto::Pi => {
                let kind = msg["type"].as_str()?;
                if !["prompt", "follow_up", "steer"].contains(&kind) {
                    return None;
                }
                emit(
                    json!({ "id": msg["id"], "type": "response", "command": kind, "success": true, "data": { "disposition": "started" } }),
                );
                Some(msg["message"].as_str().unwrap_or("").to_string())
            }
        }
    }

    fn begin(&self) {
        match self.proto {
            Proto::Claude => emit(
                json!({ "type": "system", "subtype": "init", "session_id": self.session, "tools": [], "model": "fake" }),
            ),
            Proto::Pi => emit(json!({ "type": "agent_start" })),
        }
    }

    fn say(&self, text: &Value) {
        match self.proto {
            Proto::Claude => emit(
                json!({ "type": "assistant", "session_id": self.session, "message": { "role": "assistant", "content": [{ "type": "text", "text": text }] } }),
            ),
            Proto::Pi => emit(
                json!({ "type": "message_end", "message": { "role": "assistant", "content": [{ "type": "text", "text": text }], "stopReason": "stop", "usage": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 } } }),
            ),
        }
    }

    /// A tool call starts: `name` is the tool as the agent knows it.
    fn call(&self, name: &str, input: &Value) {
        match self.proto {
            Proto::Claude => emit(
                json!({ "type": "assistant", "session_id": self.session, "message": { "role": "assistant", "content": [{ "type": "tool_use", "id": "t", "name": name, "input": input }] } }),
            ),
            Proto::Pi => emit(
                json!({ "type": "tool_execution_start", "toolCallId": "t", "toolName": name, "args": input }),
            ),
        }
    }

    fn result(&self, name: &str, text: &str, is_error: bool) {
        match self.proto {
            Proto::Claude => emit(
                json!({ "type": "user", "session_id": self.session, "message": { "role": "user", "content": [{ "type": "tool_result", "tool_use_id": "t", "content": text, "is_error": is_error }] } }),
            ),
            Proto::Pi => emit(
                json!({ "type": "tool_execution_end", "toolCallId": "t", "toolName": name, "result": { "content": [{ "type": "text", "text": text }] }, "isError": is_error }),
            ),
        }
    }

    /// Mail handed over while the turn runs: Claude Code's Stop hook, or
    /// pi's queued follow-ups.
    fn more(&self, input: &mpsc::Receiver<Value>) -> Option<String> {
        match self.proto {
            Proto::Claude => stop_hook(),
            Proto::Pi => {
                let mut texts = Vec::new();
                while let Ok(msg) = input.try_recv() {
                    if let Some(t) = self.wake(&msg) {
                        texts.push(t);
                    }
                }
                (!texts.is_empty()).then(|| texts.join("\n\n"))
            }
        }
    }

    fn end(&self, turns: u64, errors: u32, started: Instant) {
        match self.proto {
            Proto::Claude => emit(json!({
                "type": "result", "subtype": "success", "is_error": false, "session_id": self.session, "num_turns": turns,
                "duration_ms": started.elapsed().as_millis() as u64, "result": format!("played; {errors} tool errors"),
                "total_cost_usd": 0.0, "usage": { "input_tokens": 100, "output_tokens": 10, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0 },
            })),
            Proto::Pi => {
                emit(
                    json!({ "type": "message_end", "message": { "role": "assistant", "content": [{ "type": "text", "text": format!("played; {errors} tool errors") }], "stopReason": "stop", "usage": { "input": 100, "output": 10, "cacheRead": 0, "cacheWrite": 0 } } }),
                );
                emit(json!({ "type": "agent_end", "messages": [], "willRetry": false }));
                emit(json!({ "type": "agent_settled", "aborted": false }));
            }
        }
    }
}

fn play(script: &str, proto: Proto) -> Result<()> {
    let script: Value = serde_yaml::from_str(
        &std::fs::read_to_string(script).with_context(|| format!("reading {script}"))?,
    )?;
    let rules = script["rules"].as_array().cloned().unwrap_or_default();
    let out = Out {
        proto,
        session: format!("fake-{}", std::process::id()),
    };
    let mut mcp: Option<Mcp> = None;
    let mut fired: HashSet<usize> = HashSet::new();
    let mut artifact = String::new();
    let mut turns = 0u64;
    // Input is read on its own thread, so pi's follow-ups queue up while a
    // turn plays.
    let (tx, input) = mpsc::channel::<Value>();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if let Ok(v) = serde_json::from_str::<Value>(&line) {
                if tx.send(v).is_err() {
                    break;
                }
            }
        }
    });
    while let Ok(msg) = input.recv() {
        let Some(text) = out.wake(&msg) else {
            continue;
        };
        let started = Instant::now();
        turns += 1;
        out.begin();
        let mut last = String::new();
        let mut errors = 0;
        // A turn: the wake, then whatever is handed over before it ends.
        let mut text = text;
        for _ in 0..5 {
            let wake = parse_wake(&text);
            let rule = rules.iter().enumerate().find(|(i, r)| {
                !(r["when"]["once"] == json!(true) && fired.contains(i))
                    && matches(&r["when"], &wake)
            });
            if let Some((i, rule)) = rule {
                fired.insert(i);
                for action in rule["do"].as_array().into_iter().flatten() {
                    let vars = [
                        ("$message", wake.message.clone()),
                        ("$thread", wake.thread.clone()),
                        ("$from", wake.from.clone()),
                        ("$subject", wake.subject.clone()),
                        ("$request", wake.request.clone()),
                        ("$screening", wake.screening.clone()),
                        ("$contract", wake.contract.clone()),
                        ("$credential", wake.credential.clone()),
                        ("$artifact", artifact.clone()),
                        ("$last", last.clone()),
                    ];
                    let Some((name, args)) = action.as_object().and_then(|o| o.iter().next())
                    else {
                        continue;
                    };
                    let args = substitute(args, &vars);
                    match name.as_str() {
                        "say" => out.say(&args),
                        "sleep" => std::thread::sleep(std::time::Duration::from_secs_f64(
                            args.as_f64().unwrap_or(1.0),
                        )),
                        "shell" => {
                            let cmd = args.as_str().unwrap_or("true");
                            let tool = if proto == Proto::Pi { "bash" } else { "Bash" };
                            out.call(tool, &json!({ "command": cmd }));
                            last = match Command::new("sh").args(["-c", cmd]).output() {
                                Ok(o) => {
                                    String::from_utf8_lossy(&o.stdout).into_owned()
                                        + &String::from_utf8_lossy(&o.stderr)
                                }
                                Err(e) => e.to_string(),
                            };
                            out.result(tool, &last, false);
                        }
                        tool => {
                            let full = format!("mcp__office__{tool}");
                            out.call(&full, &args);
                            if mcp.is_none() {
                                mcp = Some(Mcp::start()?);
                            }
                            let r = mcp.as_mut().unwrap().request(
                                "tools/call",
                                json!({ "name": tool, "arguments": args }),
                            )?;
                            last = r["content"][0]["text"].as_str().unwrap_or("").to_string();
                            let is_error = r["isError"] == json!(true);
                            if is_error {
                                errors += 1;
                            } else if tool == "drive_publish" {
                                if let Ok(v) = serde_json::from_str::<Value>(&last) {
                                    artifact = v["ref"].as_str().unwrap_or("").to_string();
                                }
                            }
                            out.result(&full, &last, is_error);
                        }
                    }
                }
            }
            match out.more(&input) {
                Some(more) => text = more,
                None => break,
            }
        }
        out.end(turns, errors, started);
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_wake() {
        let text = "New mail: 1 message(s).\n\n── 1 of 1 ──\nThread: \"Landing page v1\" (t-3)\nMessage: m-7\nFrom: cto (CTO, your manager)\nKind: assignment · Expects: done\n";
        let w = parse_wake(text);
        assert_eq!(
            (
                w.message.as_str(),
                w.thread.as_str(),
                w.from.as_str(),
                w.kind.as_str(),
                w.subject.as_str()
            ),
            ("m-7", "t-3", "cto", "assignment", "Landing page v1")
        );
        assert!(matches(&json!({ "from": "cto", "kind": "assignment" }), &w));
        assert!(!matches(&json!({ "from": "board" }), &w));
        let r = parse_wake("Access request r-12 — decision needed.");
        assert_eq!(r.request, "r-12");
        assert_eq!(parse_wake("**Contract offer k-3: Audit**").contract, "k-3");
        assert_eq!(
            parse_wake("a contract with **Counter-offer for contract k-2**").contract,
            "k-2"
        );
        assert_eq!(
            substitute(&json!({ "m": "$message!" }), &[("$message", "m-7".into())]),
            json!({ "m": "m-7!" })
        );
    }
}
