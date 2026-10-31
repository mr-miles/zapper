//! A session's live machinery while its VM runs: its status, as a state
//! machine, the VMM, the control channel and the bounds on what the guest
//! may have going at once.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tokio::sync::{mpsc, watch, Semaphore};
use zapper_policy::proto::HostMsg;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "state", content = "detail", rename_all = "lowercase")]
pub enum Status {
    Starting,
    Running,
    Failed(String),
    Stopped(String),
}

/// A session's live machinery: its lifecycle status, the Firecracker
/// process, the control channel to the guest agent, and the counters that
/// gate shells and exec decisions. All of it is created when the VM boots
/// and dropped when it stops; the owner of "the session while it runs".
pub(crate) struct GuestRuntime {
    status: watch::Sender<Status>,
    vm: tokio::sync::Mutex<Option<crate::daemon::vm::VmProc>>,
    /// The control channel to the guest agent, once it connects.
    guest: Mutex<Option<mpsc::Sender<HostMsg>>>,
    next_shell: AtomicU64,
    /// Bounds concurrent exec decisions.
    exec_checks: Arc<Semaphore>,
    /// Bounds concurrent proxy connections from the guest.
    conns: Arc<Semaphore>,
    /// The tasks accepting on the guest's sockets (`boot::serve_guest`),
    /// ended with the VM: each holds the session.
    listeners: Mutex<Vec<tokio::task::AbortHandle>>,
}

impl GuestRuntime {
    pub fn new() -> Self {
        Self {
            status: watch::channel(Status::Starting).0,
            vm: tokio::sync::Mutex::new(None),
            guest: Mutex::new(None),
            next_shell: AtomicU64::new(1),
            exec_checks: Arc::new(Semaphore::new(64)),
            conns: Arc::new(Semaphore::new(256)),
            listeners: Mutex::new(Vec::new()),
        }
    }

    /// The lifecycle-status channel: subscribe to it, borrow it, or send a
    /// transition through it.
    pub fn status(&self) -> &watch::Sender<Status> {
        &self.status
    }

    /// Starting → Running: the guest is ready. From no other state: a stop
    /// that came first stands.
    pub fn mark_ready(&self) -> bool {
        self.status.send_if_modified(|s| {
            let starting = matches!(s, Status::Starting);
            if starting {
                *s = Status::Running;
            }
            starting
        })
    }

    /// Starting → Failed(`why`): it never came up. A session stopped
    /// meanwhile stays stopped.
    pub fn fail_if_starting(&self, why: String) -> bool {
        self.status.send_if_modified(|s| {
            let starting = matches!(s, Status::Starting);
            if starting {
                *s = Status::Failed(why);
            }
            starting
        })
    }

    /// Starting or Running → Failed(`why`). A stopped one stays stopped.
    pub fn fail(&self, why: String) -> bool {
        self.status.send_if_modified(|s| {
            let up = matches!(s, Status::Starting | Status::Running);
            if up {
                *s = Status::Failed(why);
            }
            up
        })
    }

    /// Stopped or Failed → Starting: claims a start, which only one caller
    /// gets; false while it is starting or running.
    pub fn claim_start(&self) -> bool {
        self.status.send_if_modified(|s| {
            let down = matches!(s, Status::Stopped(_) | Status::Failed(_));
            if down {
                *s = Status::Starting;
            }
            down
        })
    }

    /// → Stopped(`reason`), a failure kept as it is: whether this caller is
    /// the one stopping it (not if it was stopped already).
    pub fn begin_stop(&self, reason: &str) -> bool {
        let mut first = false;
        self.status.send_if_modified(|s| match s {
            Status::Stopped(_) => false,
            Status::Failed(_) => {
                first = true;
                false
            }
            _ => {
                first = true;
                *s = Status::Stopped(reason.into());
                true
            }
        });
        first
    }

    /// The Firecracker process handle: set at boot, taken at teardown.
    pub fn vm(&self) -> &tokio::sync::Mutex<Option<crate::daemon::vm::VmProc>> {
        &self.vm
    }

    /// Records the guest's control channel once it connects.
    pub fn set_guest(&self, tx: mpsc::Sender<HostMsg>) {
        *self.guest.lock().unwrap() = Some(tx);
    }

    /// Drops the control channel and ends the tasks accepting on the
    /// guest's sockets (teardown).
    pub fn clear_guest(&self) {
        *self.guest.lock().unwrap() = None;
        self.end_listeners();
    }

    /// Keeps a task accepting on one of the guest's sockets, to end with
    /// the VM.
    pub fn hold_listener(&self, task: tokio::task::AbortHandle) {
        self.listeners.lock().unwrap().push(task);
    }

    /// Ends the tasks accepting on the guest's sockets.
    pub fn end_listeners(&self) {
        for task in self.listeners.lock().unwrap().drain(..) {
            task.abort();
        }
    }

    /// Sends a message to the guest agent, if the channel is up.
    pub fn send(&self, msg: HostMsg) {
        if let Some(tx) = self.guest.lock().unwrap().as_ref() {
            let _ = tx.try_send(msg);
        }
    }

    /// The next terminal id.
    pub fn next_shell(&self) -> u64 {
        self.next_shell.fetch_add(1, Ordering::Relaxed)
    }

    /// New terminals get numbers after `n`, one the guest already has.
    pub fn shells_after(&self, n: u64) {
        self.next_shell.fetch_max(n + 1, Ordering::Relaxed);
    }

    /// A permit for one concurrent exec decision, or `None` over the cap.
    pub fn exec_permit(&self) -> Option<tokio::sync::OwnedSemaphorePermit> {
        self.exec_checks.clone().try_acquire_owned().ok()
    }

    /// The semaphore bounding concurrent proxy connections, for `accept_loop`.
    pub fn conns(&self) -> Arc<Semaphore> {
        self.conns.clone()
    }
}

#[cfg(test)]
mod status_tests {
    use super::{GuestRuntime, Status};

    fn at(r: &GuestRuntime, s: Status) {
        r.status().send_replace(s);
    }

    /// A session stopped while it started stays stopped: its guest's late
    /// `Ready`, or failure, doesn't bring it back.
    #[test]
    fn a_stop_stands_against_a_late_ready() {
        let r = GuestRuntime::new();
        assert!(r.begin_stop("stopped by operator"));
        assert!(!r.mark_ready());
        assert!(!r.fail("guest setup: x".into()));
        assert!(!r.fail_if_starting("vm exited".into()));
        assert_eq!(
            *r.status().borrow(),
            Status::Stopped("stopped by operator".into())
        );
        assert!(!r.begin_stop("again"), "stopped once");
    }

    #[test]
    fn ready_only_from_starting() {
        let r = GuestRuntime::new();
        assert!(r.mark_ready());
        assert_eq!(*r.status().borrow(), Status::Running);
        at(&r, Status::Failed("x".into()));
        assert!(!r.mark_ready());
        // A failure is kept by a stop, which still goes ahead.
        assert!(r.begin_stop("stopped"));
        assert_eq!(*r.status().borrow(), Status::Failed("x".into()));
    }

    /// A start is claimed once: not while one is starting or running, and by
    /// one of many callers at once.
    #[test]
    fn a_start_is_claimed_once() {
        let r = GuestRuntime::new();
        assert!(!r.claim_start(), "starting already");
        at(&r, Status::Running);
        assert!(!r.claim_start(), "running");
        at(&r, Status::Stopped("x".into()));
        let r = std::sync::Arc::new(r);
        let won = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let (r, won) = (r.clone(), won.clone());
                std::thread::spawn(move || {
                    if r.claim_start() {
                        won.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(won.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(*r.status().borrow(), Status::Starting);
    }
}
