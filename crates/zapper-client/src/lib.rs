//! A client of zapper's API over its unix socket. The workbench and the
//! office are both clients of the daemon (proposal 13, §7); this is the
//! client they share — `Zapper` (calls, streams, upgrades), where the socket
//! is, and `url_encode` for query values.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::{HeaderMap, Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::Value;
use tokio::net::UnixStream;

/// Where zapper listens: `$ZAPPER_SOCKET`, else under `$XDG_RUNTIME_DIR`, as
/// the `zapper` CLI finds it.
pub fn socket_path() -> PathBuf {
    if let Ok(p) = std::env::var("ZAPPER_SOCKET") {
        return p.into();
    }
    let base = std::env::var("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(format!("/tmp/zapper-{}", unsafe { libc::getuid() })));
    base.join("zapper").join("api.sock")
}

/// A client of zapper's API.
#[derive(Debug, Clone)]
pub struct Zapper {
    socket: PathBuf,
}

/// What zapper answered: its status and its JSON (`null` for none).
pub struct Answer {
    pub status: StatusCode,
    pub body: Value,
}

impl Answer {
    /// The JSON, if zapper said yes; its error, with its status, if not.
    pub fn ok(self) -> std::result::Result<Value, (StatusCode, String)> {
        if self.status.is_success() {
            return Ok(self.body);
        }
        let why = self.body["error"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| self.body.to_string());
        Err((self.status, why))
    }
}

impl Zapper {
    pub fn new(socket: PathBuf) -> Self {
        Zapper { socket }
    }

    /// Sends `req`, on a connection of its own (which an upgrade keeps).
    async fn send(&self, req: Request<Full<Bytes>>) -> Result<Response<Incoming>> {
        let stream = UnixStream::connect(&self.socket).await.with_context(|| {
            format!(
                "zapper at {} (is `zapper serve` running?)",
                self.socket.display()
            )
        })?;
        let (mut sender, conn) =
            hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
        tokio::spawn(conn.with_upgrades());
        Ok(sender.send_request(req).await?)
    }

    fn request(
        method: Method,
        path: &str,
        headers: &HeaderMap,
        body: Bytes,
    ) -> Result<Request<Full<Bytes>>> {
        let mut b = Request::builder()
            .method(method)
            .uri(format!("http://zapper{path}"))
            .header("host", "zapper");
        for (k, v) in headers {
            b = b.header(k, v);
        }
        Ok(b.body(Full::new(body))?)
    }

    /// `method path` with an optional JSON `body`, and what zapper answered.
    pub async fn call(&self, method: Method, path: &str, body: Option<&Value>) -> Result<Answer> {
        let mut headers = HeaderMap::new();
        let body = match body {
            Some(b) => {
                headers.insert("content-type", "application/json".parse()?);
                Bytes::from(serde_json::to_vec(b)?)
            }
            None => Bytes::new(),
        };
        let resp = self
            .send(Self::request(method, path, &headers, body)?)
            .await?;
        let status = resp.status();
        let bytes = resp.into_body().collect().await?.to_bytes();
        let body = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
        };
        Ok(Answer { status, body })
    }

    /// `method path` with a raw body and its content-type, returning the
    /// status and the response bytes as they came — for a non-JSON request
    /// body (YAML) or a binary response (a drive download) that [`call`] would
    /// lossily parse as JSON. [`call`] is this for a JSON round-trip.
    ///
    /// [`call`]: Self::call
    pub async fn call_raw(
        &self,
        method: Method,
        path: &str,
        content_type: Option<&str>,
        body: Bytes,
    ) -> Result<(StatusCode, Bytes)> {
        let mut headers = HeaderMap::new();
        if let Some(ct) = content_type {
            headers.insert("content-type", ct.parse()?);
        }
        let resp = self
            .send(Self::request(method, path, &headers, body)?)
            .await?;
        let status = resp.status();
        let bytes = resp.into_body().collect().await?.to_bytes();
        Ok((status, bytes))
    }

    /// A GET, returning what zapper answered.
    pub async fn get(&self, path: &str) -> Result<Answer> {
        self.call(Method::GET, path, None).await
    }

    /// `GET path` as it comes: for a body that streams (server-sent events,
    /// an audit log), passed on as zapper sends it.
    pub async fn stream(&self, path: &str, headers: &HeaderMap) -> Result<Response<Incoming>> {
        self.send(Self::request(Method::GET, path, headers, Bytes::new())?)
            .await
    }

    /// `GET path` upgraded to a raw stream (a terminal, or a guest port): the
    /// daemon splices it to the guest. The stream, and the terminal's number
    /// if zapper gave one.
    pub async fn upgrade(
        &self,
        path: &str,
    ) -> Result<(TokioIo<hyper::upgrade::Upgraded>, Option<String>)> {
        let mut headers = HeaderMap::new();
        headers.insert("connection", "upgrade".parse()?);
        headers.insert("upgrade", "zapper-shell".parse()?);
        let resp = self
            .send(Self::request(Method::GET, path, &headers, Bytes::new())?)
            .await?;
        if resp.status() != StatusCode::SWITCHING_PROTOCOLS {
            let status = resp.status();
            let bytes = resp.into_body().collect().await?.to_bytes();
            let why = serde_json::from_slice::<Value>(&bytes)
                .ok()
                .and_then(|v| v["error"].as_str().map(str::to_string))
                .unwrap_or_else(|| String::from_utf8_lossy(&bytes).into_owned());
            bail!("{status}: {why}");
        }
        let n = resp
            .headers()
            .get("x-zapper-terminal")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        Ok((TokioIo::new(hyper::upgrade::on(resp).await?), n))
    }
}

/// The largest first line a facility socket accepts from the daemon (the
/// caller line / MCP or ask request). The daemon sizes what it sends; this is
/// only a safety bound.
const MAX_LINE: usize = 1 << 20;

/// Reads the first line the daemon sends on a facility socket connection — a
/// byte at a time, so whatever follows (a guest's TLS-terminated stream) stays
/// in `stream` for the handler. `None` on a short or over-long line.
pub async fn read_line(stream: &mut UnixStream) -> Option<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    let mut line = Vec::new();
    let mut b = [0u8; 1];
    while line.len() < MAX_LINE {
        match stream.read(&mut b).await {
            Ok(1) if b[0] == b'\n' => return Some(line),
            Ok(1) => line.push(b[0]),
            _ => return None,
        }
    }
    None
}

/// Serves a facility's socket (the office's tools, decider, git server; the
/// workbench's facilities): for each connection, reads the daemon's first line
/// (the caller line, or an MCP/ask request) and hands `(line, stream)` to
/// `handler`, which parses the line and either replies on the stream or speaks
/// HTTP over it. Folds the accept loop all facilities repeat. The listener is
/// bound by the caller (under the daemon's `state/services/`); this converts
/// and serves it.
pub async fn serve_socket<H, Fut>(listener: std::os::unix::net::UnixListener, handler: H)
where
    H: Fn(Vec<u8>, UnixStream) -> Fut + Clone + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let Ok(listener) = tokio::net::UnixListener::from_std(listener) else {
        return;
    };
    while let Ok((mut stream, _)) = listener.accept().await {
        let handler = handler.clone();
        tokio::spawn(async move {
            if let Some(line) = read_line(&mut stream).await {
                handler(line, stream).await;
            }
        });
    }
}

/// The browser-facing gateway's shared checks: a client that serves a UI on
/// its own listener (the workbench, and the office once it is its own
/// process) authenticates a browser the same way zapper's own UI does — a
/// `Host` on loopback (defeating DNS rebinding), and, for a write, the UI's
/// marker header and `Origin`. These are the generic pieces, parameterised by
/// the client's port, whether it listens off-loopback, and its marker header
/// and cookie names.
pub mod gateway {
    use hyper::{header, HeaderMap, Method, StatusCode};

    /// The host names a loopback listener answers to on `port`.
    pub fn local_hosts(port: u16) -> [String; 3] {
        [
            format!("127.0.0.1:{port}"),
            format!("localhost:{port}"),
            format!("[::1]:{port}"),
        ]
    }

    /// Whether a request's `Host` may reach the UI: a local name on loopback,
    /// which defeats DNS rebinding; listening elsewhere, any well-formed host.
    pub fn host_ok(host: &str, port: u16, remote: bool) -> bool {
        if remote {
            !host.is_empty()
                && host.len() <= 300
                && host
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b".-:[]".contains(&b))
        } else {
            local_hosts(port).iter().any(|l| l == host)
        }
    }

    /// Whether a write's `Origin` is the UI itself.
    pub fn origin_ok(origin: &str, host: &str, port: u16, remote: bool) -> bool {
        if remote {
            origin == format!("http://{host}") || origin == format!("https://{host}")
        } else {
            local_hosts(port)
                .iter()
                .any(|l| origin == format!("http://{l}"))
        }
    }

    /// A browser's request reaches a UI only by a name it may have
    /// (`host_ok`); one that changes something only with the `marker` header,
    /// which another site's page can't send without a preflight that is never
    /// granted, and an `Origin` naming the UI (`origin_ok`). The caller maps
    /// the `(status, message)` to its own error type.
    pub fn from_the_ui(
        method: &Method,
        headers: &HeaderMap,
        port: u16,
        remote: bool,
        marker: &str,
    ) -> Result<(), (StatusCode, String)> {
        let host = headers
            .get(header::HOST)
            .and_then(|h| h.to_str().ok())
            .unwrap_or("");
        if !host_ok(host, port, remote) {
            return Err((StatusCode::FORBIDDEN, "bad Host header".into()));
        }
        if !matches!(*method, Method::GET | Method::HEAD) {
            let origin = headers
                .get(header::ORIGIN)
                .and_then(|h| h.to_str().ok())
                .unwrap_or("");
            let marked = headers.get(marker).is_some_and(|v| v == "1");
            if !marked || !origin_ok(origin, host, port, remote) {
                return Err((
                    StatusCode::FORBIDDEN,
                    format!("missing {marker} or the UI's Origin"),
                ));
            }
        }
        Ok(())
    }

    /// The `Set-Cookie` value that grants the browser `token` at sign-in.
    pub fn set_cookie(name: &str, token: &str) -> String {
        format!("{name}={token}; HttpOnly; SameSite=Strict; Path=/")
    }

    /// The `Set-Cookie` value that signs the browser out.
    pub fn clear_cookie(name: &str) -> String {
        format!("{name}=; Max-Age=0; HttpOnly; SameSite=Strict; Path=/")
    }

    /// The token in the browser's `cookie` header named `name`, if any.
    pub fn cookie_token(headers: &HeaderMap, name: &str) -> Option<String> {
        headers
            .get_all(header::COOKIE)
            .iter()
            .filter_map(|c| c.to_str().ok())
            .flat_map(|c| c.split(';'))
            .find_map(|c| {
                c.trim()
                    .strip_prefix(&format!("{name}="))
                    .map(str::to_string)
            })
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn host_and_origin() {
            assert!(host_ok("127.0.0.1:7880", 7880, false));
            assert!(host_ok("localhost:7880", 7880, false));
            assert!(!host_ok("evil.example:7880", 7880, false), "DNS rebinding");
            assert!(host_ok("anything.example", 7880, true), "off-loopback");
            assert!(origin_ok(
                "http://127.0.0.1:7880",
                "127.0.0.1:7880",
                7880,
                false
            ));
            assert!(!origin_ok(
                "http://evil.example",
                "127.0.0.1:7880",
                7880,
                false
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::Full;
    use hyper::service::service_fn;
    use hyper::Response;
    use std::convert::Infallible;

    /// A tiny unix-socket HTTP server answering three canned routes, so the
    /// client's JSON round-trip, raw-bytes call and error extraction are
    /// exercised without a real daemon.
    async fn serve(sock: std::path::PathBuf) {
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let io = TokioIo::new(stream);
                    let svc = service_fn(|req: Request<Incoming>| async move {
                        let resp = match req.uri().path() {
                            "/ok" => Response::builder()
                                .header("content-type", "application/json")
                                .body(Full::new(Bytes::from(r#"{"hello":"world"}"#)))
                                .unwrap(),
                            "/raw" => Response::builder()
                                .body(Full::new(Bytes::from(vec![0u8, 1, 2])))
                                .unwrap(),
                            _ => Response::builder()
                                .status(StatusCode::BAD_REQUEST)
                                .body(Full::new(Bytes::from(r#"{"error":"nope"}"#)))
                                .unwrap(),
                        };
                        Ok::<_, Infallible>(resp)
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(io, svc)
                        .await;
                });
            }
        });
    }

    #[tokio::test]
    async fn round_trips_json_raw_bytes_and_errors() {
        let sock = std::env::temp_dir().join(format!(
            "zc-{}-{}.sock",
            std::process::id(),
            crate::url_encode("t")
        ));
        let _ = std::fs::remove_file(&sock);
        serve(sock.clone()).await;
        let z = Zapper::new(sock.clone());

        // A JSON GET: the parsed body comes back and `ok()` yields it.
        let a = z.get("/ok").await.unwrap();
        assert_eq!(a.status, StatusCode::OK);
        assert_eq!(a.ok().unwrap()["hello"], "world");

        // A raw call keeps the bytes as they came (a binary response).
        let (st, body) = z
            .call_raw(Method::GET, "/raw", None, Bytes::new())
            .await
            .unwrap();
        assert_eq!(st, StatusCode::OK);
        assert_eq!(&body[..], &[0u8, 1, 2]);

        // A non-2xx: `ok()` surfaces the status and the `error` field.
        let err = z.get("/bad").await.unwrap().ok().unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert_eq!(err.1, "nope");

        let _ = std::fs::remove_file(&sock);
    }
}

/// `s` as a URL's query value (percent-encoded).
pub fn url_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            b => format!("%{b:02X}"),
        })
        .collect()
}
