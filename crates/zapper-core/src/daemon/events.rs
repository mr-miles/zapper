//! The daemon's events: the event model and the stream that carries it. A
//! monotonic sequence of events is broadcast to subscribers (the UI and
//! clients) and kept in a bounded recent-history buffer a subscriber reads on
//! connect, or resumes from after a reconnect; a session also records its own
//! events to its audit log.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use serde::Serialize;
use tokio::sync::broadcast;
use zapper_policy::proto::GuestEvent;
use zapper_policy::{Action, Kind};

use super::session::Session;
use super::{now, services};

/// How many recent events a subscriber reads on connect.
const RECENT_EVENTS: usize = 2000;

#[derive(Debug, Clone, Serialize)]
pub struct Event {
    pub seq: u64,
    /// Unix time, seconds.
    pub ts: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(flatten)]
    pub body: EventBody,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventBody {
    Session {
        state: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    Net {
        host: String,
        port: u16,
        #[serde(skip_serializing_if = "Option::is_none")]
        method: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        action: Action,
        #[serde(skip_serializing_if = "Option::is_none")]
        rule: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        scope: Option<String>,
        inspected: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        status: Option<u16>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        secrets: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    Exec {
        pid: i32,
        uid: u32,
        path: String,
        argv: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        action: Action,
        #[serde(skip_serializing_if = "Option::is_none")]
        rule: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        scope: Option<String>,
        /// Time the process waited for the decision.
        wait_ms: f64,
    },
    Ask {
        id: String,
        kind: Kind,
        subject: String,
    },
    Answer {
        id: String,
        allow: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        remember: Option<String>,
    },
    Dns {
        /// Empty when the query was rejected before its name was read.
        name: String,
        qtype: u16,
        allowed: bool,
        /// The response code: `noerror`, `nxdomain`, `refused`, `formerr`,
        /// `notimp` or `servfail`.
        rcode: String,
        /// Synthetic address given out.
        #[serde(skip_serializing_if = "Option::is_none")]
        answer: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    Guest {
        event: GuestEvent,
    },
    Shell {
        id: u64,
        state: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        argv: Option<Vec<String>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
    },
    Config {
        change: String,
    },
    /// A service or zone added or removed (`services`).
    Service {
        change: String,
        #[serde(flatten)]
        service: services::Info,
    },
    /// What a driven session's agent did: `started`, `turn_start`,
    /// `activity`, `steered`, `turn_end`, `exited`, `error`.
    Driver {
        event: String,
        detail: serde_json::Value,
    },
    /// A persistent session's disks, by file name, each its SHA-256:
    /// `stopped` (hashed once the guest is off), `checked` (the same at a
    /// start), `changed` (not the same: the start is refused), `accepted` (a
    /// change the operator let start) or `unverified` (no hashes from its
    /// last stop to compare with).
    Disks {
        state: String,
        disks: std::collections::BTreeMap<String, String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    /// An image build through the API (`builds`): `started`, `built` or
    /// `failed`.
    Image {
        name: String,
        build: u64,
        state: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    /// A tool called through the MCP gateway (`gateway`).
    Mcp {
        server: String,
        tool: String,
        ok: bool,
        ms: f64,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
}

/// The live event stream and its recent history.
pub(crate) struct EventBus {
    tx: broadcast::Sender<Event>,
    recent: Mutex<VecDeque<Event>>,
    seq: AtomicU64,
    /// This daemon run's ID: `seq` counts from 1 again after a restart, so a
    /// subscriber resumes from `EPOCH:SEQ`.
    epoch: String,
}

/// Where a subscriber left off: the epoch and the last `seq` it got, as
/// `EPOCH:SEQ` (an SSE `Last-Event-ID`, or `after=`).
#[derive(Debug, Clone, PartialEq)]
pub struct Cursor {
    pub epoch: String,
    pub seq: u64,
}

impl Cursor {
    pub fn parse(s: &str) -> Option<Cursor> {
        let (epoch, seq) = s.trim().split_once(':')?;
        if epoch.is_empty() || epoch.len() > 32 {
            return None;
        }
        Some(Cursor {
            epoch: epoch.to_string(),
            seq: seq.parse().ok()?,
        })
    }
}

/// What a subscriber is sent before live events: events from recent
/// history, and notices of what it can't be sent.
#[derive(Debug, Clone)]
pub enum Catchup {
    Event(Event),
    /// Events the subscriber missed and won't get: why, in words.
    Gap(String),
}

impl EventBus {
    pub(crate) fn new() -> EventBus {
        let (tx, _) = broadcast::channel(1024);
        EventBus {
            tx,
            recent: Mutex::new(VecDeque::new()),
            seq: AtomicU64::new(1),
            epoch: super::random_id(8),
        }
    }

    pub(crate) fn epoch(&self) -> &str {
        &self.epoch
    }

    /// What to send a subscriber before live events: after `after`, the
    /// events since it (and a gap if history no longer reaches back, or if
    /// the daemon has restarted since); else, with `history`, all of recent
    /// history; else nothing. Also, when something was sent, the last `seq`
    /// recent history held, so live events up to it, already sent, are
    /// skipped: subscribe first, then call this, and nothing falls between.
    pub(crate) fn catch_up(
        &self,
        session: Option<&str>,
        after: Option<&Cursor>,
        history: bool,
    ) -> (Vec<Catchup>, Option<u64>) {
        let recent = self.recent.lock().unwrap();
        let cut = recent.back().map(|e| e.seq).unwrap_or(0);
        let wanted = |e: &&Event| session.is_none() || e.session.as_deref() == session;
        let all = || recent.iter().filter(wanted).cloned().map(Catchup::Event);
        let mut out = Vec::new();
        match after {
            Some(c) if c.epoch == self.epoch => {
                if let Some(oldest) = recent.front().map(|e| e.seq) {
                    if oldest > c.seq.saturating_add(1) {
                        out.push(Catchup::Gap(format!(
                            "events {} to {} are no longer kept",
                            c.seq + 1,
                            oldest - 1
                        )));
                    }
                }
                out.extend(
                    recent
                        .iter()
                        .filter(wanted)
                        .filter(|e| e.seq > c.seq)
                        .cloned()
                        .map(Catchup::Event),
                );
            }
            Some(c) => {
                out.push(Catchup::Gap(format!(
                    "the daemon restarted: its events before run {} are gone (they were run {})",
                    self.epoch, c.epoch
                )));
                out.extend(all());
            }
            None if history => out.extend(all()),
            None => return (out, None),
        }
        (out, Some(cut))
    }

    /// Stamp `body` with the next sequence number and the time, record it on
    /// its session, keep it in recent history, and broadcast it.
    pub(crate) fn emit(&self, session: Option<&Session>, body: EventBody) {
        // Numbered, kept and sent under one lock, so history and the live
        // stream are in `seq` order, which resuming relies on: a subscriber
        // skips live events up to the last it was sent from history.
        let event = {
            let mut recent = self.recent.lock().unwrap();
            let event = Event {
                seq: self.seq.fetch_add(1, Ordering::Relaxed),
                ts: now(),
                session: session.map(|s| s.id.clone()),
                body,
            };
            if recent.len() >= RECENT_EVENTS {
                recent.pop_front();
            }
            recent.push_back(event.clone());
            let _ = self.tx.send(event.clone());
            event
        };
        if let Some(s) = session {
            s.record(&event);
        }
    }

    /// The events still in recent history: all of them, or one session's.
    pub(crate) fn recent(&self, session: Option<&str>) -> Vec<Event> {
        self.recent
            .lock()
            .unwrap()
            .iter()
            .filter(|e| session.is_none() || e.session.as_deref() == session)
            .cloned()
            .collect()
    }

    /// A receiver for the live stream.
    pub(crate) fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bus_with(n: usize) -> EventBus {
        let bus = EventBus::new();
        for _ in 0..n {
            bus.emit(
                None,
                EventBody::Session {
                    state: "x".into(),
                    detail: None,
                },
            );
        }
        bus
    }

    fn seqs(items: &[Catchup]) -> Vec<u64> {
        items
            .iter()
            .filter_map(|i| match i {
                Catchup::Event(e) => Some(e.seq),
                Catchup::Gap(_) => None,
            })
            .collect()
    }

    fn gaps(items: &[Catchup]) -> Vec<String> {
        items
            .iter()
            .filter_map(|i| match i {
                Catchup::Gap(g) => Some(g.clone()),
                Catchup::Event(_) => None,
            })
            .collect()
    }

    /// Emitted from many threads at once, events are kept, and sent live,
    /// in `seq` order, each once: what a resuming subscriber relies on.
    #[test]
    fn events_are_kept_and_sent_in_order() {
        let bus = std::sync::Arc::new(EventBus::new());
        let mut rx = bus.subscribe();
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let bus = bus.clone();
                std::thread::spawn(move || {
                    for _ in 0..100 {
                        bus.emit(
                            None,
                            EventBody::Session {
                                state: "x".into(),
                                detail: None,
                            },
                        );
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let kept: Vec<u64> = bus.recent(None).iter().map(|e| e.seq).collect();
        assert_eq!(kept, (1..=800).collect::<Vec<_>>());
        let mut live = Vec::new();
        while let Ok(e) = rx.try_recv() {
            live.push(e.seq);
        }
        assert_eq!(live, kept);
    }

    /// A cursor at the end of `u64` is neither a panic nor a made-up gap.
    #[test]
    fn a_cursor_at_the_end_of_u64_is_harmless() {
        let bus = bus_with(3);
        let after = Cursor {
            epoch: bus.epoch().into(),
            seq: u64::MAX,
        };
        let (items, _) = bus.catch_up(None, Some(&after), false);
        assert!(items.is_empty());
    }

    #[test]
    fn cursors_are_epoch_and_seq() {
        assert_eq!(
            Cursor::parse("ab3cd4ef:17"),
            Some(Cursor {
                epoch: "ab3cd4ef".into(),
                seq: 17
            })
        );
        for bad in ["", "17", ":17", "ab:", "ab:x", "ab:-1"] {
            assert_eq!(Cursor::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_subscriber_resumes_after_its_last_event() {
        let bus = bus_with(5);
        let after = Cursor {
            epoch: bus.epoch().into(),
            seq: 3,
        };
        let (items, cut) = bus.catch_up(None, Some(&after), false);
        assert_eq!(seqs(&items), vec![4, 5]);
        assert!(gaps(&items).is_empty());
        assert_eq!(cut, Some(5));
        // Live only: nothing before, and nothing skipped.
        let (items, cut) = bus.catch_up(None, None, false);
        assert!(items.is_empty());
        assert_eq!(cut, None);
    }

    #[test]
    fn what_history_no_longer_holds_is_a_gap() {
        let bus = bus_with(RECENT_EVENTS + 10);
        let after = Cursor {
            epoch: bus.epoch().into(),
            seq: 5,
        };
        let (items, _) = bus.catch_up(None, Some(&after), false);
        assert_eq!(gaps(&items), vec!["events 6 to 10 are no longer kept"]);
        assert_eq!(seqs(&items).first(), Some(&11));
    }

    #[test]
    fn another_epoch_is_a_restart() {
        let bus = bus_with(2);
        let after = Cursor {
            epoch: "older".into(),
            seq: 900,
        };
        let (items, cut) = bus.catch_up(None, Some(&after), false);
        assert_eq!(gaps(&items).len(), 1);
        assert!(
            gaps(&items)[0].contains("the daemon restarted"),
            "{:?}",
            gaps(&items)
        );
        assert_eq!(seqs(&items), vec![1, 2]);
        assert_eq!(cut, Some(2));
    }
}
