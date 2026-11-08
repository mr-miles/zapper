//! The workers' sandboxes, as the office reaches them: only through the
//! warden, which makes them from fixed templates and drives their agents.
//! In place of the office's `OfficeClient`, four calls.
//!
//! A worker's agent comes back as a connection to the office's agent port:
//! the office hands the warden a one-time ticket, the warden runs the
//! profile's headless harness in the worker's sandbox, opens a forward to
//! the agent port and says `ZCAGENT <ticket>` first; the office matches the
//! ticket and drives the stream from there.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use futures_util::future::BoxFuture;
use hyper::Method;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use zapper_driver::process::DriveStream;

/// A worker's sandbox, as the warden lists it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sandbox {
    pub name: String,
    pub profile: String,
    /// The sandbox's name at the gateway.
    pub sandbox: String,
    /// Its id at the gateway: who the edge's tokens name.
    pub id: String,
    pub ready: bool,
    /// Its profile's harness (`claude-code`, `pi`), as the warden runs it.
    pub kind: String,
}

/// The office's four calls on the warden.
pub trait Workers: Send + Sync {
    /// Worker `name`'s sandbox of `profile`, made if it isn't there.
    fn ensure<'a>(&'a self, name: &'a str, profile: &'a str) -> BoxFuture<'a, Result<Sandbox>>;
    fn delete<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<()>>;
    fn list(&self) -> BoxFuture<'_, Result<Vec<Sandbox>>>;
    /// Worker `name`'s agent, resuming conversation `resume` if given: its
    /// stdin and stdout as the harness's frames.
    fn agent<'a>(
        &'a self,
        name: &'a str,
        resume: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Box<dyn DriveStream>>>;
}

/// How long the warden has to connect an agent once it said it would.
const AGENT_CONNECT: Duration = Duration::from_secs(30);
/// How long a connection to the agent port has to say its ticket.
const GREETING: Duration = Duration::from_secs(10);
/// The greeting's longest line: `ZCAGENT ` and 128 hex digits.
const MAX_GREETING: usize = 8 + 128 + 1;

/// Agents the office is waiting for, by ticket.
#[derive(Default, Clone)]
pub struct Tickets(Arc<Mutex<HashMap<String, oneshot::Sender<TcpStream>>>>);

impl Tickets {
    /// A new ticket, and where its connection will come.
    pub fn issue(&self) -> (String, oneshot::Receiver<TcpStream>) {
        let ticket: String = rand::random::<[u8; 32]>()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let (tx, rx) = oneshot::channel();
        self.0.lock().unwrap().insert(ticket.clone(), tx);
        (ticket, rx)
    }

    pub fn cancel(&self, ticket: &str) {
        self.0.lock().unwrap().remove(ticket);
    }

    /// Hands `stream` to whoever holds `ticket`: false if nobody does.
    fn redeem(&self, ticket: &str, stream: TcpStream) -> bool {
        match self.0.lock().unwrap().remove(ticket) {
            Some(tx) => tx.send(stream).is_ok(),
            None => false,
        }
    }

    pub fn pending(&self) -> usize {
        self.0.lock().unwrap().len()
    }

    /// Serves the agent port: each connection says its ticket and goes to
    /// the agent waiting for it, or is closed.
    pub async fn serve(self, listener: TcpListener) {
        loop {
            let (mut stream, _) = match listener.accept().await {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("agent port: accepting: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            let tickets = self.clone();
            tokio::spawn(async move {
                match tokio::time::timeout(GREETING, greeting(&mut stream)).await {
                    Ok(Ok(ticket)) => {
                        if !tickets.redeem(&ticket, stream) {
                            tracing::warn!("agent port: a connection with an unknown ticket");
                        }
                    }
                    Ok(Err(e)) => tracing::warn!("agent port: {e:#}"),
                    Err(_) => tracing::warn!("agent port: no greeting in time"),
                }
            });
        }
    }
}

/// Reads `ZCAGENT <ticket>\n` a byte at a time, so nothing after it (the
/// agent's first frames) is taken from the stream.
async fn greeting(stream: &mut TcpStream) -> Result<String> {
    let mut line = Vec::new();
    loop {
        let b = stream.read_u8().await.context("reading the greeting")?;
        if b == b'\n' {
            break;
        }
        line.push(b);
        if line.len() > MAX_GREETING {
            bail!("the greeting is too long");
        }
    }
    let line = String::from_utf8(line).map_err(|_| anyhow!("the greeting isn't text"))?;
    let ticket = line
        .strip_prefix("ZCAGENT ")
        .ok_or_else(|| anyhow!("not an agent's greeting"))?;
    if ticket.len() < 32 || !ticket.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')) {
        bail!("the greeting's ticket isn't hex");
    }
    Ok(ticket.to_string())
}

/// The warden, over HTTP from the office's sandbox: its supervisor adds the
/// office's token on the way out.
pub struct Warden {
    url: String,
    tickets: Tickets,
}

impl Warden {
    pub fn new(url: &str, tickets: Tickets) -> Warden {
        Warden {
            url: url.trim_end_matches('/').to_string(),
            tickets,
        }
    }

    async fn call(
        &self,
        method: Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<serde_json::Value> {
        let url = format!("{}{path}", self.url);
        let body = body.map(|b| b.to_string().into_bytes());
        let (status, reply) = zapper_openshell::edge::http_request(method, &url, body).await?;
        let v: serde_json::Value = serde_json::from_slice(&reply).unwrap_or_else(
            |_| json!({ "error": String::from_utf8_lossy(&reply).trim().to_string() }),
        );
        if !(200..300).contains(&status) {
            let why = v["error"].as_str().unwrap_or("no reason given");
            bail!("the warden: {status}: {why}");
        }
        Ok(v)
    }

    /// The driver's JWKS, as the warden passes it on.
    pub fn jwks_url(&self) -> String {
        format!("{}/jwks", self.url)
    }
}

impl Workers for Warden {
    fn ensure<'a>(&'a self, name: &'a str, profile: &'a str) -> BoxFuture<'a, Result<Sandbox>> {
        Box::pin(async move {
            let v = self
                .call(
                    Method::PUT,
                    &format!("/workers/{name}"),
                    Some(json!({ "profile": profile })),
                )
                .await?;
            serde_json::from_value(v["worker"].clone()).context("the warden's answer")
        })
    }

    fn delete<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.call(Method::DELETE, &format!("/workers/{name}"), None)
                .await
                .map(|_| ())
        })
    }

    fn list(&self) -> BoxFuture<'_, Result<Vec<Sandbox>>> {
        Box::pin(async move {
            let v = self.call(Method::GET, "/workers", None).await?;
            let list = v.get("workers").cloned().unwrap_or(v);
            serde_json::from_value(list).context("the warden's list")
        })
    }

    fn agent<'a>(
        &'a self,
        name: &'a str,
        resume: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Box<dyn DriveStream>>> {
        Box::pin(async move {
            let (ticket, rx) = self.tickets.issue();
            let mut body = json!({ "ticket": ticket });
            if let Some(r) = resume {
                body["resume"] = json!(r);
            }
            let asked = self
                .call(Method::POST, &format!("/workers/{name}/agent"), Some(body))
                .await;
            if let Err(e) = asked {
                self.tickets.cancel(&ticket);
                return Err(e);
            }
            match tokio::time::timeout(AGENT_CONNECT, rx).await {
                Ok(Ok(stream)) => Ok(Box::new(stream) as Box<dyn DriveStream>),
                _ => {
                    self.tickets.cancel(&ticket);
                    bail!("{name}'s agent didn't reach the agent port")
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn port(tickets: &Tickets) -> std::net::SocketAddr {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(tickets.clone().serve(l));
        addr
    }

    #[tokio::test]
    async fn a_connection_with_its_ticket_reaches_its_agent_and_nothing_after_is_lost() {
        let tickets = Tickets::default();
        let addr = port(&tickets).await;
        let (ticket, rx) = tickets.issue();
        let mut c = TcpStream::connect(addr).await.unwrap();
        c.write_all(format!("ZCAGENT {ticket}\nframes").as_bytes())
            .await
            .unwrap();
        let mut s = tokio::time::timeout(Duration::from_secs(5), rx)
            .await
            .unwrap()
            .unwrap();
        let mut buf = [0u8; 6];
        s.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"frames");
        assert_eq!(tickets.pending(), 0, "a ticket is used once");
    }

    #[tokio::test]
    async fn a_connection_without_a_good_ticket_is_closed() {
        let tickets = Tickets::default();
        let addr = port(&tickets).await;
        let (_ticket, _rx) = tickets.issue();
        for greeting in [
            format!("ZCAGENT {}\n", "ab".repeat(32)),
            "GET / HTTP/1.1\r\n".to_string(),
            format!("ZCAGENT {}\n", "a".repeat(200)),
        ] {
            let mut c = TcpStream::connect(addr).await.unwrap();
            c.write_all(greeting.as_bytes()).await.unwrap();
            let mut buf = Vec::new();
            let n = tokio::time::timeout(Duration::from_secs(5), c.read_to_end(&mut buf))
                .await
                .unwrap();
            assert!(n.is_err() || buf.is_empty(), "{greeting:?}");
        }
        assert_eq!(tickets.pending(), 1, "the real ticket is still waiting");
    }

    #[test]
    fn tickets_are_long_and_hex() {
        let t = Tickets::default();
        let (a, _ra) = t.issue();
        let (b, _rb) = t.issue();
        assert_ne!(a, b);
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        t.cancel(&a);
        assert_eq!(t.pending(), 1);
    }
}
