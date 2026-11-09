//! The post office: mail between the roster's workers, the board (you) and,
//! once relays come, outside addresses (`wa:…`, `tg:…`, `cron:…`).
//!
//! What it keeps from the office's: threads, a reply-by time with
//! follow-ups, the loop and fan-out guards, and size limits. What it drops:
//! kinds, authority, teams and the chart. Anyone on the roster may mail
//! anyone.
//!
//! It decides; it doesn't keep. Every change is an event (`events`),
//! committed as a batch: journaled (one line, unless the office keeps no
//! event journal), then applied to the store (`store`), which is all the
//! state there is but the fan-out guard's recent sends.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;
use serde_json::{json, Value};

use crate::events::{Event, VERSION};
use crate::journal::Journal;
use crate::store::{number, Store};
pub use crate::store::{Meta, Thread};

/// You, as the roster's workers address you.
pub const BOARD: &str = "board";
/// The post office itself, as the sender of its notices.
pub const OFFICE: &str = "office";

pub const MAX_BODY: usize = 64 * 1024;
pub const MAX_SUBJECT: usize = 200;
pub const MAX_RECIPIENTS: usize = 16;
/// Messages one thread may hold.
pub const MAX_THREAD: usize = 200;
/// Messages workers may exchange in a thread before the board or an outside
/// address writes in it again: the loop guard.
pub const MAX_UNATTENDED: usize = 40;
/// Messages one sender may send in `RATE_WINDOW` seconds: the fan-out guard.
pub const MAX_PER_WINDOW: usize = 60;
pub const RATE_WINDOW: f64 = 600.0;
/// A reply-by time is at least this far ahead.
pub const MIN_DUE_SECS: f64 = 60.0;
/// Reminders the post office sends before it tells the sender nobody
/// answered.
pub const MAX_FOLLOW_UPS: u32 = 2;
/// The longest outside address (`ns:id`).
const MAX_ADDRESS: usize = 128;

/// A message to send.
#[derive(Debug, Clone, Default)]
pub struct Draft {
    pub from: String,
    pub to: Vec<String>,
    /// Its subject; from the body's first line if empty.
    pub subject: String,
    pub body: String,
    /// The message it answers: the reply joins that one's thread.
    pub in_reply_to: Option<String>,
    /// When an answer is due (seconds since the epoch).
    pub due: Option<f64>,
}

/// What `send` did: the message's id and thread, and the roster workers to
/// wake (the board and outside addresses aren't woken).
#[derive(Debug, Clone, PartialEq)]
pub struct Sent {
    pub id: String,
    pub thread: String,
    pub wake: Vec<String>,
    /// Whether it went to the board.
    pub to_board: bool,
}

/// One worker's (or the board's) desk: what they wait for, and what is
/// awaited of them.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Desk {
    /// Their messages whose answer is due and hasn't come.
    pub waiting: Vec<Value>,
    /// Messages to them whose answer is due and they haven't given.
    pub owed: Vec<Value>,
}

/// Who an address is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Address {
    Worker(String),
    Board,
    /// An outside address of a relay's namespace.
    Outside {
        ns: String,
        id: String,
    },
}

pub struct PostOffice {
    roster: BTreeSet<String>,
    relays: BTreeSet<String>,
    store: Box<dyn Store>,
    /// Whether events are journaled (the office's `store`: `both`).
    journaled: bool,
    /// Who → when they last sent, for the fan-out guard.
    sent_at: HashMap<String, VecDeque<f64>>,
    next: u64,
}

/// `s` cut to `n` bytes on a character boundary.
fn clip(s: &str, n: usize) -> String {
    let mut end = n.min(s.len());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// A subject from a body: its first non-empty line, clipped.
pub fn subject_from(body: &str) -> String {
    let line = body.lines().map(str::trim).find(|l| !l.is_empty());
    let s = clip(line.unwrap_or("(no subject)"), 80);
    if s.len() < line.map_or(0, str::len) {
        format!("{s}…")
    } else {
        s
    }
}

/// Applies the journal's event batches after the store's last to it: the
/// ones a crash kept from it, or all of them to a new store. Returns how
/// many batches it applied.
pub fn catch_up(store: &mut dyn Store, journal: &Path) -> Result<usize> {
    use std::io::BufRead as _;
    let Ok(f) = std::fs::File::open(zapper_record::view(journal)) else {
        return Ok(0);
    };
    let after = store.last_seq();
    let mut n = 0;
    // A line at a time: a long journal isn't taken into memory.
    let lines = std::io::BufReader::new(f).lines().map_while(Result::ok);
    for line in lines.filter(|l| l.contains("\"type\":\"events\"")) {
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let seq = v["seq"].as_u64().unwrap_or(0);
        if v["type"] != "events" || seq <= after {
            continue;
        }
        if v["v"].as_u64().unwrap_or(0) > VERSION as u64 {
            bail!(
                "the journal's events at seq {seq} are of a newer version ({})",
                v["v"]
            );
        }
        let events: Vec<Event> = serde_json::from_value(v["events"].clone())
            .with_context(|| format!("the journal's events at seq {seq}"))?;
        store.apply(seq, &events)?;
        n += 1;
    }
    Ok(n)
}

impl PostOffice {
    /// A post office for `roster`, taking outside addresses of `relays`'
    /// namespaces, its state in `store`, its events journaled if
    /// `journaled`.
    pub fn new(
        roster: impl IntoIterator<Item = String>,
        relays: impl IntoIterator<Item = String>,
        store: Box<dyn Store>,
        journaled: bool,
    ) -> PostOffice {
        let next = store.next_number();
        PostOffice {
            roster: roster.into_iter().collect(),
            relays: relays.into_iter().collect(),
            store,
            journaled,
            sent_at: HashMap::new(),
            next,
        }
    }

    /// Commits `events` as one batch: journaled, then applied.
    pub fn commit(&mut self, journal: &Journal, events: Vec<Event>) -> Result<()> {
        if events.is_empty() {
            return Ok(());
        }
        let seq = if self.journaled {
            let at = journal.append("events", json!({ "v": VERSION, "events": events }));
            if !at.written() {
                bail!("the journal can't be written; nothing was changed");
            }
            at.seq
        } else {
            self.store.last_seq() + 1
        };
        self.store.apply(seq, &events)
    }

    /// What `s` addresses, if anyone.
    pub fn address(&self, s: &str) -> Result<Address> {
        let s = s.trim();
        if s == BOARD {
            return Ok(Address::Board);
        }
        if self.roster.contains(s) {
            return Ok(Address::Worker(s.to_string()));
        }
        if let Some((ns, id)) = s.split_once(':') {
            if !self.relays.contains(ns) {
                bail!("no relay for {ns}: addresses");
            }
            let plain = |c: char| c.is_ascii_alphanumeric() || "+-_.@".contains(c);
            if id.is_empty() || s.len() > MAX_ADDRESS || !id.chars().all(plain) {
                bail!("{s:?} isn't a {ns}: address");
            }
            return Ok(Address::Outside {
                ns: ns.to_string(),
                id: id.to_string(),
            });
        }
        bail!(
            "nobody is {s:?}: mail a worker ({}) or {BOARD}",
            self.roster.iter().cloned().collect::<Vec<_>>().join(", ")
        )
    }

    /// Whether `who` may send: a worker, the board, the office, or an
    /// outside address (from its relay).
    fn may_send(&self, who: &str) -> Result<()> {
        if who == OFFICE {
            return Ok(());
        }
        self.address(who).map(|_| ())
    }

    /// Sends `d` at `now`: checked, then committed.
    pub fn send(&mut self, journal: &Journal, d: Draft, now: f64) -> Result<Sent> {
        let (sent, event) = self.decide_send(d, now)?;
        self.commit(journal, vec![event])?;
        self.next += 1;
        if self.roster.contains(&sent.from()) {
            self.sent_at.entry(sent.from()).or_default().push_back(now);
        }
        Ok(sent.sent)
    }

    /// The event that sends `d`, if the rules allow it.
    fn decide_send(&mut self, d: Draft, now: f64) -> Result<(Sending, Event)> {
        self.may_send(&d.from)?;
        if d.body.len() > MAX_BODY {
            bail!("the body is {} bytes; at most {MAX_BODY}", d.body.len());
        }
        if d.subject.len() > MAX_SUBJECT {
            bail!(
                "the subject is {} bytes; at most {MAX_SUBJECT}",
                d.subject.len()
            );
        }
        let mut to = Vec::new();
        for t in &d.to {
            let name = match self.address(t)? {
                Address::Worker(n) => n,
                Address::Board => BOARD.to_string(),
                Address::Outside { ns, id } => format!("{ns}:{id}"),
            };
            if name != d.from && !to.contains(&name) {
                to.push(name);
            }
        }
        if to.is_empty() {
            bail!("nobody to send it to (not even yourself)");
        }
        if to.len() > MAX_RECIPIENTS {
            bail!("{} recipients; at most {MAX_RECIPIENTS}", to.len());
        }
        let worker_sender = self.roster.contains(&d.from);
        let replied = match &d.in_reply_to {
            Some(r) => {
                let m = self
                    .store
                    .message(r)
                    .filter(|m| self.can_read(&d.from, &m.thread))
                    .ok_or_else(|| anyhow!("no message {r}"))?;
                self.store.thread(&m.thread)
            }
            None => None,
        };
        if let Some(t) = &replied {
            if t.messages.len() >= MAX_THREAD {
                bail!("thread {} has {MAX_THREAD} messages; start a new one", t.id);
            }
            if worker_sender && t.unattended >= MAX_UNATTENDED {
                bail!(
                    "thread {} has had {MAX_UNATTENDED} messages among workers alone; ask {BOARD} before going on",
                    t.id
                );
            }
        }
        if worker_sender {
            let times = self.sent_at.entry(d.from.clone()).or_default();
            while times.front().is_some_and(|t| now - t > RATE_WINDOW) {
                times.pop_front();
            }
            if times.len() >= MAX_PER_WINDOW {
                bail!(
                    "{MAX_PER_WINDOW} messages in {} minutes; wait before sending more",
                    RATE_WINDOW / 60.0
                );
            }
        }
        let due = match d.due {
            Some(t) if t < now + MIN_DUE_SECS => {
                bail!("a reply-by time is at least {MIN_DUE_SECS} seconds ahead")
            }
            other => other,
        };
        let subject = if d.subject.trim().is_empty() {
            subject_from(&d.body)
        } else {
            d.subject.trim().to_string()
        };
        let id = format!("m-{}", self.next);
        let (thread, new_thread) = match &replied {
            Some(t) => (t.id.clone(), None),
            None => (format!("t-{}", self.next), Some(subject.clone())),
        };
        // Answered: by one it was to, to the one who asked.
        let settles: Vec<String> = self
            .store
            .open()
            .into_iter()
            .filter(|m| m.thread == thread && m.to.contains(&d.from) && to.contains(&m.from))
            .map(|m| m.id)
            .collect();
        let before = replied.as_ref().map_or(0, |t| t.unattended);
        // The post office's notices neither count nor reset it.
        let unattended = if worker_sender {
            before + 1
        } else if d.from == OFFICE {
            before
        } else {
            0
        };
        let wake: Vec<String> = to
            .iter()
            .filter(|r| self.roster.contains(*r))
            .cloned()
            .collect();
        let to_board = to.iter().any(|r| r == BOARD);
        let sending = Sending {
            from: d.from.clone(),
            sent: Sent {
                id: id.clone(),
                thread: thread.clone(),
                wake,
                to_board,
            },
        };
        let event = Event::MessageSent {
            id,
            thread,
            from: d.from,
            to,
            subject,
            body: d.body,
            ts: now,
            in_reply_to: d.in_reply_to,
            due,
            new_thread,
            unattended,
            settles,
        };
        Ok((sending, event))
    }

    /// Whether `viewer` may read thread `thread`: the board reads all, and
    /// the post office writes its reminders into any.
    pub fn can_read(&self, viewer: &str, thread: &str) -> bool {
        viewer == BOARD
            || viewer == OFFICE
            || self
                .store
                .thread(thread)
                .is_some_and(|t| t.participants.contains(viewer))
    }

    pub fn meta(&self, id: &str) -> Option<Meta> {
        self.store.message(id)
    }

    pub fn body(&self, id: &str) -> String {
        self.store.body(id).unwrap_or_default()
    }

    /// Message `id` as `viewer` reads it, body and all.
    pub fn message(&self, id: &str, viewer: &str) -> Result<Value> {
        let m = self
            .store
            .message(id)
            .filter(|m| self.can_read(viewer, &m.thread))
            .ok_or_else(|| anyhow!("no message {id}"))?;
        let mut v = serde_json::to_value(&m)?;
        v["body"] = json!(self.body(id));
        Ok(v)
    }

    /// Thread `id` as `viewer` reads it: every message, bodies and all.
    pub fn thread(&self, id: &str, viewer: &str) -> Result<Value> {
        let t = self
            .store
            .thread(id)
            .filter(|_| self.can_read(viewer, id))
            .ok_or_else(|| anyhow!("no thread {id}"))?;
        let messages: Vec<Value> = t
            .messages
            .iter()
            .filter_map(|m| self.message(m, viewer).ok())
            .collect();
        Ok(
            json!({ "id": t.id, "subject": t.subject, "participants": t.participants, "messages": messages }),
        )
    }

    /// Every thread, most recent first, as a line each.
    pub fn threads(&self) -> Vec<Value> {
        self.store
            .threads()
            .into_iter()
            .map(|t| json!({ "id": t.id, "subject": t.subject, "participants": t.participants, "messages": t.messages.len(), "last": t.last }))
            .collect()
    }

    /// `who` has read `ids`.
    pub fn mark_read(&mut self, journal: &Journal, who: &str, ids: &[String]) -> Result<()> {
        let unread = self.unread(who);
        let ids: Vec<String> = ids.iter().filter(|i| unread.contains(i)).cloned().collect();
        if ids.is_empty() {
            return Ok(());
        }
        self.commit(
            journal,
            vec![Event::Read {
                who: who.into(),
                ids,
            }],
        )
    }

    pub fn unread(&self, who: &str) -> Vec<String> {
        self.store.unread(who)
    }

    /// `who`'s mail, newest first: a line each, at most `limit`.
    pub fn inbox(&self, who: &str, unread_only: bool, limit: usize) -> Vec<Value> {
        let unread = self.unread(who);
        self.store
            .inbox(who, unread_only, limit)
            .into_iter()
            .map(|m| json!({ "id": m.id, "thread": m.thread, "from": m.from, "subject": m.subject, "ts": m.ts, "unread": unread.contains(&m.id), "due": m.due }))
            .collect()
    }

    /// `who`'s desk.
    pub fn desk(&self, who: &str) -> Desk {
        let mut d = Desk::default();
        for m in self.store.open() {
            let line = json!({ "id": m.id, "thread": m.thread, "subject": m.subject, "from": m.from, "to": m.to, "due": m.due });
            if m.from == who {
                d.waiting.push(line.clone());
            }
            if m.to.iter().any(|t| t == who) {
                d.owed.push(line);
            }
        }
        d
    }

    /// Follow-ups due at `now`: a reminder to whoever hasn't answered, up to
    /// `MAX_FOLLOW_UPS`, then word to the sender that nobody did. Returns
    /// what was sent.
    pub fn tick(&mut self, journal: &Journal, now: f64) -> Vec<Sent> {
        let late: Vec<Meta> = self
            .store
            .open()
            .into_iter()
            .filter(|m| m.due.is_some_and(|d| d <= now))
            .collect();
        let mut sent = Vec::new();
        for m in late {
            let (event, notice) = if m.follow_ups < MAX_FOLLOW_UPS {
                let interval = m.wait.max(MIN_DUE_SECS);
                let to: Vec<String> = m.to.iter().filter(|t| !t.contains(':')).cloned().collect();
                let notice = (!to.is_empty()).then(|| Draft {
                    from: OFFICE.into(),
                    to,
                    subject: format!("Reminder: {}", m.subject),
                    body: format!("[Automatic reminder] {} is waiting for your answer to {} (\"{}\"). Reply to it with mail_reply.", m.from, m.id, m.subject),
                    in_reply_to: Some(m.id.clone()),
                    due: None,
                });
                let e = Event::FollowedUp {
                    id: m.id.clone(),
                    follow_ups: m.follow_ups + 1,
                    due: now + interval,
                };
                (e, notice)
            } else {
                let notice = (m.from != OFFICE && !m.from.contains(':')).then(|| Draft {
                    from: OFFICE.into(),
                    to: vec![m.from.clone()],
                    subject: format!("No answer: {}", m.subject),
                    body: format!("[Automatic notice] Nobody answered {} (\"{}\") after {MAX_FOLLOW_UPS} reminders. Decide what to do without it, or ask someone else.", m.id, m.subject),
                    in_reply_to: Some(m.id.clone()),
                    due: None,
                });
                (
                    Event::Settled {
                        ids: vec![m.id.clone()],
                    },
                    notice,
                )
            };
            // The follow-up and its notice, as one batch.
            let mut events = vec![event];
            let mut out = None;
            if let Some(d) = notice {
                match self.decide_send(d, now) {
                    Ok((s, e)) => {
                        events.push(e);
                        out = Some(s.sent);
                    }
                    Err(e) => tracing::warn!("the follow-up of {}: {e:#}", m.id),
                }
            }
            match self.commit(journal, events) {
                Ok(()) => {
                    if let Some(s) = out {
                        self.next += 1;
                        sent.push(s);
                    }
                }
                Err(e) => tracing::warn!("the follow-up of {}: {e:#}", m.id),
            }
        }
        sent
    }

    /// The roster and the board, as the workers see them.
    pub fn directory(&self, about: &BTreeMap<String, String>) -> Value {
        let workers: Vec<Value> = self
            .roster
            .iter()
            .map(|n| json!({ "name": n, "about": about.get(n) }))
            .collect();
        json!({ "workers": workers, "board": BOARD, "relays": self.relays })
    }

    /// Mail to outside addresses, for their relays.
    pub fn outbound(&self) -> Vec<String> {
        self.store.outbound()
    }

    // --- turns: what each worker was shown, for a restart -------------------

    /// Starts `worker`'s next turn, showing it `ids`: the turn's number.
    pub fn start_turn(&mut self, journal: &Journal, worker: &str, ids: &[String]) -> Result<u64> {
        let turn = self.store.next_turn(worker);
        let mut events = vec![Event::TurnStarted {
            worker: worker.into(),
            turn,
        }];
        if !ids.is_empty() {
            events.push(Event::Shown {
                worker: worker.into(),
                turn,
                ids: ids.to_vec(),
            });
        }
        self.commit(journal, events)?;
        Ok(turn)
    }

    /// `ids` were shown to `worker` during turn `turn`.
    pub fn show(
        &mut self,
        journal: &Journal,
        worker: &str,
        turn: u64,
        ids: &[String],
    ) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        self.commit(
            journal,
            vec![Event::Shown {
                worker: worker.into(),
                turn,
                ids: ids.to_vec(),
            }],
        )
    }

    /// Ends `worker`'s turn `turn`; `again` are its mail to show again.
    pub fn end_turn(
        &mut self,
        journal: &Journal,
        worker: &str,
        turn: u64,
        again: &[String],
    ) -> Result<()> {
        let mut events = Vec::new();
        if !again.is_empty() {
            events.push(Event::Unread {
                who: worker.into(),
                ids: again.to_vec(),
            });
        }
        events.push(Event::TurnEnded {
            worker: worker.into(),
            turn,
        });
        self.commit(journal, events)
    }

    /// Records `worker`'s agent's conversation.
    pub fn set_conversation(&mut self, journal: &Journal, worker: &str, id: &str) -> Result<()> {
        if self.store.conversation(worker).as_deref() == Some(id) {
            return Ok(());
        }
        self.commit(
            journal,
            vec![Event::Conversation {
                worker: worker.into(),
                id: id.into(),
            }],
        )
    }

    pub fn conversation(&self, worker: &str) -> Option<String> {
        self.store.conversation(worker)
    }

    /// After a restart: the turns of `worker`'s that never ended are ended,
    /// their mail unread again. Returns that mail.
    pub fn recover(&mut self, journal: &Journal, worker: &str) -> Result<Vec<String>> {
        let mut again = Vec::new();
        for (turn, ids) in self.store.open_turns(worker) {
            self.end_turn(journal, worker, turn, &ids)?;
            again.extend(ids);
        }
        again.sort_by_key(|i| number(i));
        again.dedup();
        Ok(again)
    }
}

/// A message about to be sent.
struct Sending {
    from: String,
    sent: Sent,
}

impl Sending {
    fn from(&self) -> String {
        self.from.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Memory, Sqlite};

    /// Runs `f` on a post office with each store, its journal in a new
    /// directory.
    fn each(f: impl Fn(&mut PostOffice, &Journal)) {
        for (name, store) in [
            ("memory", Box::new(Memory::new()) as Box<dyn Store>),
            ("sqlite", Box::new(Sqlite::in_memory())),
        ] {
            let dir = std::env::temp_dir().join(format!("zc-mail-{}", zapper_util::random_id(8)));
            std::fs::create_dir_all(&dir).unwrap();
            let j = Journal::create(&dir.join("journal.jsonl")).unwrap();
            let mut po = PostOffice::new(["ada".into(), "bob".into()], ["wa".into()], store, true);
            eprintln!("store: {name}");
            f(&mut po, &j);
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    fn draft(from: &str, to: &[&str], body: &str) -> Draft {
        Draft {
            from: from.into(),
            to: to.iter().map(|s| s.to_string()).collect(),
            body: body.into(),
            ..Draft::default()
        }
    }

    fn reply(to_id: &str, from: &str, to: &[&str], body: &str) -> Draft {
        Draft {
            in_reply_to: Some(to_id.into()),
            ..draft(from, to, body)
        }
    }

    #[test]
    fn mail_is_journaled_threaded_and_wakes_its_workers() {
        each(|po, j| {
            let s = po
                .send(j, draft("board", &["ada", "bob"], "Plan\nmore"), 100.0)
                .unwrap();
            assert_eq!(s.wake, vec!["ada", "bob"]);
            assert_eq!(po.meta(&s.id).unwrap().subject, "Plan");
            assert_eq!(po.body(&s.id), "Plan\nmore");
            let r = po
                .send(j, reply(&s.id, "ada", &["board"], "ok"), 101.0)
                .unwrap();
            assert_eq!(r.thread, s.thread);
            assert!(r.wake.is_empty() && r.to_board, "the board isn't woken");
            assert_eq!(po.unread("board"), vec![r.id.clone()]);
            let t = po.thread(&s.thread, "bob").unwrap();
            assert_eq!(t["messages"].as_array().unwrap().len(), 2);
            assert_eq!(po.threads()[0]["messages"], 2);
        });
    }

    #[test]
    fn only_known_addresses_and_bounded_messages() {
        each(|po, j| {
            assert!(po.send(j, draft("ada", &["eve"], "x"), 1.0).is_err());
            assert!(po.send(j, draft("eve", &["ada"], "x"), 1.0).is_err());
            assert!(po.send(j, draft("ada", &["ada"], "x"), 1.0).is_err());
            assert!(po.send(j, draft("ada", &["tg:1"], "x"), 1.0).is_err());
            assert!(po.send(j, draft("ada", &["wa:+44 7"], "x"), 1.0).is_err());
            let big = "x".repeat(MAX_BODY + 1);
            assert!(po.send(j, draft("ada", &["bob"], &big), 1.0).is_err());
            let s = po
                .send(j, draft("ada", &["wa:+447700900123"], "hi"), 1.0)
                .unwrap();
            assert!(s.wake.is_empty());
            assert_eq!(po.outbound(), vec![s.id]);
        });
    }

    #[test]
    fn a_thread_is_read_only_by_its_participants_and_the_board() {
        each(|po, j| {
            let s = po.send(j, draft("ada", &["board"], "secret"), 1.0).unwrap();
            assert!(po.message(&s.id, "bob").is_err());
            assert!(po.message(&s.id, "board").is_ok());
            assert!(
                po.send(j, reply(&s.id, "bob", &["ada"], "let me in"), 2.0)
                    .is_err(),
                "no replying into a thread you can't read"
            );
        });
    }

    #[test]
    fn workers_alone_in_a_thread_are_stopped_until_the_board_writes() {
        each(|po, j| {
            let first = po
                .send(j, draft("board", &["ada", "bob"], "go"), 0.0)
                .unwrap();
            let mut last = first.id;
            let mut now = 1.0;
            for i in 0..MAX_UNATTENDED {
                let (from, to) = if i % 2 == 0 {
                    ("ada", "bob")
                } else {
                    ("bob", "ada")
                };
                // Spread out, so the fan-out guard stays out of it.
                now += RATE_WINDOW / 20.0;
                last = po
                    .send(j, reply(&last, from, &[to], "again"), now)
                    .unwrap()
                    .id;
            }
            let e = po
                .send(j, reply(&last, "ada", &["bob"], "again"), now + 1.0)
                .unwrap_err()
                .to_string();
            assert!(e.contains("ask board"), "{e}");
            let b = po
                .send(j, reply(&last, "board", &["ada"], "carry on"), now + 2.0)
                .unwrap();
            assert!(po
                .send(j, reply(&b.id, "ada", &["bob"], "again"), now + 3.0)
                .is_ok());
        });
    }

    #[test]
    fn a_sender_is_held_to_a_rate() {
        each(|po, j| {
            for i in 0..MAX_PER_WINDOW {
                po.send(j, draft("ada", &["bob"], &format!("n{i}")), 10.0)
                    .unwrap();
            }
            assert!(po.send(j, draft("ada", &["bob"], "more"), 10.0).is_err());
            assert!(po
                .send(j, draft("ada", &["bob"], "later"), 10.0 + RATE_WINDOW + 1.0)
                .is_ok());
            assert!(po
                .send(j, draft("board", &["bob"], "not limited"), 10.0)
                .is_ok());
        });
    }

    #[test]
    fn a_due_answer_is_on_both_desks_until_given_and_is_followed_up() {
        each(|po, j| {
            let ask = Draft {
                due: Some(200.0),
                ..draft("ada", &["bob"], "numbers?")
            };
            let soon = Draft {
                due: Some(30.0),
                ..ask.clone()
            };
            assert!(po.send(j, soon, 0.0).is_err(), "too soon");
            let s = po.send(j, ask, 0.0).unwrap();
            assert_eq!(po.desk("ada").waiting.len(), 1);
            assert_eq!(po.desk("bob").owed.len(), 1);
            assert!(po.tick(j, 100.0).is_empty(), "not due yet");
            let f = po.tick(j, 200.0);
            assert_eq!(f.len(), 1);
            assert_eq!(f[0].wake, vec!["bob"]);
            assert_eq!(f[0].thread, s.thread);
            assert_eq!(po.tick(j, 300.0).len(), 0, "the next is a due interval on");
            assert_eq!(po.tick(j, 400.0).len(), 1);
            let gave_up = po.tick(j, 600.0);
            assert_eq!(
                gave_up[0].wake,
                vec!["ada"],
                "the sender hears nobody answered"
            );
            assert!(po.desk("bob").owed.is_empty());
        });
    }

    #[test]
    fn an_answer_settles_what_was_owed_to_who_asked() {
        each(|po, j| {
            let s = po
                .send(
                    j,
                    Draft {
                        due: Some(500.0),
                        ..draft("ada", &["bob"], "numbers?")
                    },
                    0.0,
                )
                .unwrap();
            po.send(j, reply(&s.id, "bob", &["board"], "not to ada"), 1.0)
                .unwrap();
            assert_eq!(po.desk("ada").waiting.len(), 1, "an answer to someone else");
            po.send(j, reply(&s.id, "bob", &["ada"], "42"), 2.0)
                .unwrap();
            assert!(po.desk("ada").waiting.is_empty());
            assert!(po.tick(j, 1000.0).is_empty());
        });
    }

    #[test]
    fn the_inbox_is_newest_first_and_marks_unread() {
        each(|po, j| {
            let a = po.send(j, draft("board", &["ada"], "one"), 1.0).unwrap();
            let b = po.send(j, draft("bob", &["ada"], "two"), 2.0).unwrap();
            assert_eq!(po.inbox("ada", false, 10)[0]["id"], json!(b.id));
            po.mark_read(j, "ada", std::slice::from_ref(&b.id)).unwrap();
            let unread = po.inbox("ada", true, 10);
            assert_eq!(unread.len(), 1);
            assert_eq!(unread[0]["id"], json!(a.id));
        });
    }

    #[test]
    fn mail_the_journal_cant_keep_isnt_sent() {
        each(|po, j| {
            j.fail_writes();
            let e = po
                .send(j, draft("ada", &["bob"], "lost?"), 1.0)
                .unwrap_err();
            assert!(e.to_string().contains("journal"), "{e}");
            assert!(po.unread("bob").is_empty());
            assert!(po.meta("m-1").is_none());
        });
    }

    #[test]
    fn a_turn_never_ended_gives_its_mail_back_on_recovery() {
        each(|po, j| {
            let a = po.send(j, draft("board", &["ada"], "one"), 1.0).unwrap();
            let b = po.send(j, draft("board", &["ada"], "two"), 2.0).unwrap();
            let t1 = po
                .start_turn(j, "ada", std::slice::from_ref(&a.id))
                .unwrap();
            po.end_turn(j, "ada", t1, &[]).unwrap();
            let t2 = po
                .start_turn(j, "ada", std::slice::from_ref(&b.id))
                .unwrap();
            assert_eq!(t2, t1 + 1);
            assert!(po.unread("ada").is_empty(), "shown is read");
            // The office stops here; started again:
            assert_eq!(po.recover(j, "ada").unwrap(), vec![b.id.clone()]);
            assert_eq!(po.unread("ada"), vec![b.id.clone()]);
            assert!(po.recover(j, "ada").unwrap().is_empty(), "once");
            // A turn that ends asking for mail again leaves it unread.
            let t3 = po
                .start_turn(j, "ada", std::slice::from_ref(&b.id))
                .unwrap();
            po.end_turn(j, "ada", t3, std::slice::from_ref(&b.id))
                .unwrap();
            assert_eq!(po.unread("ada"), vec![b.id.clone()]);
            po.set_conversation(j, "ada", "c-1").unwrap();
            assert_eq!(po.conversation("ada").as_deref(), Some("c-1"));
        });
    }

    /// The point of the events: a store made from the journal alone is the
    /// store the post office kept.
    #[test]
    fn a_store_made_again_from_the_journal_answers_the_same() {
        let dir = std::env::temp_dir().join(format!("zc-replay-{}", zapper_util::random_id(8)));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("journal.jsonl");
        let j = Journal::create(&path).unwrap();
        let mut po = PostOffice::new(
            ["ada".into(), "bob".into()],
            ["wa".into()],
            Box::new(Sqlite::open(&dir.join("office.db")).unwrap()),
            true,
        );
        let s = po
            .send(
                &j,
                Draft {
                    due: Some(100.0),
                    ..draft("ada", &["bob", "wa:+44"], "numbers?")
                },
                0.0,
            )
            .unwrap();
        po.send(&j, reply(&s.id, "board", &["ada"], "go on"), 1.0)
            .unwrap();
        po.tick(&j, 100.0);
        let t = po
            .start_turn(&j, "bob", std::slice::from_ref(&s.id))
            .unwrap();
        po.set_conversation(&j, "bob", "c-9").unwrap();
        let _ = t;
        for mut fresh in [
            Box::new(Memory::new()) as Box<dyn Store>,
            Box::new(Sqlite::in_memory()),
        ] {
            catch_up(fresh.as_mut(), &path).unwrap();
            let again = PostOffice::new(["ada".into(), "bob".into()], ["wa".into()], fresh, true);
            for who in ["ada", "bob", "board"] {
                assert_eq!(
                    again.inbox(who, false, 50),
                    po.inbox(who, false, 50),
                    "{who}'s inbox"
                );
                assert_eq!(again.unread(who), po.unread(who), "{who}'s unread");
                assert_eq!(
                    serde_json::to_value(again.desk(who)).unwrap(),
                    serde_json::to_value(po.desk(who)).unwrap(),
                    "{who}'s desk"
                );
            }
            assert_eq!(again.threads(), po.threads());
            assert_eq!(
                again.thread(&s.thread, "board").unwrap(),
                po.thread(&s.thread, "board").unwrap()
            );
            assert_eq!(again.outbound(), po.outbound());
            assert_eq!(again.conversation("bob").as_deref(), Some("c-9"));
            assert_eq!(again.store.open_turns("bob"), po.store.open_turns("bob"));
            assert_eq!(again.next, po.next, "numbering carries on");
        }
        // A database behind the journal (a crash between the two) catches up.
        let mut behind = Sqlite::in_memory();
        catch_up(&mut behind, &path).unwrap();
        let caught = behind.last_seq();
        po.send(&j, draft("bob", &["ada"], "after"), 2.0).unwrap();
        assert_eq!(
            catch_up(&mut behind, &path).unwrap(),
            1,
            "only what it missed"
        );
        assert!(behind.last_seq() > caught);
        let _ = std::fs::remove_dir_all(dir);
    }
}
