//! The MCP gateway over a core without VMs: which servers a session gets,
//! and MCP on a connection as the agent's bridge carries it.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use anyhow::anyhow;
use futures_util::future::BoxFuture;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use super::McpServer;
use crate::testing::TestCore;
use crate::{NewSession, Session};

/// Tools `echo` and `fail`, and who called.
#[derive(Default)]
struct Echo {
    callers: Mutex<Vec<String>>,
}

impl McpServer for Echo {
    fn instructions(&self) -> Option<String> {
        Some("Echoes.".into())
    }

    fn tools<'a>(&'a self, s: &'a Arc<Session>) -> BoxFuture<'a, anyhow::Result<Value>> {
        Box::pin(async move { Ok(json!([{ "name": "echo", "for": s.id }])) })
    }

    fn call<'a>(
        &'a self,
        s: &'a Arc<Session>,
        tool: &'a str,
        args: Value,
    ) -> BoxFuture<'a, anyhow::Result<Value>> {
        self.callers.lock().unwrap().push(s.id.clone());
        Box::pin(async move {
            match tool {
                "echo" => Ok(args),
                _ => Err(anyhow!("no tool {tool}")),
            }
        })
    }
}

fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn session(t: &TestCore, pairs: &[(&str, &str)]) -> Arc<Session> {
    let mut new = NewSession::plain("web".into(), t.workdir(), Default::default());
    new.labels = labels(pairs);
    t.core.test_session(new, true).unwrap()
}

const CONFIG: &str = "profiles:\n  web: {}\n";

/// The session's `mcp` events: server, tool, ok and error.
fn calls(t: &TestCore, s: &Session) -> Vec<Value> {
    t.core
        .daemon()
        .recent_events(Some(&s.id))
        .into_iter()
        .map(|e| serde_json::to_value(e).unwrap())
        .filter(|e| e["type"] == "mcp")
        .map(|e| {
            assert!(e["ms"].as_f64().unwrap() >= 0.0, "{e}");
            json!([e["server"], e["tool"], e["ok"], e["error"]])
        })
        .collect()
}

/// A connection to the gateway as `s`'s VM: the server named, then lines.
struct Conn {
    w: tokio::io::WriteHalf<UnixStream>,
    r: BufReader<tokio::io::ReadHalf<UnixStream>>,
}

impl Conn {
    fn open(t: &TestCore, s: &Arc<Session>) -> Conn {
        let (host, guest) = UnixStream::pair().unwrap();
        let (d, s) = (t.core.daemon().clone(), s.clone());
        tokio::spawn(async move { super::connection(&d, &s, host).await });
        let (r, w) = tokio::io::split(guest);
        Conn {
            w,
            r: BufReader::new(r),
        }
    }

    async fn send(&mut self, line: &str) {
        self.w
            .write_all(format!("{line}\n").as_bytes())
            .await
            .unwrap();
    }

    async fn reply(&mut self) -> Option<Value> {
        let mut l = String::new();
        let n = self.r.read_line(&mut l).await.unwrap();
        (n > 0).then(|| serde_json::from_str(&l).unwrap())
    }

    async fn ask(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(
            &json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string(),
        )
        .await;
        let r = self.reply().await.unwrap();
        assert_eq!(r["id"], id, "{r}");
        r
    }
}

#[tokio::test]
async fn a_session_talks_mcp_with_its_server() {
    let t = TestCore::new(CONFIG);
    let s = session(&t, &[("team", "a")]);
    let echo = Arc::new(Echo::default());
    t.core
        .add_mcp_server("echo", labels(&[("team", "a")]), echo.clone())
        .unwrap();
    let mut c = Conn::open(&t, &s);
    c.send("echo").await;

    let init = c
        .ask(1, "initialize", json!({ "protocolVersion": "2025-03-26" }))
        .await;
    assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
    assert_eq!(init["result"]["serverInfo"]["name"], "echo");
    assert_eq!(init["result"]["instructions"], "Echoes.");
    // A notification gets no answer; blank lines are nothing.
    c.send(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
        .await;
    c.send("").await;
    assert_eq!(c.ask(2, "ping", json!({})).await["result"], json!({}));

    let tools = c.ask(3, "tools/list", json!({})).await;
    assert_eq!(tools["result"]["tools"][0]["for"], s.id.as_str(), "{tools}");
    let ok = c
        .ask(
            4,
            "tools/call",
            json!({ "name": "echo", "arguments": { "a": 1 } }),
        )
        .await;
    assert_eq!(ok["result"]["isError"], false);
    let text = ok["result"]["content"][0]["text"].as_str().unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(text).unwrap(),
        json!({ "a": 1 })
    );
    let bad = c.ask(5, "tools/call", json!({ "name": "nope" })).await;
    assert_eq!(bad["result"]["isError"], true);
    assert_eq!(bad["result"]["content"][0]["text"], "no tool nope");
    let unknown = c.ask(6, "resources/list", json!({})).await;
    assert_eq!(unknown["error"]["code"], -32601);
    assert_eq!(
        echo.callers.lock().unwrap().as_slice(),
        [s.id.clone(), s.id.clone()]
    );

    // Each tool call is an event, and nothing else is.
    assert_eq!(
        calls(&t, &s),
        [
            json!(["echo", "echo", true, null]),
            json!(["echo", "nope", false, "no tool nope"]),
        ]
    );

    c.send("not json").await;
    let r = c.reply().await.unwrap();
    assert_eq!(r["error"]["code"], -32700);
    assert!(r["error"]["message"]
        .as_str()
        .unwrap()
        .starts_with("bad request"));
}

#[tokio::test]
async fn a_server_is_only_for_the_sessions_it_picks() {
    let t = TestCore::new(CONFIG);
    let (a, b) = (session(&t, &[("team", "a")]), session(&t, &[("team", "b")]));
    let id = t
        .core
        .add_mcp_server("echo", labels(&[("team", "a")]), Arc::new(Echo::default()))
        .unwrap();
    let mut c = Conn::open(&t, &b);
    c.send("echo").await;
    let r = c.ask(1, "tools/list", json!({})).await;
    assert_eq!(r["error"]["message"], "no MCP server echo for this session");
    // A call to a server the session hasn't is an event too, refused.
    c.ask(9, "tools/call", json!({ "name": "echo" })).await;
    assert_eq!(
        calls(&t, &b),
        [json!([
            "echo",
            "echo",
            false,
            "no MCP server echo for this session"
        ])]
    );
    // Not even a notification is anyone's.
    c.send(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
        .await;
    assert!(c.ask(2, "ping", json!({})).await["error"].is_object());

    let mut c = Conn::open(&t, &a);
    c.send("other").await;
    assert!(c.ask(1, "ping", json!({})).await["error"].is_object());

    assert!(t.core.remove_mcp_server(id));
    assert!(!t.core.remove_mcp_server(id));
    let mut c = Conn::open(&t, &a);
    c.send("echo").await;
    assert!(c.ask(1, "ping", json!({})).await["error"].is_object());
}

#[test]
fn servers_have_names_and_one_per_name_and_sessions() {
    let t = TestCore::new(CONFIG);
    let echo = || Arc::new(Echo::default());
    for bad in ["", "Office", "a b", "../x"] {
        assert!(
            t.core.add_mcp_server(bad, BTreeMap::new(), echo()).is_err(),
            "{bad:?}"
        );
    }
    t.core
        .add_mcp_server("echo", labels(&[("team", "a")]), echo())
        .unwrap();
    let e = t
        .core
        .add_mcp_server("echo", labels(&[("team", "a")]), echo())
        .unwrap_err()
        .to_string();
    assert!(e.contains("already registered"), "{e}");
    // The same name for other sessions is another server; the earliest
    // registered for a session is its.
    t.core
        .add_mcp_server("echo", BTreeMap::new(), echo())
        .unwrap();
    let a = session(&t, &[("team", "a")]);
    let gw = &t.core.daemon().gateway;
    assert!(gw.server(&a, "echo").is_some());
    assert!(gw.server(&a, "other").is_none());
}

#[tokio::test]
async fn an_overlong_line_is_answered_and_the_connection_ends() {
    let t = TestCore::new(CONFIG);
    let s = session(&t, &[]);
    t.core
        .add_mcp_server("echo", BTreeMap::new(), Arc::new(Echo::default()))
        .unwrap();
    let mut c = Conn::open(&t, &s);
    let long = "x".repeat(zapper_policy::proto::MAX_MCP_LINE + 1);
    let send = async {
        let _ = c.w.write_all(format!("echo\n{long}\n").as_bytes()).await;
    };
    let mut reply = String::new();
    let read = c.r.read_line(&mut reply);
    let (_, n) = tokio::join!(send, read);
    n.unwrap();
    let r: Value = serde_json::from_str(&reply).unwrap();
    assert!(
        r["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("request longer than"),
        "{r}"
    );
    assert_eq!(c.reply().await, None, "hung up");
}

#[tokio::test]
async fn a_sessions_vm_connects_on_its_mcp_port() {
    let t = TestCore::new(CONFIG);
    let s = session(&t, &[]);
    t.core
        .add_mcp_server("echo", BTreeMap::new(), Arc::new(Echo::default()))
        .unwrap();
    let path = t.dir.join("mcp.sock");
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    tokio::spawn(super::serve(t.core.daemon().clone(), s.clone(), listener));
    let mut conns = Vec::new();
    for i in 0..super::MAX_CONNECTIONS + 1 {
        let stream = UnixStream::connect(&path).await.unwrap();
        let (r, w) = tokio::io::split(stream);
        let mut c = Conn {
            w,
            r: BufReader::new(r),
        };
        c.send("echo").await;
        if i < super::MAX_CONNECTIONS {
            assert_eq!(c.ask(1, "ping", json!({})).await["result"], json!({}));
        } else {
            // One too many at once: closed unanswered (an end, or a reset).
            let _ =
                c.w.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n")
                    .await;
            let mut l = String::new();
            assert!(
                !matches!(c.r.read_line(&mut l).await, Ok(n) if n > 0),
                "{l}"
            );
        }
        conns.push(c);
    }
}

/// No instructions, and tools it can't list.
struct Broken;

impl McpServer for Broken {
    fn tools<'a>(&'a self, _s: &'a Arc<Session>) -> BoxFuture<'a, anyhow::Result<Value>> {
        Box::pin(async { Err(anyhow!("the tools are elsewhere")) })
    }

    fn call<'a>(
        &'a self,
        _s: &'a Arc<Session>,
        _tool: &'a str,
        _args: Value,
    ) -> BoxFuture<'a, anyhow::Result<Value>> {
        Box::pin(async { Ok(Value::Null) })
    }
}

#[tokio::test]
async fn a_server_that_cant_list_its_tools_says_so() {
    let t = TestCore::new(CONFIG);
    let s = session(&t, &[]);
    t.core
        .add_mcp_server("broken", BTreeMap::new(), Arc::new(Broken))
        .unwrap();
    let mut c = Conn::open(&t, &s);
    c.send("broken").await;
    let init = c.ask(1, "initialize", json!({})).await;
    assert!(init["result"].get("instructions").is_none(), "{init}");
    let r = c.ask(2, "tools/list", json!({})).await;
    assert_eq!(r["error"]["code"], -32603);
    assert_eq!(r["error"]["message"], "the tools are elsewhere");
}

#[tokio::test]
async fn a_call_the_guest_names_at_length_is_an_event_of_bounded_names() {
    let t = TestCore::new(CONFIG);
    let s = session(&t, &[]);
    let mut c = Conn::open(&t, &s);
    c.send(&"s".repeat(1000)).await;
    c.ask(1, "tools/call", json!({ "name": "t".repeat(1000) }))
        .await;
    // A call without an id gets no answer, and is an event all the same.
    c.send(r#"{"jsonrpc":"2.0","method":"tools/call","params":{"name":"x"}}"#)
        .await;
    assert_eq!(c.ask(2, "ping", json!({})).await["id"], 2);
    let calls = calls(&t, &s);
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_eq!(calls[0][0].as_str().unwrap().len(), super::MAX_NAME);
    assert_eq!(calls[0][1].as_str().unwrap().len(), super::MAX_NAME);
    assert!(calls[0][3].as_str().unwrap().len() <= super::MAX_ERROR);
    assert_eq!(calls[1][1], "x");
    assert_eq!(calls[1][2], false);
}

#[tokio::test]
async fn a_connection_that_names_no_server_just_ends() {
    let t = TestCore::new(CONFIG);
    let s = session(&t, &[]);
    let (host, guest) = UnixStream::pair().unwrap();
    drop(guest);
    super::connection(t.core.daemon(), &s, host).await.unwrap();
}

/// An overlong *first* line (the server name) ends the connection with no
/// reply — it fails closed, like an overlong line mid-stream.
#[tokio::test]
async fn an_overlong_server_name_line_ends_the_connection() {
    let t = TestCore::new(CONFIG);
    let s = session(&t, &[]);
    t.core
        .add_mcp_server("echo", BTreeMap::new(), Arc::new(Echo::default()))
        .unwrap();
    let (host, guest) = UnixStream::pair().unwrap();
    let (d, sc) = (t.core.daemon().clone(), s.clone());
    let served = tokio::spawn(async move { super::connection(&d, &sc, host).await });
    let (r, mut w) = tokio::io::split(guest);
    // More than MAX_MCP_LINE bytes with no newline: the name line can't be read.
    let long = "x".repeat(zapper_policy::proto::MAX_MCP_LINE + 1);
    let _ = w.write_all(long.as_bytes()).await;
    let _ = w.flush().await;
    // No reply, and the connection ends with an error (not a silent accept).
    let mut l = String::new();
    assert_eq!(BufReader::new(r).read_line(&mut l).await.unwrap(), 0, "{l}");
    assert!(served.await.unwrap().is_err(), "the name line was rejected");
}

/// A valid-JSON message that is not an object (a scalar or array) draws no
/// reply and no event, and does not break the connection.
#[tokio::test]
async fn a_non_object_message_is_ignored_without_breaking_the_connection() {
    let t = TestCore::new(CONFIG);
    let s = session(&t, &[]);
    t.core
        .add_mcp_server("echo", BTreeMap::new(), Arc::new(Echo::default()))
        .unwrap();
    let mut c = Conn::open(&t, &s);
    c.send("echo").await;
    for scalar in ["123", "\"hi\"", "[1,2]", "null"] {
        c.send(scalar).await;
    }
    // The connection still answers a well-formed request afterwards.
    assert_eq!(c.ask(1, "ping", json!({})).await["result"], json!({}));
    // None of the scalars was a tool call, so there are no events.
    assert!(calls(&t, &s).is_empty(), "{:?}", calls(&t, &s));
}

/// Removing a server takes effect on a live connection at once: the next
/// call no longer reaches it.
#[tokio::test]
async fn a_removed_server_stops_serving_a_live_connection() {
    let t = TestCore::new(CONFIG);
    let s = session(&t, &[]);
    let id = t
        .core
        .add_mcp_server("echo", BTreeMap::new(), Arc::new(Echo::default()))
        .unwrap();
    let mut c = Conn::open(&t, &s);
    c.send("echo").await;
    assert_eq!(c.ask(1, "ping", json!({})).await["result"], json!({}));
    assert!(t.core.remove_mcp_server(id));
    let r = c.ask(2, "ping", json!({})).await;
    assert_eq!(r["error"]["message"], "no MCP server echo for this session");
}

/// `initialize` without a requested protocol version gets the server default.
#[tokio::test]
async fn initialize_without_a_version_gets_the_default() {
    let t = TestCore::new(CONFIG);
    let s = session(&t, &[]);
    t.core
        .add_mcp_server("echo", BTreeMap::new(), Arc::new(Echo::default()))
        .unwrap();
    let mut c = Conn::open(&t, &s);
    c.send("echo").await;
    let init = c.ask(1, "initialize", json!({})).await;
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(
        init["result"]["serverInfo"]["version"],
        env!("CARGO_PKG_VERSION")
    );
}
