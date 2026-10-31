//! An MCP server served by a facility on a socket in `state/services/`
//! (`POST /v1/mcp`). The gateway stays the guest's MCP endpoint, with its
//! bounds and its `mcp` events; for each `tools/list` and `tools/call` it
//! asks the facility, one line each way on a connection of its own, and
//! names the caller itself: the session the guest's connection came from.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use futures_util::future::BoxFuture;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use zapper_policy::facility::{McpCall, McpMethod, McpReply};
use zapper_policy::proto::MAX_MCP_LINE;

use super::McpServer;
use crate::daemon::session::Session;

/// How long a facility has to answer: a tool may do real work (copy files,
/// hire someone), listing its tools shouldn't.
const LIST_TIMEOUT: Duration = Duration::from_secs(10);
const CALL_TIMEOUT: Duration = Duration::from_secs(120);

pub(crate) struct SocketServer {
    name: String,
    path: PathBuf,
    instructions: Option<String>,
    list_timeout: Duration,
    call_timeout: Duration,
}

impl SocketServer {
    pub(crate) fn new(name: &str, path: PathBuf, instructions: Option<String>) -> Self {
        SocketServer {
            name: name.to_string(),
            path,
            instructions,
            list_timeout: LIST_TIMEOUT,
            call_timeout: CALL_TIMEOUT,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_timeouts(mut self, list: Duration, call: Duration) -> Self {
        (self.list_timeout, self.call_timeout) = (list, call);
        self
    }

    /// Asks the facility `method` for `s`, and its result.
    async fn ask(
        &self,
        s: &Session,
        method: McpMethod,
        tool: Option<&str>,
        arguments: Option<Value>,
    ) -> Result<Value> {
        let call = McpCall {
            session: s.id.clone(),
            labels: s.labels.clone(),
            server: self.name.clone(),
            method,
            tool: tool.map(str::to_string),
            arguments,
        };
        let timeout = match method {
            McpMethod::ToolsList => self.list_timeout,
            McpMethod::ToolsCall => self.call_timeout,
        };
        let asked = async {
            let mut sock = tokio::net::UnixStream::connect(&self.path)
                .await
                .with_context(|| format!("MCP server {}'s socket", self.name))?;
            let mut line = serde_json::to_vec(&call)?;
            line.push(b'\n');
            sock.write_all(&line).await?;
            let mut reply = Vec::new();
            let n = BufReader::new(sock)
                .take(MAX_MCP_LINE as u64 + 1)
                .read_until(b'\n', &mut reply)
                .await?;
            if n == 0 {
                bail!("MCP server {} closed without answering", self.name);
            }
            if reply.len() > MAX_MCP_LINE {
                bail!(
                    "MCP server {}'s answer is longer than {MAX_MCP_LINE} bytes",
                    self.name
                );
            }
            let reply: McpReply = serde_json::from_slice(&reply)
                .map_err(|e| anyhow!("MCP server {}'s answer: {e}", self.name))?;
            match (reply.result, reply.error) {
                (_, Some(e)) => Err(anyhow!("{e}")),
                (Some(r), None) => Ok(r),
                (None, None) => Ok(Value::Null),
            }
        };
        tokio::time::timeout(timeout, asked).await.map_err(|_| {
            anyhow!(
                "MCP server {} didn't answer within {}s",
                self.name,
                timeout.as_secs()
            )
        })?
    }
}

impl McpServer for SocketServer {
    fn instructions(&self) -> Option<String> {
        self.instructions.clone()
    }

    fn tools<'a>(&'a self, s: &'a Arc<Session>) -> BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            let tools = self.ask(s, McpMethod::ToolsList, None, None).await?;
            if !tools.is_array() {
                bail!("MCP server {}'s tools aren't a list", self.name);
            }
            Ok(tools)
        })
    }

    fn call<'a>(
        &'a self,
        s: &'a Arc<Session>,
        tool: &'a str,
        args: Value,
    ) -> BoxFuture<'a, Result<Value>> {
        Box::pin(self.ask(s, McpMethod::ToolsCall, Some(tool), Some(args)))
    }
}
