//! The daemon's event stream: a monotonic sequence of events, broadcast to
//! subscribers (the UI and clients) and kept in a bounded recent-history
//! buffer that a subscriber reads on connect. A session also records its own
//! events to its audit log as they are emitted.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use tokio::sync::broadcast;

use super::session::Session;
use super::{now, Event, EventBody, RECENT_EVENTS};

/// The live event stream and its recent history.
pub(crate) struct EventBus {
    tx: broadcast::Sender<Event>,
    recent: Mutex<VecDeque<Event>>,
    seq: AtomicU64,
}

impl EventBus {
    pub(crate) fn new() -> EventBus {
        let (tx, _) = broadcast::channel(1024);
        EventBus {
            tx,
            recent: Mutex::new(VecDeque::new()),
            seq: AtomicU64::new(1),
        }
    }

    /// Stamp `body` with the next sequence number and the time, record it on
    /// its session, keep it in recent history, and broadcast it.
    pub(crate) fn emit(&self, session: Option<&Session>, body: EventBody) {
        let event = Event {
            seq: self.seq.fetch_add(1, Ordering::Relaxed),
            ts: now(),
            session: session.map(|s| s.id.clone()),
            body,
        };
        if let Some(s) = session {
            s.record(&event);
        }
        {
            let mut recent = self.recent.lock().unwrap();
            if recent.len() >= RECENT_EVENTS {
                recent.pop_front();
            }
            recent.push_back(event.clone());
        }
        let _ = self.tx.send(event);
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
