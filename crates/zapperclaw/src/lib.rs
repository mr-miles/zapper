//! zapperclaw: the office cut down to a post office, a foreman per worker
//! and a drive, run in an OpenShell sandbox of its own
//! (docs/proposals/09-zapperclaw.md). It holds no model credential and
//! reaches nothing but the warden, which makes the workers' sandboxes from
//! fixed templates and drives their agents.
//!
//! - `mail`: the post office; `journal`: the record it keeps.
//! - `foreman`: drives a worker's agent through the warden's `/agent` stream.
//! - `tools`: what the workers call at the edge's `/mcp`; `edge`: the
//!   edge's backend, whose callers come from the warden's list.
//! - `board`: you, at the board port the warden forwards to.
//! - `workers`: the warden, as the office reaches it.

pub mod board;
pub mod config;
pub mod edge;
pub mod foreman;
pub mod journal;
pub mod mail;
pub mod prompts;
pub mod tools;
pub mod workers;

#[cfg(test)]
mod fake;
#[cfg(test)]
mod office_tests;

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::{Notify, Semaphore};
use zapper_workbench::drive::{files::Files, Drive};

use config::Config;
use journal::Journal;
use mail::{Draft, PostOffice, Sent};
use workers::{Sandbox, Workers};

pub use zapper_util::now;

/// What a worker is doing, as the board shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum State {
    /// No sandbox yet.
    Off,
    /// Its sandbox is being made.
    Booting,
    Idle,
    /// Idle, waiting on answers it asked for.
    Waiting,
    /// Has mail, waiting for a turn to be free.
    Queued,
    Working,
    Error,
}

/// A worker on the roster, as the office runs it.
pub struct Worker {
    pub name: String,
    pub profile: String,
    pub state: Mutex<(State, Option<String>)>,
    /// Mail not yet shown to the worker.
    pub queue: Mutex<Vec<String>>,
    /// Texts to put before the next wake.
    pub notes: Mutex<Vec<String>>,
    /// Times the Stop hook extended the current turn.
    pub extended: AtomicU32,
    pub wake: Notify,
    /// Its sandbox, once the warden made it.
    pub sandbox: Mutex<Option<Sandbox>>,
    /// The conversation its agent resumes.
    pub conversation: Mutex<Option<String>>,
    /// Whether its instructions were given in this conversation.
    pub briefed: AtomicBool,
    /// Mail handed over during the current turn (the Stop hook, a steer):
    /// the turn's too, if it has to be shown again.
    pub handed: Mutex<Vec<String>>,
}

impl Worker {
    pub fn new(name: &str, profile: &str) -> Worker {
        Worker {
            name: name.to_string(),
            profile: profile.to_string(),
            state: Mutex::new((State::Off, None)),
            queue: Mutex::new(vec![]),
            notes: Mutex::new(vec![]),
            extended: AtomicU32::new(0),
            wake: Notify::new(),
            sandbox: Mutex::new(None),
            conversation: Mutex::new(None),
            briefed: AtomicBool::new(false),
            handed: Mutex::new(vec![]),
        }
    }

    pub fn state(&self) -> State {
        self.state.lock().unwrap().0
    }
}

/// The office: its config, record, post office, workers and drive.
pub struct Office {
    pub config: Config,
    pub dir: PathBuf,
    pub journal: Journal,
    pub post: Mutex<PostOffice>,
    pub workers: BTreeMap<String, Arc<Worker>>,
    pub backend: Arc<dyn Workers>,
    /// Turns that may run at once.
    pub turns: Arc<Semaphore>,
    pub stopping: Notify,
    stopped: AtomicBool,
    /// The drive the workers reach at the edge's `/drive/`.
    pub drive: Drive,
    pub files: Files,
    /// Gateway sandbox id → worker, for the edge.
    by_id: RwLock<HashMap<String, String>>,
    /// Woken whenever mail reaches the board.
    pub board_mail: Notify,
}

impl Office {
    /// The office of `config`, keeping its state in `config.state`, its
    /// workers reached through `backend`.
    pub fn new(config: Config, backend: Arc<dyn Workers>) -> Result<Arc<Office>> {
        let dir = config.path(&config.state);
        make_private_dir(&dir)?;
        make_private_dir(&dir.join("transcripts"))?;
        let journal = Journal::create(&dir.join("journal.jsonl"))?;
        let drive = Drive::new(dir.join("drive")).context("the drive")?;
        let files = Files::new(&dir.join("drive"));
        let mut post = PostOffice::new(config.workers.keys().cloned(), config.relays.clone());
        post.number_from(next_number(&dir.join("journal.jsonl")));
        let workers = config
            .workers
            .iter()
            .map(|(n, m)| (n.clone(), Arc::new(Worker::new(n, &m.profile))))
            .collect();
        Ok(Arc::new(Office {
            turns: Arc::new(Semaphore::new(config.max_concurrent)),
            config,
            dir,
            journal,
            post: Mutex::new(post),
            workers,
            backend,
            stopping: Notify::new(),
            stopped: AtomicBool::new(false),
            drive,
            files,
            by_id: RwLock::new(HashMap::new()),
            board_mail: Notify::new(),
        }))
    }

    pub fn worker(&self, name: &str) -> Option<&Arc<Worker>> {
        self.workers.get(name)
    }

    /// The worker whose sandbox the gateway knows as `id`.
    pub fn worker_by_id(&self, id: &str) -> Option<Arc<Worker>> {
        let name = self.by_id.read().unwrap().get(id).cloned()?;
        self.workers.get(&name).cloned()
    }

    /// Sends `d` now, and wakes whoever it reaches.
    pub fn send(&self, d: Draft) -> Result<Sent> {
        let sent = self.post.lock().unwrap().send(&self.journal, d, now())?;
        self.deliver(&sent);
        Ok(sent)
    }

    fn deliver(&self, sent: &Sent) {
        for name in &sent.wake {
            if let Some(w) = self.workers.get(name) {
                w.queue.lock().unwrap().push(sent.id.clone());
                w.wake.notify_one();
            }
        }
        let to_board = self
            .post
            .lock()
            .unwrap()
            .messages
            .get(&sent.id)
            .is_some_and(|m| m.to.iter().any(|t| t == mail::BOARD));
        if to_board {
            self.board_mail.notify_waiters();
        }
    }

    /// Follow-ups due now, sent.
    pub fn tick(&self) {
        let sent = self.post.lock().unwrap().tick(&self.journal, now());
        for s in &sent {
            self.deliver(s);
        }
    }

    pub fn set_state(&self, w: &Worker, st: State, detail: Option<String>) {
        let mut cur = w.state.lock().unwrap();
        if cur.0 != st || cur.1 != detail {
            *cur = (st, detail.clone());
            drop(cur);
            self.journal.append(
                "worker",
                json!({ "worker": w.name, "state": st, "detail": detail }),
            );
        }
    }

    /// Records worker `w`'s sandbox.
    pub fn set_sandbox(&self, w: &Worker, s: Sandbox) {
        let mut by_id = self.by_id.write().unwrap();
        by_id.retain(|_, n| n != &w.name);
        by_id.insert(s.id.clone(), w.name.clone());
        *w.sandbox.lock().unwrap() = Some(s);
    }

    pub fn stopped(&self) -> bool {
        self.stopped.load(Ordering::Relaxed)
    }

    /// Stops the foremen; the sandboxes stay.
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Relaxed);
        self.stopping.notify_waiters();
        for w in self.workers.values() {
            w.wake.notify_one();
        }
    }

    /// Makes (or finds) worker `w`'s sandbox and runs its foreman, retrying
    /// with a growing pause while the warden can't.
    pub async fn run_worker(self: Arc<Self>, w: Arc<Worker>) {
        let mut pause = 5;
        loop {
            if self.stopped() {
                return;
            }
            self.set_state(&w, State::Booting, None);
            match self.backend.ensure(&w.name, &w.profile).await {
                Ok(s) => {
                    self.set_sandbox(&w, s);
                    break;
                }
                Err(e) => {
                    self.set_state(&w, State::Error, Some(format!("its sandbox: {e:#}")));
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(pause)) => {}
                        _ = self.stopping.notified() => return,
                    }
                    pause = (pause * 2).min(300);
                }
            }
        }
        foreman::run(self, w).await;
    }

    /// Every worker's sandbox and foreman, and the follow-up tick.
    pub fn start(self: &Arc<Self>) {
        self.journal.append(
            "office",
            json!({ "event": "start", "workers": self.config.workers.keys().collect::<Vec<_>>() }),
        );
        for w in self.workers.values() {
            tokio::spawn(self.clone().run_worker(w.clone()));
        }
        let office = self.clone();
        tokio::spawn(async move {
            while !office.stopped() {
                office.tick();
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                    _ = office.stopping.notified() => {}
                }
            }
        });
    }

    /// The workers as the board shows them.
    pub fn roster(&self) -> Value {
        let post = self.post.lock().unwrap();
        let list: Vec<Value> = self
            .workers
            .values()
            .map(|w| {
                let (st, detail) = w.state.lock().unwrap().clone();
                json!({
                    "name": w.name,
                    "profile": w.profile,
                    "state": st,
                    "detail": detail,
                    "queued": w.queue.lock().unwrap().len(),
                    "unread": post.unread(&w.name).len(),
                    "sandbox": w.sandbox.lock().unwrap().as_ref().map(|s| &s.sandbox),
                })
            })
            .collect();
        json!(list)
    }
}

/// One past the highest message or thread number journal `path` has.
fn next_number(path: &std::path::Path) -> u64 {
    use std::io::BufRead as _;
    // A line at a time: a long journal isn't taken into memory.
    let Ok(f) = std::fs::File::open(zapper_record::view(path)) else {
        return 1;
    };
    let mut max = 0;
    let lines = std::io::BufReader::new(f).lines().map_while(Result::ok);
    for line in lines.filter(|l| l.contains("\"type\":\"mail\"")) {
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        for k in ["id", "thread"] {
            let n = v[k]
                .as_str()
                .and_then(|s| s.split_once('-'))
                .and_then(|(_, n)| n.parse::<u64>().ok());
            max = max.max(n.unwrap_or(0));
        }
    }
    max + 1
}

/// `dir`, made private to its user if it isn't there.
fn make_private_dir(dir: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("making {}", dir.display()))
}
