//! The warden for the tests: sandboxes in memory, and each agent the office
//! starts handed to the test to play, as the frames the harness speaks.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

use anyhow::{bail, Result};
use futures_util::future::BoxFuture;
use tokio::io::{AsyncWriteExt, DuplexStream, ReadHalf, WriteHalf};
use tokio::sync::mpsc;
use zapper_driver::process::DriveStream;
use zapper_policy::proto::Frame;

use crate::workers::{Sandbox, Workers};

/// An agent the office started, as the test playing it sees it.
pub struct Agent {
    pub name: String,
    pub resume: Option<String>,
    read: ReadHalf<DuplexStream>,
    write: WriteHalf<DuplexStream>,
    buf: Vec<u8>,
}

impl Agent {
    /// The next line of its stdin; none once the office closed it.
    pub async fn line(&mut self) -> Option<String> {
        loop {
            if let Some(i) = self.buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = self.buf.drain(..=i).collect();
                return Some(String::from_utf8_lossy(&line[..i]).into_owned());
            }
            match zapper_driver::read_frame(&mut self.read).await {
                Ok(Some(Frame::Data(d))) => self.buf.extend(d),
                Ok(Some(_)) => {}
                _ => return None,
            }
        }
    }

    /// Writes `line` to its stdout.
    pub async fn say(&mut self, line: &str) {
        let f = Frame::Data(format!("{line}\n").into_bytes());
        let _ = self.write.write_all(&f.encode()).await;
    }

    /// The agent exits with `code`.
    pub async fn exit(mut self, code: i32) {
        let _ = self.write.write_all(&Frame::Exit(code).encode()).await;
    }
}

pub struct FakeWarden {
    agents: mpsc::UnboundedSender<Agent>,
    pub made: Mutex<Vec<String>>,
    /// `ensure` fails this many more times.
    pub fail_ensure: AtomicU32,
    /// The harness its sandboxes run.
    pub kind: Mutex<String>,
}

impl FakeWarden {
    pub fn new() -> (FakeWarden, mpsc::UnboundedReceiver<Agent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let w = FakeWarden {
            agents: tx,
            made: Mutex::new(vec![]),
            fail_ensure: AtomicU32::new(0),
            kind: Mutex::new("claude-code".into()),
        };
        (w, rx)
    }
}

pub fn sandbox_of(name: &str, profile: &str) -> Sandbox {
    Sandbox {
        name: name.to_string(),
        profile: profile.to_string(),
        sandbox: format!("zcw-{name}"),
        id: format!("id-{name}"),
        ready: true,
        kind: "claude-code".to_string(),
    }
}

impl Workers for FakeWarden {
    fn ensure<'a>(&'a self, name: &'a str, profile: &'a str) -> BoxFuture<'a, Result<Sandbox>> {
        Box::pin(async move {
            let left = self.fail_ensure.load(Ordering::Relaxed);
            if left > 0 {
                self.fail_ensure.store(left - 1, Ordering::Relaxed);
                bail!("the warden: 429: the most it makes");
            }
            let mut made = self.made.lock().unwrap();
            if !made.iter().any(|m| m == name) {
                made.push(name.to_string());
            }
            let mut s = sandbox_of(name, profile);
            s.kind = self.kind.lock().unwrap().clone();
            Ok(s)
        })
    }

    fn delete<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.made.lock().unwrap().retain(|m| m != name);
            Ok(())
        })
    }

    fn list(&self) -> BoxFuture<'_, Result<Vec<Sandbox>>> {
        Box::pin(async move {
            Ok(self
                .made
                .lock()
                .unwrap()
                .iter()
                .map(|n| sandbox_of(n, "claude"))
                .collect())
        })
    }

    fn agent<'a>(
        &'a self,
        name: &'a str,
        resume: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Box<dyn DriveStream>>> {
        Box::pin(async move {
            let (ours, theirs) = tokio::io::duplex(256 * 1024);
            let (read, write) = tokio::io::split(theirs);
            let agent = Agent {
                name: name.to_string(),
                resume: resume.map(str::to_string),
                read,
                write,
                buf: vec![],
            };
            if self.agents.send(agent).is_err() {
                bail!("nobody plays {name}'s agent");
            }
            Ok(Box::new(ours) as Box<dyn DriveStream>)
        })
    }
}
