//! Image builds the daemon runs for the API: one at a time, from the running
//! config, each with its log kept (its latest lines) and followed live while
//! it runs, and an `image` event when it starts and when it ends.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Result};
use serde::Serialize;
use tokio::sync::broadcast;

use super::now;

/// Lines of a build's log kept for those who follow it later.
const KEPT_LINES: usize = 5000;
/// Builds kept in the list once they end.
const KEPT_BUILDS: usize = 16;

/// How a build ended.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// The image's file.
    Built(String),
    Failed(String),
}

/// What a build's followers are sent as it runs: a line of its log, and,
/// last, how it ended.
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    Line(String),
    End(Outcome),
}

/// One build: its log so far and, once it ends, its outcome.
pub struct ImageBuild {
    pub id: u64,
    pub name: String,
    pub started: f64,
    log: Mutex<Log>,
    tx: broadcast::Sender<Message>,
}

struct Log {
    lines: VecDeque<String>,
    /// Lines no longer kept, from the start.
    dropped: u64,
    outcome: Option<Outcome>,
}

/// A build as the list shows it.
#[derive(Debug, Clone, Serialize)]
pub struct BuildInfo {
    pub id: u64,
    pub name: String,
    pub started: f64,
    pub outcome: Option<Outcome>,
}

/// What a follower is sent: the kept lines (after `dropped` lines no longer
/// kept); then, if the build has ended, its `outcome`, else the messages of
/// `live` up to its `End`.
pub struct Follow {
    pub dropped: u64,
    pub kept: Vec<String>,
    pub outcome: Option<Outcome>,
    pub live: broadcast::Receiver<Message>,
}

impl ImageBuild {
    fn line(&self, line: &str) {
        let mut log = self.log.lock().unwrap();
        if log.lines.len() >= KEPT_LINES {
            log.lines.pop_front();
            log.dropped += 1;
        }
        log.lines.push_back(line.to_string());
        // Under the lock, so a follower's snapshot and subscription agree.
        let _ = self.tx.send(Message::Line(line.to_string()));
    }

    fn end(&self, outcome: Outcome) {
        let mut log = self.log.lock().unwrap();
        log.outcome = Some(outcome.clone());
        let _ = self.tx.send(Message::End(outcome));
    }

    /// The log so far and a subscription to the rest, taken together so no
    /// line, and not the end, falls between them.
    pub fn follow(&self) -> Follow {
        let log = self.log.lock().unwrap();
        Follow {
            dropped: log.dropped,
            kept: log.lines.iter().cloned().collect(),
            outcome: log.outcome.clone(),
            live: self.tx.subscribe(),
        }
    }

    pub fn info(&self) -> BuildInfo {
        BuildInfo {
            id: self.id,
            name: self.name.clone(),
            started: self.started,
            outcome: self.log.lock().unwrap().outcome.clone(),
        }
    }
}

/// The daemon's builds: the one running, and the latest that ended.
#[derive(Default)]
pub struct Builds {
    next: AtomicU64,
    /// Shared with the running build's thread, which clears it at the end.
    running: Arc<Mutex<Option<Arc<ImageBuild>>>>,
    all: Mutex<VecDeque<Arc<ImageBuild>>>,
}

/// The running build's place, emptied when dropped.
struct Slot(Arc<Mutex<Option<Arc<ImageBuild>>>>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).take();
    }
}

impl Builds {
    /// Starts `run` on a blocking thread as the build of `name`, given the
    /// build's id and a log for its lines; `ended` is called with the build
    /// once it has its outcome. Refused while another build runs.
    pub fn start<F, E>(&self, name: &str, run: F, ended: E) -> Result<Arc<ImageBuild>>
    where
        F: FnOnce(u64, &dyn Fn(&str)) -> Result<PathBuf> + Send + 'static,
        E: FnOnce(&ImageBuild) + Send + 'static,
    {
        let build = {
            let mut running = self.running.lock().unwrap();
            if let Some(b) = running.as_ref() {
                bail!(
                    "image {} is being built (build {}); one build runs at a time",
                    b.name,
                    b.id
                );
            }
            let (tx, _) = broadcast::channel(1024);
            let build = Arc::new(ImageBuild {
                id: self.next.fetch_add(1, Ordering::Relaxed) + 1,
                name: name.to_string(),
                started: now(),
                log: Mutex::new(Log {
                    lines: VecDeque::new(),
                    dropped: 0,
                    outcome: None,
                }),
                tx,
            });
            *running = Some(build.clone());
            build
        };
        {
            let mut all = self.all.lock().unwrap();
            if all.len() >= KEPT_BUILDS {
                all.pop_front();
            }
            all.push_back(build.clone());
        }
        let (b, running) = (build.clone(), self.running.clone());
        tokio::task::spawn_blocking(move || {
            // The slot is freed once the build has ended and `ended` has
            // run, or either panicked.
            let _slot = Slot(running);
            let run = std::panic::AssertUnwindSafe(|| run(b.id, &|l| b.line(l)));
            let outcome = match std::panic::catch_unwind(run) {
                Ok(Ok(path)) => Outcome::Built(path.display().to_string()),
                Ok(Err(e)) => {
                    let e = format!("{e:#}");
                    b.line(&e);
                    Outcome::Failed(e)
                }
                Err(_) => {
                    let e = "the build panicked".to_string();
                    b.line(&e);
                    Outcome::Failed(e)
                }
            };
            b.end(outcome);
            ended(&b);
        });
        Ok(build)
    }

    /// Whether a build is running now.
    #[cfg(test)]
    pub fn busy(&self) -> bool {
        self.running.lock().unwrap().is_some()
    }

    pub fn get(&self, id: u64) -> Option<Arc<ImageBuild>> {
        self.all
            .lock()
            .unwrap()
            .iter()
            .find(|b| b.id == id)
            .cloned()
    }

    pub fn list(&self) -> Vec<BuildInfo> {
        self.all.lock().unwrap().iter().map(|b| b.info()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn finished(f: &mut Follow) -> (Vec<String>, Outcome) {
        let mut lines = f.kept.clone();
        if let Some(o) = &f.outcome {
            return (lines, o.clone());
        }
        loop {
            match f.live.recv().await.unwrap() {
                Message::Line(l) => lines.push(l),
                Message::End(o) => return (lines, o),
            }
        }
    }

    /// The slot is freed just after `ended` runs: waits for it.
    async fn wait_free(builds: &Builds) {
        for _ in 0..100 {
            if !builds.busy() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("the slot wasn't freed");
    }

    #[tokio::test]
    async fn a_build_is_followed_to_its_end() {
        let builds = Builds::default();
        let (go, wait) = std::sync::mpsc::channel::<()>();
        let b = builds
            .start(
                "web",
                move |_, log| {
                    log("one");
                    wait.recv().unwrap();
                    log("two");
                    Ok(PathBuf::from("/images/web.ext4"))
                },
                |_| {},
            )
            .unwrap();
        // Another while it runs is refused.
        let e = builds
            .start("other", |_, _| Ok(PathBuf::new()), |_| {})
            .err()
            .unwrap();
        assert!(e.to_string().contains("image web is being built"), "{e}");
        let mut f = b.follow();
        go.send(()).unwrap();
        let (lines, outcome) = finished(&mut f).await;
        assert!(lines.ends_with(&["two".to_string()]), "{lines:?}");
        assert!(lines.contains(&"one".to_string()), "{lines:?}");
        assert_eq!(outcome, Outcome::Built("/images/web.ext4".into()));
        // Followed after the end: the kept log and the outcome.
        let late = b.follow();
        assert_eq!(late.kept, vec!["one", "two"]);
        assert_eq!(late.outcome, Some(outcome));
        wait_free(&builds).await;
        assert_eq!(builds.list()[0].name, "web");
    }

    #[tokio::test]
    async fn a_failed_build_says_why_and_frees_the_slot() {
        let builds = Builds::default();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let b = builds
            .start(
                "web",
                |_, _| bail!("podman build failed"),
                move |b| {
                    let _ = tx.send(b.id);
                },
            )
            .unwrap();
        assert_eq!(rx.await.unwrap(), b.id);
        assert_eq!(
            b.info().outcome,
            Some(Outcome::Failed("podman build failed".into()))
        );
        assert_eq!(b.follow().kept, vec!["podman build failed"]);
        wait_free(&builds).await;
        builds
            .start("web", |_, _| Ok(PathBuf::new()), |_| {})
            .unwrap();
    }

    /// A build that panics ends, failed, and the next is taken.
    #[tokio::test]
    async fn a_panicking_build_ends_and_frees_the_slot() {
        let builds = Builds::default();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let b = builds
            .start(
                "web",
                |_, _| panic!("a bug in the build"),
                move |b| {
                    let _ = tx.send(b.id);
                },
            )
            .unwrap();
        assert_eq!(rx.await.unwrap(), b.id);
        assert_eq!(
            b.info().outcome,
            Some(Outcome::Failed("the build panicked".into()))
        );
        wait_free(&builds).await;
        builds
            .start("web", |_, _| Ok(PathBuf::new()), |_| {})
            .unwrap();
    }

    /// The slot is taken until `ended` has run: a build started from it
    /// (as the end's event is sent) is refused, not run alongside.
    #[tokio::test]
    async fn the_slot_is_held_until_the_end_is_handled() {
        let builds = Arc::new(Builds::default());
        let (tx, rx) = tokio::sync::oneshot::channel();
        let inner = builds.clone();
        builds
            .start(
                "web",
                |_, _| Ok(PathBuf::new()),
                move |_| {
                    let _ = tx.send(inner.busy());
                },
            )
            .unwrap();
        assert!(rx.await.unwrap(), "still busy while ended runs");
    }
}
