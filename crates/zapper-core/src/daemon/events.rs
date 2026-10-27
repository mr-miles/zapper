//! The daemon's events: the event model and the stream that carries it. A
//! monotonic sequence of events is broadcast to subscribers (the UI and
//! clients) and kept in a bounded recent-history buffer a subscriber reads on
//! connect; a session also records its own events to its audit log.

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
