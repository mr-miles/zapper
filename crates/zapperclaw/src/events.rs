//! The post office's events: the only way its state changes. The post
//! office decides them (the rules, the clock, the numbering) and a store
//! applies them, doing nothing but what they say, so the same events give
//! the same state in any store, and again on replay.
//!
//! A batch of events is one journal line (`{"type": "events", "events":
//! [...]}`), with the journal's `seq`; the database records the last `seq`
//! it applied, so it catches up from the journal after a crash, and can be
//! made again from it.

use serde::{Deserialize, Serialize};

/// The events' format; a store reads every version up to this.
pub const VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// A message, as sent: to the board, workers, outside addresses.
    MessageSent {
        id: String,
        thread: String,
        from: String,
        to: Vec<String>,
        subject: String,
        body: String,
        ts: f64,
        #[serde(default)]
        in_reply_to: Option<String>,
        #[serde(default)]
        due: Option<f64>,
        /// The thread's subject, if this message starts it.
        #[serde(default)]
        new_thread: Option<String>,
        /// The thread's count of messages among workers alone, after this.
        unattended: usize,
        /// Messages whose awaited answer this is.
        #[serde(default)]
        settles: Vec<String>,
    },
    /// `who` has read `ids`.
    Read { who: String, ids: Vec<String> },
    /// `ids` are `who`'s to read again (a turn that didn't take them).
    Unread { who: String, ids: Vec<String> },
    /// A reminder about `id` was sent; the next is due at `due`.
    FollowedUp {
        id: String,
        follow_ups: u32,
        due: f64,
    },
    /// The answers to `ids` are no longer awaited.
    Settled { ids: Vec<String> },
    /// Worker `worker`'s turn `turn` began.
    TurnStarted { worker: String, turn: u64 },
    /// `ids` were shown to `worker` in turn `turn` (and so read).
    Shown {
        worker: String,
        turn: u64,
        ids: Vec<String>,
    },
    /// The turn ended, whatever its outcome.
    TurnEnded { worker: String, turn: u64 },
    /// Worker `worker`'s agent's conversation is `id`.
    Conversation { worker: String, id: String },
}
