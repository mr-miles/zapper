//! A decider a client (the office) serves on a socket in `state/services/`
//! (`POST /v1/deciders`). On an `ask` the core connects, sends one line
//! naming the caller from the VM the guest's connection came from, and reads
//! the referral within a short timeout; no answer, or an error, refuses the
//! request as a denial. An `allowed` note (a rule the decider set was used)
//! is sent best-effort, off the guest request's path.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use futures_util::future::BoxFuture;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use zapper_policy::facility::{AskCall, AskMethod, AskReply};
use zapper_policy::proto::MAX_MCP_LINE;
use zapper_policy::NetRequest;

use super::{Decider, Referral};
use crate::daemon::session::Session;

/// How long the decider has to refer an `ask`: short, since the guest's
/// request waits on it (and on no answer is refused as a denial).
const ASK_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) struct SocketDecider {
    path: PathBuf,
    timeout: Duration,
}

impl SocketDecider {
    pub(crate) fn new(path: PathBuf) -> Self {
        SocketDecider {
            path,
            timeout: ASK_TIMEOUT,
        }
    }

    /// Sends one call line and reads one reply line, within the timeout.
    async fn exchange(&self, call: &AskCall) -> Result<AskReply> {
        let asked = async {
            let mut sock = tokio::net::UnixStream::connect(&self.path)
                .await
                .with_context(|| "the decider's socket")?;
            let mut line = serde_json::to_vec(call)?;
            line.push(b'\n');
            sock.write_all(&line).await?;
            let mut reply = Vec::new();
            let n = BufReader::new(sock)
                .take(MAX_MCP_LINE as u64 + 1)
                .read_until(b'\n', &mut reply)
                .await?;
            if n == 0 {
                bail!("the decider closed without answering");
            }
            if reply.len() > MAX_MCP_LINE {
                bail!("the decider's answer is longer than {MAX_MCP_LINE} bytes");
            }
            serde_json::from_slice::<AskReply>(&reply)
                .map_err(|e| anyhow!("the decider's answer: {e}"))
        };
        tokio::time::timeout(self.timeout, asked)
            .await
            .map_err(|_| {
                anyhow!(
                    "the decider didn't answer within {}s",
                    self.timeout.as_secs()
                )
            })?
    }
}

fn call_of(
    s: &Session,
    method: AskMethod,
    req: Option<&NetRequest>,
    rule: Option<String>,
) -> AskCall {
    AskCall {
        session: s.id.clone(),
        labels: s.labels.clone(),
        method,
        host: req.map(|r| r.host.clone()).unwrap_or_default(),
        port: req.map(|r| r.port).unwrap_or_default(),
        http_method: req.and_then(|r| r.method.clone()),
        path: req.and_then(|r| r.path.clone()),
        rule,
    }
}

impl Decider for SocketDecider {
    fn ask<'a>(&'a self, s: &'a Session, req: &'a NetRequest) -> BoxFuture<'a, Result<Referral>> {
        Box::pin(async move {
            let call = call_of(s, AskMethod::Ask, Some(req), None);
            let reply = self.exchange(&call).await?;
            match (reply.referral, reply.error) {
                (_, Some(e)) => bail!("{e}"),
                (Some(r), None) => Ok(Referral {
                    id: r.id,
                    to: r.to,
                    then: r.then,
                }),
                (None, None) => bail!("the decider gave no referral"),
            }
        })
    }

    fn allowed<'a>(&'a self, s: &'a Session, rule: &'a str) -> BoxFuture<'a, ()> {
        // Off the guest request's path: the once-only note is best-effort,
        // sent from a task of its own so a slow decider never holds a
        // request its rule already allowed.
        let call = call_of(s, AskMethod::Allowed, None, Some(rule.to_string()));
        let path = self.path.clone();
        let timeout = self.timeout;
        tokio::spawn(async move {
            let send = async {
                let mut sock = tokio::net::UnixStream::connect(&path).await?;
                let mut line = serde_json::to_vec(&call)?;
                line.push(b'\n');
                sock.write_all(&line).await?;
                Ok::<(), anyhow::Error>(())
            };
            let _ = tokio::time::timeout(timeout, send).await;
        });
        Box::pin(async {})
    }
}
