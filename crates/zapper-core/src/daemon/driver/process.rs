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
use tokio::io::AsyncWriteExt;
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use zapper_policy::proto::{Frame, MAX_LINE};

use crate::daemon::harness::{Conversation, Event, TurnEnd};
use crate::daemon::session::read_frame;

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
    /// An output line: a JSON object, or text from stderr or not JSON
    /// (`kind` says which).
    fn transcript(&self, kind: &str, line: Value);
    /// The conversation's id, for resuming it.
    fn conversation(&self, id: &str);
    /// What the agent is doing now.
    fn activity(&self, text: &str);
}

pub struct Proc {
    stdin: OwnedWriteHalf,
    pub events: mpsc::Receiver<Out>,
    pub conv: Arc<std::sync::Mutex<Box<dyn Conversation>>>,
    /// The harness takes messages while the agent works.
    pub steers: bool,
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
    pub async fn write(&mut self, line: Vec<u8>) -> Result<()> {
        let mut buf = Vec::new();
        Frame::Data(line).write_to(&mut buf)?;
        self.stdin.write_all(&buf).await?;
        Ok(())
    }

    pub async fn send(&mut self, text: &str) -> Result<()> {
        let line = self.conv.lock().unwrap().prompt(text);
        self.write(line).await
    }
}

pub async fn start(
    stream: UnixStream,
    conv: Box<dyn Conversation>,
    observer: Arc<dyn Observer>,
) -> Result<Proc> {
    let steers = conv.steers();
    let conv = Arc::new(std::sync::Mutex::new(conv));
    let reader = conv.clone();
    let (mut r, stdin) = stream.into_split();
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
                observer.transcript(
                    "stderr",
                    json!({ "text": text.chars().take(4096).collect::<String>() }),
                );
                return None;
            }
            match serde_json::from_str::<Value>(text) {
                Ok(v) if v.is_object() => {
                    if reader.lock().unwrap().transcribe(&v) {
                        observer.transcript("line", v.clone());
                    }
                    Some(v)
                }
                _ => {
                    observer.transcript(
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
                observer.transcript("truncated", json!({ "bytes": line.len() }));
            }
        }
        let _ = tx.send(Out::Exit(code)).await;
    });
    Ok(Proc {
        stdin,
        events,
        conv,
        steers,
        reader: reader_task,
    })
}
