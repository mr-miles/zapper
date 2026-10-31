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
//! events. A firm's workers are driven by the office's foreman, which has
//! its own turn loop (mail, budgets, officers) over the same `start` and
//! `Proc`.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Result};
use serde_json::{json, Value};
use tokio::net::UnixStream;
use tokio::sync::Notify;

use super::harness::{Conversation, Outcome};

mod process;
pub use process::{start, Observer, Out, Proc};

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
    /// Where a persistent session's conversation id is kept, so a daemon
    /// adopting the session resumes the conversation.
    kept: Option<std::path::PathBuf>,
}

impl Driver {
    pub fn new() -> Arc<Driver> {
        Self::with(None, None)
    }

    /// A persistent session's: its conversation id kept in `path`, and
    /// read back from it.
    pub fn kept(path: std::path::PathBuf) -> Arc<Driver> {
        let conversation = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .and_then(|v| v["conversation"].as_str().map(str::to_string));
        Self::with(conversation, Some(path))
    }

    fn with(conversation: Option<String>, kept: Option<std::path::PathBuf>) -> Arc<Driver> {
        Arc::new(Driver {
            state: Mutex::new(State {
                status: Status::Idle,
                queue: VecDeque::new(),
                activity: None,
                error: None,
                conversation,
                turns: 0,
                tokens: 0,
            }),
            wake: Notify::new(),
            stopping: Notify::new(),
            kept,
        })
    }

    /// The conversation the agent is in, kept if the session is persistent.
    fn set_conversation(&self, id: &str) {
        let mut st = self.state.lock().unwrap();
        if st.conversation.as_deref() == Some(id) {
            return;
        }
        st.conversation = Some(id.to_string());
        if let Some(path) = &self.kept {
            let body = json!({ "conversation": id }).to_string();
            if let Err(e) = crate::util::write_private(path, body.as_bytes()) {
                tracing::warn!("keeping the conversation id in {}: {e:#}", path.display());
            }
        }
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

    /// After `stop`, ready to be driven again (`drive`) when the session is
    /// started again: its queue and its conversation stay, so the agent
    /// resumes where it was.
    pub fn revive(&self) {
        let mut st = self.state.lock().unwrap();
        if st.status == Status::Stopped {
            st.status = Status::Idle;
        }
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
        self.driver.set_conversation(id);
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
    turn_timeout: Duration,
) {
    use tokio::time::Instant;
    let watch: Arc<dyn Observer> = Arc::new(Watch {
        driver: driver.clone(),
        report: report.clone(),
        transcript,
    });
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
        // Prompts during the turn, for harnesses that take them while the
        // agent works.
        let more = || {
            let more = driver.take_queue();
            (!more.is_empty()).then(|| {
                report.event("steered", json!({ "prompts": more.len() }));
                more.join("\n\n")
            })
        };
        let Some(outcome) = p
            .turn(&prompt, deadline, &driver.wake, &driver.stopping, more)
            .await
        else {
            return;
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
                    let backoff = turn_timeout.min(Duration::from_secs(600));
                    backoff_until = rest(&driver, report.as_ref(), why, backoff);
                } else {
                    driver.set(Status::Idle, None);
                }
            }
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
                    backoff_until = rest(&driver, report.as_ref(), why, Duration::from_secs(120));
                    restarts.clear();
                } else {
                    driver.set(Status::Idle, None);
                }
            }
        }
    }
}

/// Logs an error, moves the driver to `Error`, and returns a fresh backoff
/// deadline `backoff` from now. Shared by the turn loop's give-up branches.
fn rest(
    driver: &Driver,
    report: &dyn Report,
    why: String,
    backoff: Duration,
) -> tokio::time::Instant {
    report.event("error", json!({ "error": why }));
    driver.set(Status::Error, Some(why));
    tokio::time::Instant::now() + backoff
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
pub(crate) fn drive(daemon: &Arc<super::Daemon>, session: &Arc<super::session::Session>) {
    let Some(driver) = session.driver.clone() else {
        return;
    };
    let kind = session.resolved.agent.kind.clone();
    let Some(harness) = super::harness::get(&kind).and_then(|h| h.headless()) else {
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
            let req = zapper_policy::proto::ShellRequest::exec(argv);
            let (stream, _) = s.open_shell(req).await?;
            Ok(stream)
        })
    });
    let report = Arc::new(SessionReport {
        daemon: Arc::downgrade(daemon),
        session: Arc::downgrade(session),
    });
    let file = super::record::Writer::open(&session.dir.driver_log()).ok();
    let file = Mutex::new(file);
    let transcript = Box::new(move |v: Value| {
        if let Some(f) = file.lock().unwrap().as_mut() {
            let _ = f.line(&serde_json::to_vec(&v).unwrap_or_default());
        }
    });
    tokio::spawn(run(
        driver,
        conversations,
        spawn,
        report,
        transcript,
        TURN_TIMEOUT,
    ));
}

#[cfg(test)]
mod tests;
