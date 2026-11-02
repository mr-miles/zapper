//! The process guard: one run of a session's agent over a terminal channel.
//! `start` spawns the reader that frames the agent's output into [`Out`]
//! events and reports its transcript and activity through an [`Observer`];
//! [`Proc`] is the handle the turn loop writes prompts to, and dropping it
//! closes the connection so the agent hangs up. The office's foreman drives
//! its workers over the same `start` and `Proc`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use zapper_policy::proto::{Frame, MAX_LINE};

use crate::harness::{Conversation, Event, TurnEnd};
use crate::read_frame;

/// A terminal stream the driver runs an agent over: any `AsyncRead +
/// AsyncWrite` the office can hand [`start`]. A combined trait so a call site
/// that can't name the concrete type (the office's `attach`, whose stream is
/// a local `UnixStream` in-process and an upgraded API connection over the
/// socket) returns it as `Box<dyn DriveStream>`. The blanket impl covers
/// every concrete stream, and `Box<dyn DriveStream>` is itself one (tokio's
/// `AsyncRead`/`AsyncWrite` for `Box`), so it feeds straight into `start`.
pub trait DriveStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + ?Sized> DriveStream for T {}

pub enum Out {
    /// A turn ended.
    End(TurnEnd),
    /// The process exited.
    Exit(Option<i32>),
    /// The turn ran past the turn timeout.
    Late,
}

/// Where a driven agent's output goes: its transcript and what it is doing.
pub trait Observer: Send + Sync {
    /// An output line the conversation transcribes: a JSON object.
    fn line(&self, line: Value);
    /// Anything else the transcript notes: stderr, text that isn't JSON, a
    /// line cut short (`kind` says which).
    fn note(&self, kind: &str, detail: Value);
    /// The conversation's id, for resuming it.
    fn conversation(&self, id: &str);
    /// What the agent is doing now.
    fn activity(&self, text: &str);
}

pub struct Proc {
    stdin: Box<dyn AsyncWrite + Unpin + Send>,
    events: mpsc::Receiver<Out>,
    conv: Arc<std::sync::Mutex<Box<dyn Conversation>>>,
    /// The harness takes messages while the agent works.
    steers: bool,
    /// Reads the agent's output; it holds the connection's other half.
    reader: tokio::task::JoinHandle<()>,
}

impl Drop for Proc {
    /// Closes the connection both ways: the agent hangs the process up.
    fn drop(&mut self) {
        self.reader.abort();
    }
}

impl Proc {
    async fn write(&mut self, line: Vec<u8>) -> Result<()> {
        self.stdin.write_all(&Frame::Data(line).encode()).await?;
        Ok(())
    }

    async fn send(&mut self, text: &str) -> Result<()> {
        let line = self.conv.lock().unwrap().prompt(text);
        self.write(line).await
    }

    /// One turn: `prompt` sent, then what ends it, the turn's end, the
    /// process's, or `deadline` passing (`Out::Late`). While it runs, each
    /// `wake` of a harness that steers asks `more` for what came meanwhile
    /// and gives it to the agent. `None` if `stopping` came first. The
    /// driver's and the office's foreman's turn.
    pub async fn turn(
        &mut self,
        prompt: &str,
        deadline: tokio::time::Instant,
        wake: &tokio::sync::Notify,
        stopping: &tokio::sync::Notify,
        mut more: impl FnMut() -> Option<String>,
    ) -> Option<Out> {
        if self.send(prompt).await.is_err() {
            return Some(Out::Exit(None));
        }
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => return Some(Out::Late),
                o = self.events.recv() => return Some(o.unwrap_or(Out::Exit(None))),
                _ = wake.notified(), if self.steers => {
                    let Some(text) = more() else { continue };
                    let line = self.conv.lock().unwrap().steer(&text);
                    if let Some(line) = line {
                        if self.write(line).await.is_err() {
                            return Some(Out::Exit(None));
                        }
                    }
                }
                _ = stopping.notified() => return None,
            }
        }
    }
}

pub fn start<S>(stream: S, conv: Box<dyn Conversation>, observer: Arc<dyn Observer>) -> Proc
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let steers = conv.steers();
    let conv = Arc::new(std::sync::Mutex::new(conv));
    let reader = conv.clone();
    let (mut r, stdin) = tokio::io::split(stream);
    let (tx, events) = mpsc::channel(16);
    let reader_task = tokio::spawn(async move {
        let mut buf: Vec<u8> = Vec::new();
        let mut err: Vec<u8> = Vec::new();
        let mut last_activity = Instant::now() - Duration::from_secs(10);
        let mut code = None;
        let line_out = |line: &[u8], buf_err: bool| -> Option<Value> {
            let text = String::from_utf8_lossy(line);
            let text = text.trim();
            if text.is_empty() {
                return None;
            }
            if buf_err {
                observer.note(
                    "stderr",
                    json!({ "text": text.chars().take(4096).collect::<String>() }),
                );
                return None;
            }
            match serde_json::from_str::<Value>(text) {
                Ok(v) if v.is_object() => {
                    if reader.lock().unwrap().transcribe(&v) {
                        observer.line(v.clone());
                    }
                    Some(v)
                }
                _ => {
                    observer.note(
                        "raw",
                        json!({ "text": text.chars().take(4096).collect::<String>() }),
                    );
                    None
                }
            }
        };
        loop {
            let frame = match read_frame(&mut r).await {
                Ok(Some(f)) => f,
                _ => break,
            };
            let (data, is_err) = match frame {
                Frame::Data(d) => (d, false),
                Frame::Stderr(d) => (d, true),
                Frame::Exit(c) => {
                    code = Some(c);
                    break;
                }
                _ => continue,
            };
            let b = if is_err { &mut err } else { &mut buf };
            b.extend_from_slice(&data);
            while let Some(i) = b.iter().position(|c| *c == b'\n') {
                let line: Vec<u8> = b.drain(..=i).collect();
                let Some(v) = line_out(&line, is_err) else {
                    continue;
                };
                let events = reader.lock().unwrap().read(&v);
                for e in events {
                    match e {
                        Event::Session(id) => observer.conversation(&id),
                        Event::Activity(a) => {
                            if last_activity.elapsed() > Duration::from_secs(3) {
                                last_activity = Instant::now();
                                observer.activity(&a);
                            }
                        }
                        Event::TurnEnd(t) => {
                            let _ = tx.send(Out::End(t)).await;
                        }
                    }
                }
            }
            if b.len() > MAX_LINE {
                let line: Vec<u8> = std::mem::take(b);
                observer.note("truncated", json!({ "bytes": line.len() }));
            }
        }
        let _ = tx.send(Out::Exit(code)).await;
    });
    Proc {
        stdin: Box::new(stdin),
        events,
        conv,
        steers,
        reader: reader_task,
    }
}
