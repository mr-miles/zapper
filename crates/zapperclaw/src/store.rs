//! Where the post office's state is kept: the projection of its events
//! (`events`). Two stores give the same answers: `Memory` (rebuilt from
//! the journal at start; the tests') and `Sqlite` (`office.db`, which
//! survives a restart and catches up from the journal).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

use crate::events::Event;

/// A message, without its body.
#[derive(Debug, Clone, PartialEq, Serialize)]
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
    pub wait: f64,
    /// Its answer is no longer awaited (answered, or given up on).
    #[serde(skip)]
    pub settled: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Thread {
    pub id: String,
    pub subject: String,
    pub participants: BTreeSet<String>,
    pub messages: Vec<String>,
    /// Messages among workers alone since the board or an outside address
    /// last wrote in it.
    #[serde(skip)]
    pub unattended: usize,
    /// When its last message was sent.
    pub last: f64,
}

/// A message or thread id's number (`m-3`, `t-3`).
pub fn number(id: &str) -> u64 {
    id.split_once('-')
        .and_then(|(_, n)| n.parse().ok())
        .unwrap_or(0)
}

/// The post office's state, as its events made it.
pub trait Store: Send {
    /// The journal `seq` of the last batch applied (0: none).
    fn last_seq(&self) -> u64;
    /// Applies a batch, as one change, recording `seq`.
    fn apply(&mut self, seq: u64, events: &[Event]) -> Result<()>;

    fn message(&self, id: &str) -> Option<Meta>;
    fn body(&self, id: &str) -> Option<String>;
    fn thread(&self, id: &str) -> Option<Thread>;
    /// Every thread, most recent first.
    fn threads(&self) -> Vec<Thread>;
    /// Mail to `who`, newest first: at most `limit`, unread only if asked.
    fn inbox(&self, who: &str, unread_only: bool, limit: usize) -> Vec<Meta>;
    /// `who`'s unread mail, oldest first.
    fn unread(&self, who: &str) -> Vec<String>;
    /// Messages awaiting an answer, oldest first.
    fn open(&self) -> Vec<Meta>;
    /// One past the highest message or thread number.
    fn next_number(&self) -> u64;
    fn conversation(&self, worker: &str) -> Option<String>;
    /// `worker`'s turns that never ended, oldest first, with the mail
    /// each showed.
    fn open_turns(&self, worker: &str) -> Vec<(u64, Vec<String>)>;
    /// The number of `worker`'s next turn.
    fn next_turn(&self, worker: &str) -> u64;
    /// Messages to outside addresses (relays'), oldest first.
    fn outbound(&self) -> Vec<String>;
}

/// The thread a message joins, with what its event says.
struct ThreadUpdate<'a> {
    thread: &'a str,
    subject: Option<&'a str>,
    participants: Vec<&'a str>,
    unattended: usize,
    ts: f64,
}

// --- in memory ---------------------------------------------------------

#[derive(Default)]
pub struct Memory {
    seq: u64,
    messages: HashMap<String, (Meta, String)>,
    threads: BTreeMap<String, Thread>,
    /// Who → their unread ids, oldest first.
    unread: HashMap<String, Vec<String>>,
    outbound: Vec<String>,
    conversations: HashMap<String, String>,
    /// (worker, turn) → (ended, shown ids).
    turns: BTreeMap<(String, u64), (bool, Vec<String>)>,
}

impl Memory {
    pub fn new() -> Memory {
        Memory::default()
    }

    fn one(&mut self, e: &Event) {
        match e {
            Event::MessageSent {
                id,
                thread,
                from,
                to,
                subject,
                body,
                ts,
                in_reply_to,
                due,
                new_thread,
                unattended,
                settles,
            } => {
                for s in settles {
                    if let Some((m, _)) = self.messages.get_mut(s) {
                        m.settled = true;
                    }
                }
                let t = self
                    .threads
                    .entry(thread.clone())
                    .or_insert_with(|| Thread {
                        id: thread.clone(),
                        subject: new_thread.clone().unwrap_or_else(|| subject.clone()),
                        participants: BTreeSet::new(),
                        messages: vec![],
                        unattended: 0,
                        last: *ts,
                    });
                t.participants.insert(from.clone());
                t.participants.extend(to.iter().cloned());
                t.messages.push(id.clone());
                t.unattended = *unattended;
                t.last = *ts;
                for r in to {
                    if r.contains(':') {
                        self.outbound.push(id.clone());
                    } else {
                        self.unread.entry(r.clone()).or_default().push(id.clone());
                    }
                }
                let meta = Meta {
                    id: id.clone(),
                    thread: thread.clone(),
                    from: from.clone(),
                    to: to.clone(),
                    subject: subject.clone(),
                    ts: *ts,
                    in_reply_to: in_reply_to.clone(),
                    due: *due,
                    follow_ups: 0,
                    wait: due.map_or(0.0, |d| d - ts),
                    settled: false,
                };
                self.messages.insert(id.clone(), (meta, body.clone()));
            }
            Event::Read { who, ids } => {
                if let Some(u) = self.unread.get_mut(who) {
                    u.retain(|i| !ids.contains(i));
                }
            }
            Event::Unread { who, ids } => {
                let u = self.unread.entry(who.clone()).or_default();
                for id in ids {
                    if !u.contains(id) && self.messages.contains_key(id) {
                        u.push(id.clone());
                    }
                }
                u.sort_by_key(|i| number(i));
            }
            Event::FollowedUp {
                id,
                follow_ups,
                due,
            } => {
                if let Some((m, _)) = self.messages.get_mut(id) {
                    m.follow_ups = *follow_ups;
                    m.due = Some(*due);
                }
            }
            Event::Settled { ids } => {
                for id in ids {
                    if let Some((m, _)) = self.messages.get_mut(id) {
                        m.settled = true;
                    }
                }
            }
            Event::TurnStarted { worker, turn } => {
                self.turns.insert((worker.clone(), *turn), (false, vec![]));
            }
            Event::Shown { worker, turn, ids } => {
                if let Some(u) = self.unread.get_mut(worker) {
                    u.retain(|i| !ids.contains(i));
                }
                let t = self
                    .turns
                    .entry((worker.clone(), *turn))
                    .or_insert((false, vec![]));
                t.1.extend(ids.iter().cloned());
            }
            Event::TurnEnded { worker, turn } => {
                if let Some(t) = self.turns.get_mut(&(worker.clone(), *turn)) {
                    t.0 = true;
                }
            }
            Event::Conversation { worker, id } => {
                self.conversations.insert(worker.clone(), id.clone());
            }
        }
    }
}

impl Store for Memory {
    fn last_seq(&self) -> u64 {
        self.seq
    }

    fn apply(&mut self, seq: u64, events: &[Event]) -> Result<()> {
        for e in events {
            self.one(e);
        }
        self.seq = seq;
        Ok(())
    }

    fn message(&self, id: &str) -> Option<Meta> {
        self.messages.get(id).map(|(m, _)| m.clone())
    }

    fn body(&self, id: &str) -> Option<String> {
        self.messages.get(id).map(|(_, b)| b.clone())
    }

    fn thread(&self, id: &str) -> Option<Thread> {
        self.threads.get(id).cloned()
    }

    fn threads(&self) -> Vec<Thread> {
        let mut all: Vec<Thread> = self.threads.values().cloned().collect();
        all.sort_by(|a, b| {
            b.last
                .total_cmp(&a.last)
                .then(number(&b.id).cmp(&number(&a.id)))
        });
        all
    }

    fn inbox(&self, who: &str, unread_only: bool, limit: usize) -> Vec<Meta> {
        let unread = self.unread(who);
        let mut mine: Vec<&Meta> = self
            .messages
            .values()
            .map(|(m, _)| m)
            .filter(|m| m.to.iter().any(|t| t == who))
            .filter(|m| !unread_only || unread.contains(&m.id))
            .collect();
        mine.sort_by_key(|m| std::cmp::Reverse(number(&m.id)));
        mine.into_iter().take(limit).cloned().collect()
    }

    fn unread(&self, who: &str) -> Vec<String> {
        self.unread.get(who).cloned().unwrap_or_default()
    }

    fn open(&self) -> Vec<Meta> {
        let mut open: Vec<Meta> = self
            .messages
            .values()
            .map(|(m, _)| m)
            .filter(|m| m.due.is_some() && !m.settled)
            .cloned()
            .collect();
        open.sort_by_key(|m| number(&m.id));
        open
    }

    fn next_number(&self) -> u64 {
        let m = self.messages.keys().map(|k| number(k)).max().unwrap_or(0);
        let t = self.threads.keys().map(|k| number(k)).max().unwrap_or(0);
        m.max(t) + 1
    }

    fn conversation(&self, worker: &str) -> Option<String> {
        self.conversations.get(worker).cloned()
    }

    fn open_turns(&self, worker: &str) -> Vec<(u64, Vec<String>)> {
        self.turns
            .iter()
            .filter(|((w, _), (ended, _))| w == worker && !ended)
            .map(|((_, t), (_, shown))| (*t, shown.clone()))
            .collect()
    }

    fn next_turn(&self, worker: &str) -> u64 {
        self.turns
            .keys()
            .filter(|(w, _)| w == worker)
            .map(|(_, t)| *t)
            .max()
            .unwrap_or(0)
            + 1
    }

    fn outbound(&self) -> Vec<String> {
        self.outbound.clone()
    }
}

// --- SQLite --------------------------------------------------------------

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta (k TEXT PRIMARY KEY, v TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS messages (
  n INTEGER PRIMARY KEY, id TEXT NOT NULL UNIQUE, thread TEXT NOT NULL, sender TEXT NOT NULL,
  subject TEXT NOT NULL, body TEXT NOT NULL, ts REAL NOT NULL, in_reply_to TEXT, due REAL,
  follow_ups INTEGER NOT NULL DEFAULT 0, wait REAL NOT NULL DEFAULT 0, settled INTEGER NOT NULL DEFAULT 0);
CREATE INDEX IF NOT EXISTS messages_thread ON messages (thread, n);
CREATE INDEX IF NOT EXISTS messages_open ON messages (settled, due);
CREATE TABLE IF NOT EXISTS recipients (
  message TEXT NOT NULL, who TEXT NOT NULL, pos INTEGER NOT NULL, unread INTEGER NOT NULL,
  outside INTEGER NOT NULL, PRIMARY KEY (message, who));
CREATE INDEX IF NOT EXISTS recipients_who ON recipients (who, unread);
CREATE TABLE IF NOT EXISTS threads (
  id TEXT PRIMARY KEY, subject TEXT NOT NULL, unattended INTEGER NOT NULL, last REAL NOT NULL);
CREATE TABLE IF NOT EXISTS participants (thread TEXT NOT NULL, who TEXT NOT NULL, PRIMARY KEY (thread, who));
CREATE TABLE IF NOT EXISTS turns (worker TEXT NOT NULL, turn INTEGER NOT NULL, ended INTEGER NOT NULL, PRIMARY KEY (worker, turn));
CREATE TABLE IF NOT EXISTS shown (worker TEXT NOT NULL, turn INTEGER NOT NULL, message TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS conversations (worker TEXT PRIMARY KEY, id TEXT NOT NULL);
";

pub struct Sqlite {
    db: Connection,
}

impl Sqlite {
    /// The store at `path`, made (0600) if it isn't there.
    pub fn open(path: &Path) -> Result<Sqlite> {
        let fresh = !path.exists();
        let db = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        if fresh {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.pragma_update(None, "synchronous", "NORMAL")?;
        db.execute_batch(SCHEMA)?;
        Ok(Sqlite { db })
    }

    /// An in-memory database, for the tests.
    #[cfg(test)]
    pub fn in_memory() -> Sqlite {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(SCHEMA).unwrap();
        Sqlite { db }
    }

    fn meta_of(&self, row: &rusqlite::Row) -> rusqlite::Result<Meta> {
        let id: String = row.get(0)?;
        let mut to = self
            .db
            .prepare_cached("SELECT who FROM recipients WHERE message = ?1 ORDER BY pos")?;
        let to: Vec<String> = to
            .query_map([&id], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(Meta {
            id,
            thread: row.get(1)?,
            from: row.get(2)?,
            to,
            subject: row.get(3)?,
            ts: row.get(4)?,
            in_reply_to: row.get(5)?,
            due: row.get(6)?,
            follow_ups: row.get(7)?,
            wait: row.get(8)?,
            settled: row.get::<_, i64>(9)? != 0,
        })
    }

    fn metas(&self, sql: &str, args: impl rusqlite::Params) -> Vec<Meta> {
        let run = || -> rusqlite::Result<Vec<Meta>> {
            let mut q = self.db.prepare_cached(sql)?;
            let rows = q.query_map(args, |r| self.meta_of(r))?;
            rows.collect()
        };
        run().unwrap_or_else(|e| {
            tracing::error!("the post office's database: {e}");
            vec![]
        })
    }

    fn strings(&self, sql: &str, args: impl rusqlite::Params) -> Vec<String> {
        let run = || -> rusqlite::Result<Vec<String>> {
            let mut q = self.db.prepare_cached(sql)?;
            let rows = q.query_map(args, |r| r.get(0))?;
            rows.collect()
        };
        run().unwrap_or_else(|e| {
            tracing::error!("the post office's database: {e}");
            vec![]
        })
    }
}

const META_COLUMNS: &str =
    "id, thread, sender, subject, ts, in_reply_to, due, follow_ups, wait, settled";

fn apply_one(tx: &rusqlite::Transaction, e: &Event) -> rusqlite::Result<()> {
    match e {
        Event::MessageSent {
            id,
            thread,
            from,
            to,
            subject,
            body,
            ts,
            in_reply_to,
            due,
            new_thread,
            unattended,
            settles,
        } => {
            for s in settles {
                tx.execute("UPDATE messages SET settled = 1 WHERE id = ?1", [s])?;
            }
            let t = ThreadUpdate {
                thread,
                subject: new_thread.as_deref(),
                participants: std::iter::once(from.as_str())
                    .chain(to.iter().map(String::as_str))
                    .collect(),
                unattended: *unattended,
                ts: *ts,
            };
            tx.execute(
                "INSERT INTO threads (id, subject, unattended, last) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (id) DO UPDATE SET unattended = ?3, last = ?4",
                params![
                    t.thread,
                    t.subject.unwrap_or(subject),
                    t.unattended as i64,
                    t.ts
                ],
            )?;
            for p in &t.participants {
                tx.execute(
                    "INSERT OR IGNORE INTO participants (thread, who) VALUES (?1, ?2)",
                    params![t.thread, p],
                )?;
            }
            tx.execute(
                "INSERT INTO messages (n, id, thread, sender, subject, body, ts, in_reply_to, due, wait)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    number(id) as i64,
                    id,
                    thread,
                    from,
                    subject,
                    body,
                    ts,
                    in_reply_to,
                    due,
                    due.map_or(0.0, |d| d - ts)
                ],
            )?;
            for (pos, r) in to.iter().enumerate() {
                let outside = r.contains(':');
                tx.execute(
                    "INSERT OR IGNORE INTO recipients (message, who, pos, unread, outside) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![id, r, pos as i64, !outside, outside],
                )?;
            }
        }
        Event::Read { who, ids } => {
            for id in ids {
                tx.execute(
                    "UPDATE recipients SET unread = 0 WHERE message = ?1 AND who = ?2",
                    [id, who],
                )?;
            }
        }
        Event::Unread { who, ids } => {
            for id in ids {
                tx.execute(
                    "UPDATE recipients SET unread = 1 WHERE message = ?1 AND who = ?2",
                    [id, who],
                )?;
            }
        }
        Event::FollowedUp {
            id,
            follow_ups,
            due,
        } => {
            tx.execute(
                "UPDATE messages SET follow_ups = ?2, due = ?3 WHERE id = ?1",
                params![id, follow_ups, due],
            )?;
        }
        Event::Settled { ids } => {
            for id in ids {
                tx.execute("UPDATE messages SET settled = 1 WHERE id = ?1", [id])?;
            }
        }
        Event::TurnStarted { worker, turn } => {
            tx.execute(
                "INSERT OR REPLACE INTO turns (worker, turn, ended) VALUES (?1, ?2, 0)",
                params![worker, *turn as i64],
            )?;
        }
        Event::Shown { worker, turn, ids } => {
            for id in ids {
                tx.execute(
                    "UPDATE recipients SET unread = 0 WHERE message = ?1 AND who = ?2",
                    [id, worker],
                )?;
                tx.execute(
                    "INSERT INTO shown (worker, turn, message) VALUES (?1, ?2, ?3)",
                    params![worker, *turn as i64, id],
                )?;
            }
        }
        Event::TurnEnded { worker, turn } => {
            tx.execute(
                "UPDATE turns SET ended = 1 WHERE worker = ?1 AND turn = ?2",
                params![worker, *turn as i64],
            )?;
        }
        Event::Conversation { worker, id } => {
            tx.execute(
                "INSERT OR REPLACE INTO conversations (worker, id) VALUES (?1, ?2)",
                [worker, id],
            )?;
        }
    }
    Ok(())
}

impl Store for Sqlite {
    fn last_seq(&self) -> u64 {
        self.db
            .query_row("SELECT v FROM meta WHERE k = 'seq'", [], |r| {
                r.get::<_, String>(0)
            })
            .optional()
            .ok()
            .flatten()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    }

    fn apply(&mut self, seq: u64, events: &[Event]) -> Result<()> {
        let tx = self.db.transaction()?;
        for e in events {
            apply_one(&tx, e).with_context(|| format!("applying {e:?}"))?;
        }
        tx.execute(
            "INSERT OR REPLACE INTO meta (k, v) VALUES ('seq', ?1)",
            [seq.to_string()],
        )?;
        tx.commit()?;
        Ok(())
    }

    fn message(&self, id: &str) -> Option<Meta> {
        self.metas(
            &format!("SELECT {META_COLUMNS} FROM messages WHERE id = ?1"),
            [id],
        )
        .pop()
    }

    fn body(&self, id: &str) -> Option<String> {
        self.db
            .query_row("SELECT body FROM messages WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .optional()
            .ok()
            .flatten()
    }

    fn thread(&self, id: &str) -> Option<Thread> {
        let (subject, unattended, last): (String, i64, f64) = self
            .db
            .query_row(
                "SELECT subject, unattended, last FROM threads WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .ok()
            .flatten()?;
        Some(Thread {
            id: id.to_string(),
            subject,
            participants: self
                .strings("SELECT who FROM participants WHERE thread = ?1", [id])
                .into_iter()
                .collect(),
            messages: self.strings("SELECT id FROM messages WHERE thread = ?1 ORDER BY n", [id]),
            unattended: unattended as usize,
            last,
        })
    }

    fn threads(&self) -> Vec<Thread> {
        self.strings(
            "SELECT id FROM threads ORDER BY last DESC, CAST(substr(id, 3) AS INTEGER) DESC",
            [],
        )
        .iter()
        .filter_map(|id| self.thread(id))
        .collect()
    }

    fn inbox(&self, who: &str, unread_only: bool, limit: usize) -> Vec<Meta> {
        self.metas(
            &format!(
                "SELECT {META_COLUMNS} FROM messages WHERE id IN
                   (SELECT message FROM recipients WHERE who = ?1 AND (?2 = 0 OR unread = 1))
                 ORDER BY n DESC LIMIT ?3"
            ),
            params![who, unread_only, limit as i64],
        )
    }

    fn unread(&self, who: &str) -> Vec<String> {
        self.strings(
            "SELECT r.message FROM recipients r JOIN messages m ON m.id = r.message
             WHERE r.who = ?1 AND r.unread = 1 ORDER BY m.n",
            [who],
        )
    }

    fn open(&self) -> Vec<Meta> {
        self.metas(
            &format!("SELECT {META_COLUMNS} FROM messages WHERE due IS NOT NULL AND settled = 0 ORDER BY n"),
            [],
        )
    }

    fn next_number(&self) -> u64 {
        let m: i64 = self
            .db
            .query_row("SELECT COALESCE(MAX(n), 0) FROM messages", [], |r| r.get(0))
            .unwrap_or(0);
        let t: i64 = self
            .db
            .query_row(
                "SELECT COALESCE(MAX(CAST(substr(id, 3) AS INTEGER)), 0) FROM threads",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        m.max(t) as u64 + 1
    }

    fn conversation(&self, worker: &str) -> Option<String> {
        self.db
            .query_row(
                "SELECT id FROM conversations WHERE worker = ?1",
                [worker],
                |r| r.get(0),
            )
            .optional()
            .ok()
            .flatten()
    }

    fn open_turns(&self, worker: &str) -> Vec<(u64, Vec<String>)> {
        let turns: Vec<i64> = {
            let run = || -> rusqlite::Result<Vec<i64>> {
                let mut q = self.db.prepare_cached(
                    "SELECT turn FROM turns WHERE worker = ?1 AND ended = 0 ORDER BY turn",
                )?;
                let rows = q.query_map([worker], |r| r.get(0))?;
                rows.collect()
            };
            run().unwrap_or_default()
        };
        turns
            .into_iter()
            .map(|t| {
                let ids = self.strings(
                    "SELECT message FROM shown WHERE worker = ?1 AND turn = ?2 ORDER BY rowid",
                    params![worker, t],
                );
                (t as u64, ids)
            })
            .collect()
    }

    fn next_turn(&self, worker: &str) -> u64 {
        self.db
            .query_row(
                "SELECT COALESCE(MAX(turn), 0) FROM turns WHERE worker = ?1",
                [worker],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0) as u64
            + 1
    }

    fn outbound(&self) -> Vec<String> {
        self.strings(
            "SELECT r.message FROM recipients r JOIN messages m ON m.id = r.message
             WHERE r.outside = 1 ORDER BY m.n",
            [],
        )
    }
}
