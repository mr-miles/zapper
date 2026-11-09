//! The post office: mail between the roster's workers, the board (you) and,
//! once relays come, outside addresses (`wa:…`, `tg:…`, `cron:…`).
//!
//! What it keeps from the office's: threads, the journal as the record (a
//! message's body is read back from it), a reply-by time with follow-ups,
//! the loop and fan-out guards, and size limits. What it drops: kinds,
//! authority, teams and the chart. Anyone on the roster may mail anyone.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

use anyhow::{anyhow, bail, Result};
use serde::Serialize;
use serde_json::{json, Value};

use crate::journal::{At, Journal};

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

/// A message the post office holds; its body stays in the journal.
#[derive(Debug, Clone, Serialize)]
pub struct Meta {
    pub id: String,
    pub thread: String,
    pub from: String,
    pub to: Vec<String>,
    pub subject: String,
    pub ts: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_reply_to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub due: Option<f64>,
    /// Reminders sent about it.
    #[serde(skip)]
    pub follow_ups: u32,
    /// How long its sender gave for an answer: the time between reminders.
    #[serde(skip)]
    wait: f64,
    /// Its answer is no longer awaited (answered, or given up on).
    #[serde(skip)]
    pub settled: bool,
    #[serde(skip)]
    at: At,
}

#[derive(Debug, Clone, Serialize)]
pub struct Thread {
    pub id: String,
    pub subject: String,
    pub participants: BTreeSet<String>,
    pub messages: Vec<String>,
    /// Messages since the board or an outside address last wrote in it.
    #[serde(skip)]
    unattended: usize,
}

/// What `send` did: the message's id and thread, and the roster workers to
/// wake (the board and outside addresses aren't woken).
#[derive(Debug, Clone, PartialEq)]
pub struct Sent {
    pub id: String,
    pub thread: String,
    pub wake: Vec<String>,
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

/// The post office's state, rebuilt from nothing but the journal.
pub struct PostOffice {
    roster: BTreeSet<String>,
    relays: BTreeSet<String>,
    pub messages: HashMap<String, Meta>,
    pub threads: BTreeMap<String, Thread>,
    /// Who → their unread message ids, oldest first.
    unread: HashMap<String, Vec<String>>,
    /// Who → when they last sent, for the fan-out guard.
    sent_at: HashMap<String, VecDeque<f64>>,
    /// Mail to outside addresses, for their relays (later).
    pub outbound: VecDeque<String>,
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

impl PostOffice {
    /// A post office for `roster`, taking outside addresses of `relays`'
    /// namespaces.
    pub fn new(
        roster: impl IntoIterator<Item = String>,
        relays: impl IntoIterator<Item = String>,
    ) -> PostOffice {
        PostOffice {
            roster: roster.into_iter().collect(),
            relays: relays.into_iter().collect(),
            messages: HashMap::new(),
            threads: BTreeMap::new(),
            unread: HashMap::new(),
            sent_at: HashMap::new(),
            outbound: VecDeque::new(),
            next: 1,
        }
    }

    /// Numbers new messages and threads from `next` on: past every one an
    /// earlier run of the office journaled, so none is used twice.
    pub fn number_from(&mut self, next: u64) {
        self.next = self.next.max(next);
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

    /// Sends `d` at `now`: checked, journaled, then delivered.
    pub fn send(&mut self, journal: &Journal, d: Draft, now: f64) -> Result<Sent> {
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
            let a = self.address(t)?;
            let name = match a {
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
        let thread = match &d.in_reply_to {
            Some(r) => {
                let m = self
                    .messages
                    .get(r)
                    .ok_or_else(|| anyhow!("no message {r}"))?;
                if !self.can_read(&d.from, &m.thread) {
                    bail!("no message {r}");
                }
                Some(m.thread.clone())
            }
            None => None,
        };
        if let Some(t) = thread.as_ref().and_then(|t| self.threads.get(t)) {
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
        let thread = thread.unwrap_or_else(|| format!("t-{}", self.next));
        let at = journal.append(
            "mail",
            json!({ "id": id, "thread": thread, "from": d.from, "to": to, "subject": subject,
                    "body": d.body, "in_reply_to": d.in_reply_to, "due": due }),
        );
        if !at.written() {
            bail!("the journal can't be written; the message wasn't sent");
        }
        self.next += 1;
        if worker_sender {
            self.sent_at
                .entry(d.from.clone())
                .or_default()
                .push_back(now);
        }
        // An answer settles what its sender owed in the thread.
        let owed: Vec<String> = self
            .threads
            .get(&thread)
            .map(|t| t.messages.clone())
            .unwrap_or_default();
        for mid in owed {
            if let Some(m) = self.messages.get_mut(&mid) {
                // Answered: by one it was to, to the one who asked.
                if !m.settled && m.due.is_some() && m.to.contains(&d.from) && to.contains(&m.from) {
                    m.settled = true;
                }
            }
        }
        let t = self
            .threads
            .entry(thread.clone())
            .or_insert_with(|| Thread {
                id: thread.clone(),
                subject: subject.clone(),
                participants: BTreeSet::new(),
                messages: vec![],
                unattended: 0,
            });
        t.participants.insert(d.from.clone());
        t.participants.extend(to.iter().cloned());
        t.messages.push(id.clone());
        // The post office's notices neither count nor reset it.
        t.unattended = if worker_sender {
            t.unattended + 1
        } else if d.from == OFFICE {
            t.unattended
        } else {
            0
        };
        let mut wake = Vec::new();
        for r in &to {
            if r.contains(':') {
                self.outbound.push_back(id.clone());
            } else {
                self.unread.entry(r.clone()).or_default().push(id.clone());
                if self.roster.contains(r) {
                    wake.push(r.clone());
                }
            }
        }
        self.messages.insert(
            id.clone(),
            Meta {
                id: id.clone(),
                thread: thread.clone(),
                from: d.from,
                to,
                subject,
                ts: now,
                in_reply_to: d.in_reply_to,
                due,
                follow_ups: 0,
                wait: due.map_or(0.0, |d| d - now),
                settled: false,
                at,
            },
        );
        Ok(Sent { id, thread, wake })
    }

    /// Whether `viewer` may read thread `thread`: the board reads all, and
    /// the post office writes its reminders into any.
    pub fn can_read(&self, viewer: &str, thread: &str) -> bool {
        viewer == BOARD
            || viewer == OFFICE
            || self
                .threads
                .get(thread)
                .is_some_and(|t| t.participants.contains(viewer))
    }

    /// A message's body, from the journal.
    pub fn body(&self, journal: &Journal, id: &str) -> String {
        self.messages
            .get(id)
            .and_then(|m| journal.read(m.at).ok())
            .and_then(|v| v["body"].as_str().map(str::to_string))
            .unwrap_or_default()
    }

    /// Message `id` as `viewer` reads it, body and all.
    pub fn message(&self, journal: &Journal, id: &str, viewer: &str) -> Result<Value> {
        let m = self
            .messages
            .get(id)
            .filter(|m| self.can_read(viewer, &m.thread))
            .ok_or_else(|| anyhow!("no message {id}"))?;
        let mut v = serde_json::to_value(m)?;
        v["body"] = json!(self.body(journal, id));
        Ok(v)
    }

    /// Thread `id` as `viewer` reads it: every message, bodies and all.
    pub fn thread(&self, journal: &Journal, id: &str, viewer: &str) -> Result<Value> {
        let t = self
            .threads
            .get(id)
            .filter(|_| self.can_read(viewer, id))
            .ok_or_else(|| anyhow!("no thread {id}"))?;
        let messages: Vec<Value> = t
            .messages
            .iter()
            .filter_map(|m| self.message(journal, m, viewer).ok())
            .collect();
        Ok(
            json!({ "id": t.id, "subject": t.subject, "participants": t.participants, "messages": messages }),
        )
    }

    pub fn mark_read(&mut self, who: &str, ids: &[String]) {
        if let Some(u) = self.unread.get_mut(who) {
            u.retain(|id| !ids.contains(id));
        }
    }

    pub fn unread(&self, who: &str) -> Vec<String> {
        self.unread.get(who).cloned().unwrap_or_default()
    }

    /// `who`'s mail, newest first: a line each, at most `limit`.
    pub fn inbox(&self, who: &str, unread_only: bool, limit: usize) -> Vec<Value> {
        let unread = self.unread(who);
        let mut mine: Vec<&Meta> = self
            .messages
            .values()
            .filter(|m| m.to.iter().any(|t| t == who))
            .filter(|m| !unread_only || unread.contains(&m.id))
            .collect();
        mine.sort_by(|a, b| b.ts.total_cmp(&a.ts).then(seq(&b.id).cmp(&seq(&a.id))));
        mine.into_iter()
            .take(limit)
            .map(|m| json!({ "id": m.id, "thread": m.thread, "from": m.from, "subject": m.subject, "ts": m.ts, "unread": unread.contains(&m.id), "due": m.due }))
            .collect()
    }

    /// `who`'s desk.
    pub fn desk(&self, who: &str) -> Desk {
        let mut d = Desk::default();
        let mut open: Vec<&Meta> = self
            .messages
            .values()
            .filter(|m| m.due.is_some() && !m.settled)
            .collect();
        open.sort_by_key(|m| seq(&m.id));
        for m in open {
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
        let mut late: Vec<(String, f64)> = self
            .messages
            .values()
            .filter(|m| !m.settled && m.due.is_some_and(|d| d <= now))
            .map(|m| (m.id.clone(), m.due.unwrap()))
            .collect();
        late.sort_by_key(|l| seq(&l.0));
        let mut sent = Vec::new();
        for (id, _) in late {
            let m = self.messages[&id].clone();
            let notice = if m.follow_ups < MAX_FOLLOW_UPS {
                let interval = m.wait.max(MIN_DUE_SECS);
                let to: Vec<String> = m.to.iter().filter(|t| !t.contains(':')).cloned().collect();
                if let Some(mm) = self.messages.get_mut(&id) {
                    mm.follow_ups += 1;
                    mm.due = Some(now + interval);
                }
                (!to.is_empty()).then(|| Draft {
                    from: OFFICE.into(),
                    to,
                    subject: format!("Reminder: {}", m.subject),
                    body: format!("[Automatic reminder] {} is waiting for your answer to {} (\"{}\"). Reply to it with mail_reply.", m.from, m.id, m.subject),
                    in_reply_to: Some(id.clone()),
                    due: None,
                })
            } else {
                if let Some(mm) = self.messages.get_mut(&id) {
                    mm.settled = true;
                }
                (m.from != OFFICE && !m.from.contains(':')).then(|| Draft {
                    from: OFFICE.into(),
                    to: vec![m.from.clone()],
                    subject: format!("No answer: {}", m.subject),
                    body: format!("[Automatic notice] Nobody answered {} (\"{}\") after {MAX_FOLLOW_UPS} reminders. Decide what to do without it, or ask someone else.", m.id, m.subject),
                    in_reply_to: Some(id.clone()),
                    due: None,
                })
            };
            if let Some(d) = notice {
                match self.send(journal, d, now) {
                    Ok(s) => sent.push(s),
                    Err(e) => tracing::warn!("follow-up of {id}: {e:#}"),
                }
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
}

/// A message id's number, for ordering.
fn seq(id: &str) -> u64 {
    id.trim_start_matches("m-").parse().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn po() -> (PostOffice, Journal, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("zc-mail-{}", zapper_util::random_id(8)));
        std::fs::create_dir_all(&dir).unwrap();
        let j = Journal::create(&dir.join("journal.jsonl")).unwrap();
        let po = PostOffice::new(["ada".into(), "bob".into()], ["wa".into()]);
        (po, j, dir)
    }

    fn draft(from: &str, to: &[&str], body: &str) -> Draft {
        Draft {
            from: from.into(),
            to: to.iter().map(|s| s.to_string()).collect(),
            body: body.into(),
            ..Draft::default()
        }
    }

    #[test]
    fn mail_is_journaled_threaded_and_wakes_its_workers() {
        let (mut po, j, dir) = po();
        let s = po
            .send(&j, draft("board", &["ada", "bob"], "Plan\nmore"), 100.0)
            .unwrap();
        assert_eq!(s.wake, vec!["ada", "bob"]);
        assert_eq!(po.messages[&s.id].subject, "Plan");
        assert_eq!(po.body(&j, &s.id), "Plan\nmore");
        let r = po
            .send(
                &j,
                Draft {
                    in_reply_to: Some(s.id.clone()),
                    ..draft("ada", &["board"], "ok")
                },
                101.0,
            )
            .unwrap();
        assert_eq!(r.thread, s.thread);
        assert!(r.wake.is_empty(), "the board isn't woken");
        assert_eq!(po.unread("board"), vec![r.id.clone()]);
        let t = po.thread(&j, &s.thread, "bob").unwrap();
        assert_eq!(t["messages"].as_array().unwrap().len(), 2);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn only_known_addresses_and_bounded_messages() {
        let (mut po, j, dir) = po();
        assert!(po.send(&j, draft("ada", &["eve"], "x"), 1.0).is_err());
        assert!(po.send(&j, draft("eve", &["ada"], "x"), 1.0).is_err());
        assert!(po.send(&j, draft("ada", &["ada"], "x"), 1.0).is_err());
        assert!(po.send(&j, draft("ada", &["tg:1"], "x"), 1.0).is_err());
        assert!(po.send(&j, draft("ada", &["wa:+44 7"], "x"), 1.0).is_err());
        let big = "x".repeat(MAX_BODY + 1);
        assert!(po.send(&j, draft("ada", &["bob"], &big), 1.0).is_err());
        let s = po
            .send(&j, draft("ada", &["wa:+447700900123"], "hi"), 1.0)
            .unwrap();
        assert!(s.wake.is_empty());
        assert_eq!(po.outbound.front(), Some(&s.id));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_thread_is_read_only_by_its_participants_and_the_board() {
        let (mut po, j, dir) = po();
        let s = po
            .send(&j, draft("ada", &["board"], "secret"), 1.0)
            .unwrap();
        assert!(po.message(&j, &s.id, "bob").is_err());
        assert!(po.message(&j, &s.id, "board").is_ok());
        let reply = Draft {
            in_reply_to: Some(s.id.clone()),
            ..draft("bob", &["ada"], "let me in")
        };
        assert!(
            po.send(&j, reply, 2.0).is_err(),
            "no replying into a thread you can't read"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn workers_alone_in_a_thread_are_stopped_until_the_board_writes() {
        let (mut po, j, dir) = po();
        let first = po
            .send(&j, draft("board", &["ada", "bob"], "go"), 0.0)
            .unwrap();
        let mut last = first.id;
        let mut now = 1.0;
        for i in 0..MAX_UNATTENDED {
            let from = if i % 2 == 0 { "ada" } else { "bob" };
            let to = if i % 2 == 0 { "bob" } else { "ada" };
            // Spread out, so the fan-out guard stays out of it.
            now += RATE_WINDOW / 20.0;
            last = po
                .send(
                    &j,
                    Draft {
                        in_reply_to: Some(last),
                        ..draft(from, &[to], "again")
                    },
                    now,
                )
                .unwrap()
                .id;
        }
        let more = Draft {
            in_reply_to: Some(last.clone()),
            ..draft("ada", &["bob"], "again")
        };
        let e = po.send(&j, more, now + 1.0).unwrap_err().to_string();
        assert!(e.contains("ask board"), "{e}");
        let board = Draft {
            in_reply_to: Some(last),
            ..draft("board", &["ada"], "carry on")
        };
        let b = po.send(&j, board, now + 2.0).unwrap();
        let ok = Draft {
            in_reply_to: Some(b.id),
            ..draft("ada", &["bob"], "again")
        };
        assert!(po.send(&j, ok, now + 3.0).is_ok());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_sender_is_held_to_a_rate() {
        let (mut po, j, dir) = po();
        for i in 0..MAX_PER_WINDOW {
            po.send(&j, draft("ada", &["bob"], &format!("n{i}")), 10.0)
                .unwrap();
        }
        assert!(po.send(&j, draft("ada", &["bob"], "more"), 10.0).is_err());
        assert!(po
            .send(
                &j,
                draft("ada", &["bob"], "later"),
                10.0 + RATE_WINDOW + 1.0
            )
            .is_ok());
        assert!(po
            .send(
                &j,
                draft("board", &["bob"], "the board isn't limited"),
                10.0
            )
            .is_ok());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_due_answer_is_on_both_desks_until_given_and_is_followed_up() {
        let (mut po, j, dir) = po();
        let ask = Draft {
            due: Some(200.0),
            ..draft("ada", &["bob"], "numbers?")
        };
        assert!(
            po.send(
                &j,
                Draft {
                    due: Some(30.0),
                    ..ask.clone()
                },
                0.0
            )
            .is_err(),
            "too soon"
        );
        let s = po.send(&j, ask, 0.0).unwrap();
        assert_eq!(po.desk("ada").waiting.len(), 1);
        assert_eq!(po.desk("bob").owed.len(), 1);
        assert!(po.tick(&j, 100.0).is_empty(), "not due yet");
        let f = po.tick(&j, 200.0);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].wake, vec!["bob"]);
        assert_eq!(f[0].thread, s.thread);
        assert_eq!(
            po.tick(&j, 300.0).len(),
            0,
            "the next reminder is a due interval on"
        );
        assert_eq!(po.tick(&j, 400.0).len(), 1);
        let gave_up = po.tick(&j, 600.0);
        assert_eq!(
            gave_up[0].wake,
            vec!["ada"],
            "the sender hears nobody answered"
        );
        assert!(po.desk("bob").owed.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_answer_settles_what_was_owed() {
        let (mut po, j, dir) = po();
        let s = po
            .send(
                &j,
                Draft {
                    due: Some(500.0),
                    ..draft("ada", &["bob"], "numbers?")
                },
                0.0,
            )
            .unwrap();
        po.send(
            &j,
            Draft {
                in_reply_to: Some(s.id),
                ..draft("bob", &["ada"], "42")
            },
            1.0,
        )
        .unwrap();
        assert!(po.desk("ada").waiting.is_empty());
        assert!(po.tick(&j, 1000.0).is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_inbox_is_newest_first_and_marks_unread() {
        let (mut po, j, dir) = po();
        let a = po.send(&j, draft("board", &["ada"], "one"), 1.0).unwrap();
        let b = po.send(&j, draft("bob", &["ada"], "two"), 2.0).unwrap();
        let inbox = po.inbox("ada", false, 10);
        assert_eq!(inbox[0]["id"], json!(b.id));
        po.mark_read("ada", std::slice::from_ref(&b.id));
        let unread = po.inbox("ada", true, 10);
        assert_eq!(unread.len(), 1);
        assert_eq!(unread[0]["id"], json!(a.id));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn mail_the_journal_cant_keep_isnt_sent() {
        let (mut po, j, dir) = po();
        j.fail_writes();
        let e = po
            .send(&j, draft("ada", &["bob"], "lost?"), 1.0)
            .unwrap_err();
        assert!(e.to_string().contains("journal"), "{e}");
        assert!(po.unread("bob").is_empty());
        assert!(po.messages.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }
}
