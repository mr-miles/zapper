//! The driver: runs a session's agent headless (`agent.driven`), through
//! its harness (`claude -p` with stream-json, `pi --mode rpc`), and gives it
//! the tasks the operator sends (the API's prompt route, `zapper tell`, the
//! UI). One conversation per session: the process stays between turns, and
//! a new one resumes it after a crash or a turn that ran too long.
//!
//! The agent runs over a terminal channel without a tty, as `zapper exec`
//! does. Prompts that arrive while it works wait for the next turn, or are
//! handed over at once for harnesses that take them (pi). Every output line
//! goes to the session's `driver.jsonl`; turns, activity and exits are
//! events. A firm's workers are driven by the office's foreman, not here.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::UnixStream;
use tokio::sync::{mpsc, Notify};
use zapper_policy::proto::{Frame, MAX_LINE};

use super::harness::{Conversation, Event, Outcome, TurnEnd};
use super::session::read_frame;

/// Largest prompt the driver takes.
pub const MAX_PROMPT: usize = 64 << 10;
/// Prompts waiting at most; more are refused until the agent catches up.
const MAX_QUEUED: usize = 64;
/// How long a turn may take before the process is dropped.
const TURN_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Restarts allowed within ten minutes before the driver rests (as the
/// foreman's `MAX_RESTARTS`).
const MAX_RESTARTS: usize = 5;
/// Turns in a row that may run out of time before the driver rests (as the
/// foreman's `MAX_TIMEOUTS`).
const MAX_TIMEOUTS: u32 = 3;

enum Out {
    /// A turn ended.
    End(TurnEnd),
    /// A line the protocol wants written back.
    Write(Vec<u8>),
    /// The process exited.
    Exit(Option<i32>),
    /// The turn ran past the turn timeout.
    Late,
}

/// Where a driven agent's output goes: its transcript and what it is doing.
trait Observer: Send + Sync {
    /// An output line: a JSON object, or text from stderr or not JSON
    /// (`kind` says which).
    fn transcript(&self, kind: &str, line: Value);
    /// The conversation's id, for resuming it.
    fn conversation(&self, id: &str);
    /// What the agent is doing now.
    fn activity(&self, text: &str);
}

struct Proc {
    stdin: OwnedWriteHalf,
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
        let mut buf = Vec::new();
        Frame::Data(line).write_to(&mut buf)?;
        self.stdin.write_all(&buf).await?;
        Ok(())
    }

    async fn send(&mut self, text: &str) -> Result<()> {
        let line = self.conv.lock().unwrap().prompt(text);
        self.write(line).await
    }
}

async fn start(
    stream: UnixStream,
    mut conv: Box<dyn Conversation>,
    observer: Arc<dyn Observer>,
) -> Result<Proc> {
    let opening = conv.opening();
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
                        Event::Send(line) => {
                            let _ = tx.send(Out::Write(line)).await;
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
    let mut p = Proc {
        stdin,
        events,
        conv,
        steers,
        reader: reader_task,
    };
    for line in opening {
        p.write(line).await?;
    }
    Ok(p)
}

/// What the driver is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Waiting for a prompt.
    Idle,
    /// Starting the agent.
    Starting,
    /// In a turn.
    Working,
    /// Waiting out the provider's rate limit.
    Backoff,
    /// Gave up until the next prompt (see `error`).
    Error,
    /// The session ended.
    Stopped,
}

impl Status {
    fn name(self) -> &'static str {
        match self {
            Status::Idle => "idle",
            Status::Starting => "starting",
            Status::Working => "working",
            Status::Backoff => "backoff",
            Status::Error => "error",
            Status::Stopped => "stopped",
        }
    }
}

struct State {
    status: Status,
    queue: VecDeque<String>,
    activity: Option<String>,
    error: Option<String>,
    conversation: Option<String>,
    turns: u64,
    tokens: u64,
}

/// A session's driver: its queue of prompts and what it is doing.
pub struct Driver {
    state: Mutex<State>,
    wake: Notify,
    stopping: Notify,
}

impl Driver {
    pub fn new() -> Arc<Driver> {
        Arc::new(Driver {
            state: Mutex::new(State {
                status: Status::Idle,
                queue: VecDeque::new(),
                activity: None,
                error: None,
                conversation: None,
                turns: 0,
                tokens: 0,
            }),
            wake: Notify::new(),
            stopping: Notify::new(),
        })
    }

    /// Queues a prompt; how many are waiting, this one included.
    pub fn prompt(&self, text: &str) -> Result<usize> {
        let text = text.trim();
        if text.is_empty() {
            bail!("the prompt is empty");
        }
        if text.len() > MAX_PROMPT {
            bail!("the prompt is longer than {} KiB", MAX_PROMPT >> 10);
        }
        let mut st = self.state.lock().unwrap();
        if st.status == Status::Stopped {
            bail!("the session has ended");
        }
        if st.queue.len() >= MAX_QUEUED {
            bail!("{MAX_QUEUED} prompts are already waiting");
        }
        st.queue.push_back(text.to_string());
        let n = st.queue.len();
        drop(st);
        self.wake.notify_one();
        Ok(n)
    }

    /// The driver's state, for the session's info.
    pub fn info(&self) -> Value {
        let st = self.state.lock().unwrap();
        json!({
            "status": st.status.name(),
            "activity": st.activity,
            "error": st.error,
            "queued": st.queue.len(),
            "turns": st.turns,
            "tokens": st.tokens,
            "conversation": st.conversation,
        })
    }

    #[cfg(test)]
    pub fn status(&self) -> Status {
        self.state.lock().unwrap().status
    }

    /// Ends the loop; the agent's process goes with it.
    pub fn stop(&self) {
        self.state.lock().unwrap().status = Status::Stopped;
        self.stopping.notify_waiters();
        self.wake.notify_one();
    }

    fn set(&self, status: Status, error: Option<String>) {
        let mut st = self.state.lock().unwrap();
        if st.status != Status::Stopped {
            st.status = status;
            st.error = error;
            if status != Status::Working {
                st.activity = None;
            }
        }
    }

    fn take_queue(&self) -> Vec<String> {
        self.state.lock().unwrap().queue.drain(..).collect()
    }

    fn requeue(&self, prompts: Vec<String>) {
        let mut st = self.state.lock().unwrap();
        for p in prompts.into_iter().rev() {
            st.queue.push_front(p);
        }
    }

    fn stopped(&self) -> bool {
        self.state.lock().unwrap().status == Status::Stopped
    }

    fn has_work(&self) -> bool {
        !self.state.lock().unwrap().queue.is_empty()
    }
}

/// What the loop reports: the session's events.
trait Report: Send + Sync {
    fn event(&self, event: &str, detail: Value);
}

/// The driver's observer: transcript and conversation id into the driver
/// and the report.
struct Watch {
    driver: Arc<Driver>,
    report: Arc<dyn Report>,
    transcript: Box<dyn Fn(Value) + Send + Sync>,
}

impl Observer for Watch {
    fn transcript(&self, kind: &str, line: Value) {
        (self.transcript)(if kind == "line" {
            line
        } else {
            json!({ "zapper": kind, "detail": line })
        });
    }

    fn conversation(&self, id: &str) {
        self.driver.state.lock().unwrap().conversation = Some(id.to_string());
    }

    fn activity(&self, text: &str) {
        self.driver.state.lock().unwrap().activity = Some(text.to_string());
        self.report.event("activity", json!({ "activity": text }));
    }
}

/// How the loop starts the agent: a terminal channel running `argv`.
type Spawn = Box<
    dyn Fn(Vec<String>) -> futures_util::future::BoxFuture<'static, Result<UnixStream>>
        + Send
        + Sync,
>;

/// How the loop starts a conversation, resuming the given one.
type Conversations = Box<dyn Fn(Option<&str>) -> Box<dyn Conversation> + Send + Sync>;

/// Waits for a reason to look again, or a timeout (as the foreman's `nap`).
async fn nap(driver: &Driver, secs: u64) {
    tokio::select! {
        _ = driver.wake.notified() => {}
        _ = driver.stopping.notified() => {}
        _ = tokio::time::sleep(Duration::from_secs(secs)) => {}
    }
}

/// Runs a session's agent until the driver stops. The loop is the
/// foreman's (`office/foreman.rs`), with the operator's prompts for mail
/// and without budgets.
async fn run(
    driver: Arc<Driver>,
    conversations: Conversations,
    spawn: Spawn,
    report: Arc<dyn Report>,
    transcript: Box<dyn Fn(Value) + Send + Sync>,
    turn_timeout: Option<Duration>,
) {
    use tokio::time::Instant;
    let watch: Arc<dyn Observer> = Arc::new(Watch {
        driver: driver.clone(),
        report: report.clone(),
        transcript,
    });
    let turn_timeout = turn_timeout.unwrap_or(TURN_TIMEOUT);
    let mut proc: Option<Proc> = None;
    let mut restarts: VecDeque<Instant> = VecDeque::new();
    let mut backoff_until = Instant::now();
    let mut timeouts = 0;
    loop {
        if driver.stopped() {
            return;
        }
        // Nothing to do, or resting after a rate limit or an error.
        if !driver.has_work() || Instant::now() < backoff_until {
            let secs = backoff_until
                .saturating_duration_since(Instant::now())
                .as_secs()
                .clamp(1, 5);
            nap(&driver, secs).await;
            continue;
        }
        if proc.is_none() {
            driver.set(Status::Starting, None);
            let resume = driver.state.lock().unwrap().conversation.clone();
            let conv = conversations(resume.as_deref());
            let started = match spawn(conv.argv()).await {
                Ok(stream) => start(stream, conv, watch.clone()).await,
                Err(e) => Err(e),
            };
            match started {
                Ok(p) => {
                    report.event("started", json!({ "resumed": resume }));
                    proc = Some(p);
                }
                Err(e) => {
                    let why = format!("starting the agent: {e:#}");
                    report.event("error", json!({ "error": why }));
                    driver.set(Status::Error, Some(why));
                    nap(&driver, 30).await;
                    continue;
                }
            }
        }
        // What this turn delivers.
        let prompts = driver.take_queue();
        let prompt = prompts.join("\n\n");
        driver.set(Status::Working, None);
        report.event("turn_start", json!({ "prompts": prompts.len() }));
        let started = Instant::now();
        let deadline = started + turn_timeout;
        let p = proc.as_mut().unwrap();
        let outcome = match p.send(&prompt).await {
            Ok(()) => loop {
                tokio::select! {
                    _ = tokio::time::sleep_until(deadline) => break Out::Late,
                    o = p.events.recv() => match o {
                        Some(Out::Write(line)) => {
                            if p.write(line).await.is_err() {
                                break Out::Exit(None);
                            }
                        }
                        Some(o) => break o,
                        None => break Out::Exit(None),
                    },
                    // Prompts during the turn, for harnesses that take them
                    // while the agent works.
                    _ = driver.wake.notified(), if p.steers => {
                        let more = driver.take_queue();
                        if !more.is_empty() {
                            report.event("steered", json!({ "prompts": more.len() }));
                            let line = p.conv.lock().unwrap().steer(&more.join("\n\n"));
                            if let Some(line) = line {
                                if p.write(line).await.is_err() {
                                    break Out::Exit(None);
                                }
                            }
                        }
                    }
                    _ = driver.stopping.notified() => return,
                }
            },
            Err(_) => Out::Exit(None),
        };
        let ms = started.elapsed().as_millis() as u64;
        match outcome {
            Out::End(end) => {
                let t = end.tokens;
                if end.outcome == Outcome::RateLimited {
                    // Not the agent's fault: the same prompts, later.
                    driver.requeue(prompts);
                    backoff_until = Instant::now() + Duration::from_secs(60);
                    driver.set(Status::Backoff, None);
                    report.event("turn_end", json!({ "outcome": "rate_limited", "ms": ms }));
                } else if let Outcome::Failed(why) = end.outcome {
                    // The agent never saw the prompts: keep them, say why,
                    // retry later.
                    driver.requeue(prompts);
                    backoff_until = Instant::now() + Duration::from_secs(60);
                    report.event(
                        "turn_end",
                        json!({ "outcome": "failed", "ms": ms, "error": why }),
                    );
                    driver.set(Status::Error, Some(why));
                } else {
                    {
                        let mut st = driver.state.lock().unwrap();
                        st.turns += 1;
                        st.tokens += t;
                    }
                    let mut rec = json!({ "outcome": "done", "ms": ms, "tokens": t });
                    zapper_policy::merge(&mut rec, &end.detail);
                    report.event("turn_end", rec);
                    // A turn that went through clears an earlier error.
                    timeouts = 0;
                    driver.set(Status::Idle, None);
                }
            }
            Out::Late => {
                // Dropping the process closes its connection: the agent
                // hangs up, and the next turn resumes the conversation. The
                // prompts aren't handed over again (the agent has seen them).
                proc = None;
                let limit = format!("{} minutes", turn_timeout.as_secs() / 60);
                report.event("turn_end", json!({ "outcome": "timed_out", "ms": ms, "error": format!("no turn end within {limit}") }));
                timeouts += 1;
                if timeouts >= MAX_TIMEOUTS {
                    timeouts = 0;
                    let why = format!("{MAX_TIMEOUTS} turns in a row ran past {limit}");
                    // As long as a turn, up to ten minutes.
                    backoff_until = Instant::now() + turn_timeout.min(Duration::from_secs(600));
                    report.event("error", json!({ "error": why }));
                    driver.set(Status::Error, Some(why));
                } else {
                    driver.set(Status::Idle, None);
                }
            }
            // Written while waiting for the turn's end.
            Out::Write(_) => {}
            Out::Exit(code) => {
                proc = None;
                driver.requeue(prompts);
                let at = Instant::now();
                report.event("exited", json!({ "code": code, "ms": ms }));
                restarts.push_back(at);
                while restarts
                    .front()
                    .is_some_and(|t| at - *t > Duration::from_secs(600))
                {
                    restarts.pop_front();
                }
                if restarts.len() > MAX_RESTARTS {
                    let why = format!(
                        "the agent exited {} times in ten minutes (last code {code:?})",
                        restarts.len()
                    );
                    report.event("error", json!({ "error": why }));
                    driver.set(Status::Error, Some(why));
                    backoff_until = Instant::now() + Duration::from_secs(120);
                    restarts.clear();
                } else {
                    driver.set(Status::Idle, None);
                }
            }
        }
    }
}

/// The session's events, from the driver.
struct SessionReport {
    daemon: std::sync::Weak<super::Daemon>,
    session: std::sync::Weak<super::session::Session>,
}

impl Report for SessionReport {
    fn event(&self, event: &str, detail: Value) {
        if let (Some(d), Some(s)) = (self.daemon.upgrade(), self.session.upgrade()) {
            d.emit(
                Some(&s),
                super::EventBody::Driver {
                    event: event.into(),
                    detail,
                },
            );
        }
    }
}

/// Starts the driver of a driven session that has just come up: its agent
/// runs over a terminal channel, its output goes to `driver.jsonl` in the
/// session's directory.
pub fn drive(daemon: &Arc<super::Daemon>, session: &Arc<super::session::Session>) {
    let Some(driver) = session.driver.clone() else {
        return;
    };
    let kind = session.resolved.agent.kind.clone();
    let Some(harness) = super::harness::get(&kind).and_then(|h| h.driver()) else {
        // Validation allows only kinds with a driver; said here all the same.
        driver.set(
            Status::Error,
            Some(format!("agent kind {kind} can't be driven")),
        );
        return;
    };
    let conversations: Conversations = Box::new(move |resume| harness.conversation(resume));
    let s = session.clone();
    let spawn: Spawn = Box::new(move |argv| {
        let s = s.clone();
        Box::pin(async move {
            let req = zapper_policy::proto::ShellRequest {
                id: 0,
                argv,
                tty: None,
                env: Default::default(),
                system: false,
            };
            let (stream, _) = s.open_shell(req).await?;
            Ok(stream)
        })
    });
    let report = Arc::new(SessionReport {
        daemon: Arc::downgrade(daemon),
        session: Arc::downgrade(session),
    });
    let file = super::record::Writer::open(&session.dir.join("driver.jsonl")).ok();
    let file = Mutex::new(file);
    let transcript = Box::new(move |v: Value| {
        if let Some(f) = file.lock().unwrap().as_mut() {
            let _ = f.line(&serde_json::to_vec(&v).unwrap_or_default());
        }
    });
    tokio::spawn(run(driver, conversations, spawn, report, transcript, None));
}

#[cfg(test)]
mod tests;
