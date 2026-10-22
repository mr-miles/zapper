//! The gateway against fake servers: a streamable-HTTP one that answers some
//! requests as event streams, and a stdio one.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use zapper_policy::{evaluate_mcp, Config, Decision, McpRequest, McpServer, Scope};

use super::gateway::{Backend, Call, Gateway};

const POLICY: &str = r#"
policy:
  defaults: { mcp: deny }
  mcp:
    - { action: deny, name: "delete_*", final: true }
    - { action: allow, server: web, type: tool, name: "echo" }
    - { action: ask, server: web, type: tool, name: "ask_me" }
    - { action: allow, server: web, type: resource }
    - { action: allow, server: files, type: tool }
    - { action: allow, server: web, type: prompt, name: "greet" }
"#;

struct Test {
    config: Config,
    servers: HashMap<String, McpServer>,
    asked: Mutex<Vec<String>>,
    approve: AtomicBool,
    calls: Mutex<Vec<Call>>,
}

impl Test {
    fn new(servers: &[(&str, McpServer)]) -> Self {
        Test {
            config: Config::from_yaml(POLICY).unwrap(),
            servers: servers
                .iter()
                .map(|(n, s)| (n.to_string(), s.clone()))
                .collect(),
            asked: Default::default(),
            approve: AtomicBool::new(false),
            calls: Default::default(),
        }
    }
}

impl Backend for Test {
    fn server(&self, name: &str) -> Option<McpServer> {
        self.servers.get(name).cloned()
    }
    fn attached(&self) -> Vec<String> {
        let mut v: Vec<_> = self.servers.keys().cloned().collect();
        v.sort();
        v
    }
    fn secret(&self, name: &str) -> Result<String, String> {
        match name {
            "tok" => Ok("s3cret".into()),
            _ => Err(format!("unknown secret {name}")),
        }
    }
    fn decide(&self, req: &McpRequest) -> Decision {
        evaluate_mcp(&[(Scope::Global, &self.config.policy)], req)
    }
    async fn ask(&self, req: &McpRequest, _args: &Value) -> bool {
        self.asked
            .lock()
            .unwrap()
            .push(format!("{}:{}", req.server, req.name));
        self.approve.load(Ordering::Relaxed)
    }
    fn record(&self, call: Call) {
        self.calls.lock().unwrap().push(call);
    }
}

/// What the fake HTTP server saw.
#[derive(Default)]
struct Seen {
    requests: AtomicUsize,
    tool_calls: AtomicUsize,
    /// Handshakes, to see a session being renewed.
    initializes: AtomicUsize,
    /// Answer the next request with 404, as a server that restarted does.
    forget_session: AtomicBool,
}

async fn read_request(
    sock: &mut tokio::net::TcpStream,
) -> Option<(HashMap<String, String>, Vec<u8>)> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let end = loop {
        let n = sock.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..end]).to_string();
    let headers: HashMap<String, String> = head
        .lines()
        .skip(1)
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let len: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    while buf.len() < end + len {
        let n = sock.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    Some((headers, buf[end..end + len].to_vec()))
}

async fn respond(
    sock: &mut tokio::net::TcpStream,
    status: &str,
    headers: &[(&str, &str)],
    body: &str,
) {
    let mut out = format!(
        "HTTP/1.1 {status}\r\ncontent-length: {}\r\nconnection: close\r\n",
        body.len()
    );
    for (k, v) in headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str("\r\n");
    out.push_str(body);
    let _ = sock.write_all(out.as_bytes()).await;
    let _ = sock.shutdown().await;
}

/// A streamable-HTTP MCP server on a loopback port.
async fn fake_http(seen: Arc<Seen>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let seen = seen.clone();
            tokio::spawn(async move {
                let Some((h, body)) = read_request(&mut sock).await else {
                    return;
                };
                seen.requests.fetch_add(1, Ordering::Relaxed);
                let msg: Value = serde_json::from_slice(&body).unwrap();
                let method = msg["method"].as_str().unwrap_or("");
                let id = msg["id"].clone();
                if h.get("authorization").map(String::as_str) != Some("Bearer s3cret") {
                    return respond(&mut sock, "401 Unauthorized", &[], "").await;
                }
                if method == "initialize" {
                    seen.initializes.fetch_add(1, Ordering::Relaxed);
                    let r = json!({ "jsonrpc": "2.0", "id": id, "result": {
                        "protocolVersion": "2025-03-26", "capabilities": { "tools": {} },
                        "serverInfo": { "name": "fake", "version": "1" } } });
                    return respond(
                        &mut sock,
                        "200 OK",
                        &[
                            ("content-type", "application/json"),
                            ("mcp-session-id", "sess-1"),
                        ],
                        &r.to_string(),
                    )
                    .await;
                }
                if seen.forget_session.swap(false, Ordering::Relaxed) {
                    return respond(&mut sock, "404 Not Found", &[], "").await;
                }
                if h.get("mcp-session-id").map(String::as_str) != Some("sess-1")
                    || h.get("mcp-protocol-version").map(String::as_str) != Some("2025-03-26")
                {
                    return respond(
                        &mut sock,
                        "400 Bad Request",
                        &[],
                        "session or version missing",
                    )
                    .await;
                }
                let p = &msg["params"];
                let result = match method {
                    "notifications/initialized" => {
                        return respond(&mut sock, "202 Accepted", &[], "").await
                    }
                    "tools/list" => json!({ "tools": [
                        { "name": "echo", "description": "Echoes", "inputSchema": { "type": "object" } },
                        { "name": "ask_me", "inputSchema": { "type": "object" } },
                        { "name": "delete_all", "inputSchema": { "type": "object" } },
                    ] }),
                    "tools/call" => {
                        seen.tool_calls.fetch_add(1, Ordering::Relaxed);
                        let r = json!({ "jsonrpc": "2.0", "id": id, "result": { "content": [
                            { "type": "text", "text": format!("{}:{}", p["name"].as_str().unwrap(), p["arguments"]["text"].as_str().unwrap_or("")) } ] } });
                        // Answered on an event stream, after a notification.
                        let sse = format!(
                            "event: message\ndata: {}\n\nevent: message\ndata: {}\n\n",
                            json!({ "jsonrpc": "2.0", "method": "notifications/message", "params": {} }),
                            r
                        );
                        return respond(
                            &mut sock,
                            "200 OK",
                            &[("content-type", "text/event-stream")],
                            &sse,
                        )
                        .await;
                    }
                    "resources/list" => {
                        json!({ "resources": [{ "uri": "mem://a b/c", "name": "c" }] })
                    }
                    "resources/read" => {
                        json!({ "contents": [{ "uri": p["uri"], "text": "hello" }] })
                    }
                    "prompts/list" => {
                        let e = json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": "no prompts" } });
                        return respond(
                            &mut sock,
                            "200 OK",
                            &[("content-type", "application/json")],
                            &e.to_string(),
                        )
                        .await;
                    }
                    "prompts/get" => json!({ "messages": [] }),
                    _ => {
                        let e = json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": "nope" } });
                        return respond(
                            &mut sock,
                            "200 OK",
                            &[("content-type", "application/json")],
                            &e.to_string(),
                        )
                        .await;
                    }
                };
                let r = json!({ "jsonrpc": "2.0", "id": id, "result": result });
                respond(
                    &mut sock,
                    "200 OK",
                    &[("content-type", "application/json")],
                    &r.to_string(),
                )
                .await;
            });
        }
    });
    url
}

fn web(url: &str) -> McpServer {
    McpServer {
        url: Some(url.into()),
        headers: BTreeMap::from([("Authorization".into(), "Bearer {{secret:tok}}".into())]),
        ..Default::default()
    }
}

async fn call(g: &Gateway, b: &Test, method: &str, params: Value) -> Value {
    let msg = json!({ "jsonrpc": "2.0", "id": 7, "method": method, "params": params });
    g.handle(b, msg).await.expect("an answer")
}

fn names(v: &Value, key: &str, field: &str) -> Vec<String> {
    let mut n: Vec<String> = v["result"][key]
        .as_array()
        .unwrap_or_else(|| panic!("{v}"))
        .iter()
        .map(|t| t[field].as_str().unwrap().to_string())
        .collect();
    n.sort();
    n
}

fn text(v: &Value) -> String {
    v["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("{v}"))
        .to_string()
}

#[tokio::test]
async fn http_server_through_the_gateway() {
    let seen = Arc::new(Seen::default());
    let url = fake_http(seen.clone()).await;
    let b = Test::new(&[("web", web(&url))]);
    let g = Gateway::default();

    // Notifications take no answer; initialize lists the servers.
    assert!(g
        .handle(
            &b,
            json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })
        )
        .await
        .is_none());
    let init = call(
        &g,
        &b,
        "initialize",
        json!({ "protocolVersion": "2024-11-05" }),
    )
    .await;
    assert_eq!(init["result"]["protocolVersion"], "2024-11-05");
    assert!(init["result"]["instructions"]
        .as_str()
        .unwrap()
        .contains("- web"));
    let init = call(
        &g,
        &b,
        "initialize",
        json!({ "protocolVersion": "1999-01-01" }),
    )
    .await;
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");

    // Tools are namespaced; the denied one is not offered; the secret and
    // the server's session and version were used to get them.
    let tools = call(&g, &b, "tools/list", json!({})).await;
    assert_eq!(
        names(&tools, "tools", "name"),
        ["web__ask_me", "web__echo", "zapper-status"]
    );

    // An allowed call, answered by the server on an event stream.
    let r = call(
        &g,
        &b,
        "tools/call",
        json!({ "name": "web__echo", "arguments": { "text": "hi" } }),
    )
    .await;
    assert_eq!(text(&r), "echo:hi");
    assert_eq!(seen.tool_calls.load(Ordering::Relaxed), 1);

    // Denied: the model is told, and the server never hears of it.
    let r = call(&g, &b, "tools/call", json!({ "name": "web__delete_all" })).await;
    assert_eq!(r["result"]["isError"], true);
    assert!(text(&r).contains("denied by rule"), "{r}");
    let r = call(&g, &b, "tools/call", json!({ "name": "web__unlisted" })).await;
    assert!(text(&r).contains("denied by rule default"), "{r}");
    assert_eq!(seen.tool_calls.load(Ordering::Relaxed), 1);

    // An ask waits for the operator: refused, then approved.
    let r = call(
        &g,
        &b,
        "tools/call",
        json!({ "name": "web__ask_me", "arguments": { "text": "x" } }),
    )
    .await;
    assert!(text(&r).contains("not approved"), "{r}");
    assert_eq!(seen.tool_calls.load(Ordering::Relaxed), 1);
    b.approve.store(true, Ordering::Relaxed);
    let r = call(
        &g,
        &b,
        "tools/call",
        json!({ "name": "web__ask_me", "arguments": { "text": "x" } }),
    )
    .await;
    assert_eq!(text(&r), "ask_me:x");
    assert_eq!(*b.asked.lock().unwrap(), ["web:ask_me", "web:ask_me"]);

    // Not a name of this session's.
    let r = call(&g, &b, "tools/call", json!({ "name": "nope__echo" })).await;
    assert_eq!(r["error"]["code"], -32602);
    let r = call(&g, &b, "tools/call", json!({ "name": "plain" })).await;
    assert_eq!(r["error"]["code"], -32602);
    let r = call(&g, &b, "frobnicate", json!({})).await;
    assert_eq!(r["error"]["code"], -32601);

    // Every decision was recorded.
    let calls = b.calls.lock().unwrap();
    let outcome: Vec<(String, bool)> = calls.iter().map(|c| (c.name.clone(), c.allowed)).collect();
    assert_eq!(
        outcome,
        [
            ("echo".into(), true),
            ("delete_all".into(), false),
            ("unlisted".into(), false),
            ("ask_me".into(), false),
            ("ask_me".into(), true),
        ]
    );
    assert_eq!(calls[1].rule.as_deref(), Some("mcp-1"));
    assert_eq!(calls[0].args.as_deref(), Some(r#"{"text":"hi"}"#));
}

#[tokio::test]
async fn resources_prompts_and_a_forgotten_session() {
    let seen = Arc::new(Seen::default());
    let url = fake_http(seen.clone()).await;
    let b = Test::new(&[("web", web(&url))]);
    let g = Gateway::default();

    let list = call(&g, &b, "resources/list", json!({})).await;
    let uri = list["result"]["resources"][0]["uri"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(uri.starts_with("zapper-mcp://web/"), "{uri}");
    // The server forgets us; the gateway shakes hands again and goes on.
    seen.forget_session.store(true, Ordering::Relaxed);
    let read = call(&g, &b, "resources/read", json!({ "uri": uri })).await;
    assert_eq!(read["result"]["contents"][0]["text"], "hello");
    assert_eq!(
        read["result"]["contents"][0]["uri"], uri,
        "clients keep the gateway's URI"
    );
    assert_eq!(seen.initializes.load(Ordering::Relaxed), 2);
    let r = call(
        &g,
        &b,
        "resources/read",
        json!({ "uri": "file:///etc/passwd" }),
    )
    .await;
    assert_eq!(r["error"]["code"], -32002);

    // A server without prompts is not an error; prompts are decided too.
    let list = call(&g, &b, "prompts/list", json!({})).await;
    assert_eq!(list["result"]["prompts"], json!([]));
    let r = call(&g, &b, "prompts/get", json!({ "name": "web__greet" })).await;
    assert_eq!(r["result"]["messages"], json!([]));
    let r = call(&g, &b, "prompts/get", json!({ "name": "web__other" })).await;
    assert!(
        r["error"]["message"].as_str().unwrap().contains("denied"),
        "{r}"
    );
}

#[tokio::test]
async fn unreachable_and_unauthorised_servers_are_skipped() {
    let seen = Arc::new(Seen::default());
    let url = fake_http(seen).await;
    // Nothing listens on port 1; the other has the wrong token.
    let mut wrong = web(&url);
    wrong
        .headers
        .insert("Authorization".into(), "Bearer wrong".into());
    let b = Test::new(&[
        (
            "down",
            McpServer {
                url: Some("http://127.0.0.1:1/mcp".into()),
                ..Default::default()
            },
        ),
        ("web", web(&url)),
        ("wrong", wrong),
    ]);
    let g = Gateway::default();
    let tools = call(&g, &b, "tools/list", json!({})).await;
    // Policy denies everything of "down" and "wrong" by default; the point is
    // that listing still worked, and the status says why.
    assert_eq!(
        names(&tools, "tools", "name"),
        ["web__ask_me", "web__echo", "zapper-status"]
    );
    let status = text(&call(&g, &b, "tools/call", json!({ "name": "zapper-status" })).await);
    assert!(status.contains("down: unavailable"), "{status}");
    assert!(
        status.contains("wrong: unavailable") && status.contains("refused"),
        "{status}"
    );
    assert!(status.contains("web: connected"), "{status}");
}

fn have_python() -> bool {
    std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_ok()
}

const STDIO: &str = r#"
import json, os, sys
for line in sys.stdin:
    m = json.loads(line)
    if "id" not in m:
        continue
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "method": "notifications/message", "params": {}}) + "\n")
    method = m["method"]
    if method == "initialize":
        r = {"protocolVersion": "2025-06-18", "capabilities": {"tools": {}}, "serverInfo": {"name": "s", "version": "1"}}
    elif method == "tools/list":
        r = {"tools": [{"name": "upper", "inputSchema": {"type": "object"}}, {"name": "token", "inputSchema": {"type": "object"}}]}
    elif method == "tools/call":
        p = m["params"]
        if p["name"] == "upper":
            t = p["arguments"]["text"].upper()
        else:
            t = os.environ.get("TOKEN", "") + "|" + str(sorted(k for k in os.environ if k in ("HOME", "PATH", "SECRET_FROM_DAEMON")))
        r = {"content": [{"type": "text", "text": t}]}
    else:
        sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": m["id"], "error": {"code": -32601, "message": "no"}}) + "\n")
        sys.stdout.flush()
        continue
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": m["id"], "result": r}) + "\n")
    sys.stdout.flush()
"#;

#[tokio::test]
async fn stdio_server_with_a_clean_environment() {
    if !have_python() {
        eprintln!("skipped: no python3");
        return;
    }
    std::env::set_var("SECRET_FROM_DAEMON", "x");
    let dir = std::env::temp_dir().join(format!("zapper-mcp-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("server.py");
    std::fs::write(&script, STDIO).unwrap();
    let files = McpServer {
        command: vec!["python3".into(), script.display().to_string()],
        env: BTreeMap::from([("TOKEN".into(), "t={{secret:tok}}".into())]),
        ..Default::default()
    };
    let b = Test::new(&[("files", files)]);
    let g = Gateway::default();
    let tools = call(&g, &b, "tools/list", json!({})).await;
    assert_eq!(
        names(&tools, "tools", "name"),
        ["files__token", "files__upper", "zapper-status"]
    );
    let r = call(
        &g,
        &b,
        "tools/call",
        json!({ "name": "files__upper", "arguments": { "text": "abc" } }),
    )
    .await;
    assert_eq!(text(&r), "ABC");
    // The server got its secret, and not the daemon's environment.
    let r = call(&g, &b, "tools/call", json!({ "name": "files__token" })).await;
    let t = text(&r);
    assert!(t.starts_with("t=s3cret|"), "{t}");
    assert!(!t.contains("SECRET_FROM_DAEMON"), "{t}");
    g.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_server_that_cannot_start_is_reported_to_the_model() {
    let b = Test::new(&[(
        "files",
        McpServer {
            command: vec!["/nonexistent/zapper-mcp".into()],
            ..Default::default()
        },
    )]);
    let g = Gateway::default();
    let r = call(&g, &b, "tools/call", json!({ "name": "files__upper" })).await;
    assert_eq!(r["result"]["isError"], true);
    assert!(text(&r).contains("starting /nonexistent/zapper-mcp"), "{r}");
}

/// The HTTP front over a real connection.
#[tokio::test]
async fn http_front() {
    use http_body_util::{BodyExt, Full};
    use hyper::service::service_fn;
    use hyper_util::rt::TokioIo;

    let seen = Arc::new(Seen::default());
    let url = fake_http(seen).await;
    let b = Arc::new(Test::new(&[("web", web(&url))]));
    let g = Arc::new(Gateway::default());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (sock, _) = listener.accept().await.unwrap();
            let (b, g) = (b.clone(), g.clone());
            tokio::spawn(async move {
                let svc = service_fn(move |req| {
                    let (b, g) = (b.clone(), g.clone());
                    async move { Ok::<_, std::convert::Infallible>(super::process(&g, &*b, req).await) }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(sock), svc)
                    .await;
            });
        }
    });
    let send = |method: &'static str, path: &'static str, body: String| async move {
        let sock = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (mut tx, conn) = hyper::client::conn::http1::handshake(TokioIo::new(sock))
            .await
            .unwrap();
        tokio::spawn(conn);
        let req = hyper::Request::builder()
            .method(method)
            .uri(path)
            .header("host", "mcp.zapper.internal")
            .body(Full::new(bytes::Bytes::from(body)))
            .unwrap();
        let resp = tx.send_request(req).await.unwrap();
        let status = resp.status().as_u16();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8_lossy(&bytes).to_string())
    };

    let (st, body) = send(
        "POST",
        "/mcp",
        json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" }).to_string(),
    )
    .await;
    assert_eq!(
        (
            st,
            serde_json::from_str::<Value>(&body).unwrap()["result"].clone()
        ),
        (200, json!({}))
    );
    // A notification takes no answer.
    let (st, body) = send(
        "POST",
        "/mcp",
        json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }).to_string(),
    )
    .await;
    assert_eq!((st, body.as_str()), (202, ""));
    // A batch answers what needs an answer, in one array.
    let batch = json!([
        { "jsonrpc": "2.0", "id": 1, "method": "ping" },
        { "jsonrpc": "2.0", "method": "notifications/initialized" },
        { "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": { "name": "web__echo", "arguments": { "text": "b" } } },
    ]);
    let (st, body) = send("POST", "/mcp/", batch.to_string()).await;
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!((st, v.as_array().unwrap().len()), (200, 2));
    assert!(body.contains("echo:b"), "{body}");
    let (st, _) = send("POST", "/mcp", "{nope".into()).await;
    assert_eq!(st, 400);
    let (st, _) = send("POST", "/elsewhere", "{}".into()).await;
    assert_eq!(st, 404);
    let (st, _) = send("GET", "/mcp", String::new()).await;
    assert_eq!(st, 405);
    let (st, _) = send("PUT", "/mcp", String::new()).await;
    assert_eq!(st, 405);
}
