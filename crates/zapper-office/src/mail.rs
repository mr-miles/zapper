//! The post office: mail between workers and the board, threads, who owes
//! whom an answer, and automatic follow-ups.
//!
//! Bodies live in the run's journal; this keeps where they are and what the
//! desks and follow-ups need.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::{Arc, Mutex, RwLock};

use crate::firm::{Firm, BOARD};
use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::journal::{At, Journal};

/// The post office's own address, for notices.
pub const OFFICE: &str = "office";
pub const MAX_BODY: usize = 64 * 1024;
const MAX_SUBJECT: usize = 200;
const MAX_RECIPIENTS: usize = 64;
const MAX_ATTACHMENTS: usize = 16;
/// Shortest follow-up interval a message may ask for.
const MIN_FOLLOW_UP_SECS: f64 = 5.0;
const DEFAULT_FOLLOW_UPS: u32 = 2;
/// When the board gives no date, the time it allows for an answer before the
/// post office follows up for it.
const BOARD_ANSWER_SECS: f64 = 1200.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    Assignment,
    Question,
    Reply,
    Report,
    Fyi,
    /// Sent by the post office only.
    FollowUp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Expects {
    Reply,
    Done,
    Nothing,
}

impl Kind {
    fn default_expects(self) -> Expects {
        match self {
            Kind::Assignment => Expects::Done,
            Kind::Question => Expects::Reply,
            _ => Expects::Nothing,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct FollowUp {
    /// Seconds between follow-ups; default: the time from sending to `due`.
    #[serde(default)]
    pub every: Option<f64>,
    #[serde(default)]
    pub max: Option<u32>,
    /// No follow-ups at all.
    #[serde(default)]
    pub off: bool,
}

/// A message to send.
#[derive(Debug, Clone)]
pub struct Draft {
    pub from: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub subject: String,
    pub body: String,
    pub kind: Kind,
    pub expects: Option<Expects>,
    pub due: Option<f64>,
    pub attachments: Vec<String>,
    pub in_reply_to: Option<String>,
    /// Closes the assignment being answered.
    pub close: bool,
    pub follow_up: FollowUp,
    /// Written by the post office, not by the sender.
    pub automatic: bool,
    /// The thread the sender was working on when it started this one, for
    /// the delegation tree.
    pub context: Option<String>,
}

impl Draft {
    pub fn new(from: &str, to: Vec<String>, subject: &str, body: &str, kind: Kind) -> Draft {
        Draft {
            from: from.into(),
            to,
            cc: vec![],
            subject: subject.into(),
            body: body.into(),
            kind,
            expects: None,
            due: None,
            attachments: vec![],
            in_reply_to: None,
            close: false,
            follow_up: FollowUp::default(),
            automatic: false,
            context: None,
        }
    }
}

/// What the post office keeps about a message; the body is in the journal.
#[derive(Debug, Clone, Serialize)]
pub struct Meta {
    pub id: String,
    pub thread: String,
    pub from: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub subject: String,
    pub kind: Kind,
    pub expects: Expects,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub due: Option<f64>,
    pub created: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_reply_to: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub automatic: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub close: bool,
    /// First line of the body, for lists.
    pub preview: String,
    #[serde(skip)]
    pub at: At,
}

#[derive(Debug, Clone, Serialize)]
pub struct Thread {
    pub id: String,
    pub subject: String,
    pub participants: BTreeSet<String>,
    pub messages: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    pub paused: bool,
    pub created: f64,
    pub updated: f64,
}

/// Someone owes someone an answer to a message.
#[derive(Debug, Clone, Serialize)]
pub struct Obligation {
    pub message: String,
    pub thread: String,
    pub subject: String,
    /// Who owes the answer.
    pub debtor: String,
    /// Who waits for it.
    pub creditor: String,
    pub expects: Expects,
    pub kind: Kind,
    pub created: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub due: Option<f64>,
    pub follow_ups: u32,
    pub max_follow_ups: u32,
    #[serde(skip)]
    every: f64,
    #[serde(skip)]
    next: Option<f64>,
    pub escalated: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Sent {
    pub id: String,
    pub thread: String,
    pub recipients: Vec<String>,
    /// This message answered the board's first assignment: the objective is
    /// done.
    pub objective_done: bool,
}

#[derive(Debug, Default, Serialize)]
pub struct Desk {
    /// Assignments and questions waiting for this worker.
    pub owed: Vec<Obligation>,
    /// Answers this worker waits for.
    pub waiting: Vec<Obligation>,
}

pub struct PostOffice {
    /// The run's live chart, shared with the run. Read through `chart()`, a
    /// copy, so its lock is never held across anything else.
    firm: Arc<RwLock<Firm>>,
    /// Workers who left, and who gets their mail: their manager.
    departed: BTreeMap<String, String>,
    /// The board's first assignment of the run: the objective.
    kickoff: Option<String>,
    max_thread: usize,
    mail_per_hour: usize,
    fanout_per_hour: usize,
    pair_per_10min: usize,
    mail_per_run: usize,
    pub messages: BTreeMap<String, Meta>,
    pub threads: BTreeMap<String, Thread>,
    /// Open obligations by message id and debtor.
    obligations: BTreeMap<(String, String), Obligation>,
    unread: HashMap<String, BTreeSet<String>>,
    sent_log: HashMap<String, VecDeque<f64>>,
    fanout_log: HashMap<String, VecDeque<f64>>,
    /// Messages between two workers, for the loop guard; and when the board
    /// was last told about each pair.
    pair_log: HashMap<(String, String), VecDeque<f64>>,
    pair_told: HashMap<(String, String), f64>,
    counter: u64,
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    s.chars().take(n).collect::<String>() + "…"
}

/// A subject from a body: its first line, without Markdown marks, cut short.
pub fn subject_from(body: &str) -> String {
    let line = body
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let line = line
        .trim_start_matches(['#', '>', '-', '*', ' '])
        .replace("**", "")
        .replace('`', "");
    let s = clip(line.trim(), 80);
    if s.is_empty() {
        "(no subject)".into()
    } else {
        s
    }
}

fn preview(body: &str) -> String {
    clip(
        body.lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or(""),
        120,
    )
}

/// The run's post office behind its lock. Mail changes only through
/// these methods; other modules read it inside `with`, which holds the lock
/// for the closure alone, so no caller keeps it while taking another.
pub(crate) struct Post(Mutex<PostOffice>);

impl Post {
    pub(crate) fn new(po: PostOffice) -> Post {
        Post(Mutex::new(po))
    }

    /// Reads the post office.
    pub(crate) fn with<R>(&self, f: impl FnOnce(&PostOffice) -> R) -> R {
        f(&self.0.lock().unwrap())
    }

    pub(crate) fn send(&self, journal: &Journal, d: Draft, now: f64) -> Result<Sent> {
        self.0.lock().unwrap().send(journal, d, now)
    }

    pub(crate) fn send_notice(&self, journal: &Journal, d: Draft, now: f64) -> Result<Sent> {
        self.0.lock().unwrap().send_notice(journal, d, now)
    }

    pub(crate) fn mark_read(&self, who: &str, ids: &[String]) {
        self.0.lock().unwrap().mark_read(who, ids)
    }

    /// A message for `viewer`, now read by them.
    pub(crate) fn read_message(&self, journal: &Journal, id: &str, viewer: &str) -> Result<Value> {
        let mut po = self.0.lock().unwrap();
        let v = po.message(journal, id, viewer)?;
        po.mark_read(viewer, &[id.to_string()]);
        Ok(v)
    }

    /// A thread for `viewer`, its messages now read by them.
    pub(crate) fn read_thread(&self, journal: &Journal, id: &str, viewer: &str) -> Result<Value> {
        let mut po = self.0.lock().unwrap();
        let v = po.thread(journal, id, viewer)?;
        let ids: Vec<String> = v["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| m["id"].as_str().map(str::to_string))
            .collect();
        po.mark_read(viewer, &ids);
        Ok(v)
    }

    pub(crate) fn tick(&self, journal: &Journal, now: f64) -> Vec<Sent> {
        self.0.lock().unwrap().tick(journal, now)
    }

    pub(crate) fn leave(&self, worker: &str, to: &str) -> (Vec<Obligation>, Vec<String>) {
        self.0.lock().unwrap().leave(worker, to)
    }

    pub(crate) fn settle_thread(&self, journal: &Journal, thread: &str, who: &str, by: &str) {
        self.0
            .lock()
            .unwrap()
            .settle_thread(journal, thread, who, by)
    }
}

impl PostOffice {
    pub fn new(firm: &Firm, chart: Arc<RwLock<Firm>>) -> PostOffice {
        PostOffice {
            firm: chart,
            departed: BTreeMap::new(),
            kickoff: None,
            max_thread: firm.limits.thread_messages as usize,
            mail_per_hour: firm.limits.mail_per_hour as usize,
            fanout_per_hour: firm.limits.fanout_per_hour as usize,
            pair_per_10min: firm.limits.pair_per_10min as usize,
            mail_per_run: firm.limits.mail_per_run as usize,
            messages: BTreeMap::new(),
            threads: BTreeMap::new(),
            obligations: BTreeMap::new(),
            unread: HashMap::new(),
            sent_log: HashMap::new(),
            fanout_log: HashMap::new(),
            pair_log: HashMap::new(),
            pair_told: HashMap::new(),
            counter: 0,
        }
    }

    fn next_id(&mut self, prefix: &str) -> String {
        self.counter += 1;
        format!("{prefix}-{}", self.counter)
    }

    fn chart(&self) -> Firm {
        self.firm.read().unwrap().clone()
    }

    /// Where mail to `a` goes: `a`, or for someone who left, the manager
    /// who took over their work (following further departures).
    fn forward(&self, a: &str) -> String {
        let mut cur = a.to_string();
        for _ in 0..64 {
            match self.departed.get(&cur) {
                Some(m) => cur = m.clone(),
                None => break,
            }
        }
        cur
    }

    /// `worker` left; `to`, their manager, takes over what they owed and
    /// what they waited for, and gets their mail from now on. Returns what
    /// they owed, and the messages `to` has to read now: the ones the worker
    /// hadn't read and the ones behind what they owed (not `to`'s own).
    pub fn leave(&mut self, worker: &str, to: &str) -> (Vec<Obligation>, Vec<String>) {
        self.departed.insert(worker.to_string(), to.to_string());
        let mut moved: Vec<Obligation> = Vec::new();
        let keys: Vec<(String, String)> = self.obligations.keys().cloned().collect();
        for k in keys {
            let Some(mut o) = self.obligations.remove(&k) else {
                continue;
            };
            if o.debtor == worker {
                o.debtor = to.to_string();
                // What the manager asked of them is nobody's debt now: the
                // manager can't owe themselves, or wait for themselves.
                if o.creditor == o.debtor {
                    continue;
                }
                moved.push(o.clone());
                self.obligations
                    .insert((o.message.clone(), o.debtor.clone()), o);
            } else {
                if o.creditor == worker {
                    o.creditor = to.to_string();
                }
                if o.creditor == o.debtor {
                    continue;
                }
                self.obligations.insert(k, o);
            }
        }
        // The mail they hadn't read, and the requests they hadn't answered,
        // go to their manager to read.
        let mut ids: Vec<String> = self
            .unread
            .remove(worker)
            .map(|u| u.into_iter().collect())
            .unwrap_or_default();
        ids.extend(moved.iter().map(|o| o.message.clone()));
        ids.sort_by_key(|id| {
            id.trim_start_matches("m-")
                .parse::<u64>()
                .unwrap_or(u64::MAX)
        });
        ids.dedup();
        ids.retain(|id| self.messages.get(id).is_some_and(|m| m.from != to));
        let u = self.unread.entry(to.to_string()).or_default();
        for id in &ids {
            u.insert(id.clone());
        }
        (moved, ids)
    }

    fn is_address(&self, a: &str) -> bool {
        a == BOARD || self.chart().worker(&self.forward(a)).is_some()
    }

    /// Expands `team:NAME` (NAME's direct reports) and `all` (everyone but
    /// the sender); checks the rest are workers or the board.
    fn expand(&self, from: &str, list: &[String]) -> Result<(Vec<String>, bool)> {
        let mut out = Vec::new();
        let mut fanout = false;
        let chart = self.chart();
        for a in list {
            let a = a.trim().to_ascii_lowercase();
            if a == "all" {
                fanout = true;
                out.extend(
                    chart
                        .workers
                        .iter()
                        .map(|w| w.name.clone())
                        .filter(|n| n != from),
                );
            } else if let Some(team) = a.strip_prefix("team:") {
                let team = self.forward(team);
                if chart.worker(&team).is_none() {
                    bail!("team:{team}: no worker {team}");
                }
                fanout = true;
                out.extend(
                    chart
                        .reports_of(&team)
                        .into_iter()
                        .map(str::to_string)
                        .filter(|n| n != from),
                );
            } else if self.is_address(&a) {
                let a = self.forward(&a);
                if a != from {
                    out.push(a);
                }
            } else {
                bail!("{a:?} is not an address: use a worker's name, board, team:NAME or all (see directory)");
            }
        }
        let mut seen = BTreeSet::new();
        out.retain(|a| seen.insert(a.clone()));
        Ok((out, fanout))
    }

    fn rate_ok(
        log: &mut HashMap<String, VecDeque<f64>>,
        who: &str,
        limit: usize,
        now: f64,
    ) -> bool {
        let q = log.entry(who.to_string()).or_default();
        while q.front().is_some_and(|t| now - t > 3600.0) {
            q.pop_front();
        }
        if q.len() >= limit {
            return false;
        }
        q.push_back(now);
        true
    }

    /// Sends a message; checks addresses, authority and limits.
    pub fn send(&mut self, journal: &Journal, d: Draft, now: f64) -> Result<Sent> {
        let system = d.automatic;
        let mut d = d;
        // A subject is optional: a new thread without one is named by its
        // first line.
        if d.subject.trim().is_empty() && d.in_reply_to.is_none() {
            d.subject = subject_from(&d.body);
        }
        if d.body.len() > MAX_BODY {
            bail!("body is {} bytes; at most {MAX_BODY}", d.body.len());
        }
        if d.subject.len() > MAX_SUBJECT {
            bail!("subject is longer than {MAX_SUBJECT} bytes");
        }
        if d.attachments.len() > MAX_ATTACHMENTS {
            bail!("at most {MAX_ATTACHMENTS} attachments");
        }
        if d.kind == Kind::FollowUp && !system {
            bail!("follow-ups are sent by the post office");
        }
        if self.messages.len() >= self.mail_per_run {
            bail!("the run's mail limit ({}) is reached", self.mail_per_run);
        }
        let (to, fan_to) = self.expand(&d.from, &d.to)?;
        let (mut cc, fan_cc) = self.expand(&d.from, &d.cc)?;
        cc.retain(|c| !to.contains(c));
        if to.is_empty() {
            bail!("no recipients");
        }
        if to.len() + cc.len() > MAX_RECIPIENTS {
            bail!("at most {MAX_RECIPIENTS} recipients");
        }
        // Authority flows down the chart.
        let chart = self.chart();
        if d.kind == Kind::Assignment && d.from != BOARD && !system {
            for r in &to {
                if !chart.is_above(&d.from, r) {
                    bail!(
                        "{} can't give {r} an assignment: only the board and {r}'s managers can. Send a question instead, or ask {}",
                        d.from,
                        chart.manager_of(r).unwrap_or(BOARD)
                    );
                }
            }
        }
        let parent = match &d.in_reply_to {
            Some(id) => {
                let m = self
                    .messages
                    .get(id)
                    .ok_or_else(|| anyhow!("no message {id}"))?;
                if !system
                    && d.from != BOARD
                    && m.from != d.from
                    && !m.to.contains(&d.from)
                    && !m.cc.contains(&d.from)
                {
                    bail!("message {id} isn't addressed to you");
                }
                Some(m.clone())
            }
            None => None,
        };
        let thread_id = parent.as_ref().map(|p| p.thread.clone());
        if let Some(t) = thread_id.as_ref().and_then(|t| self.threads.get(t)) {
            if t.messages.len() >= self.max_thread && !system && d.from != BOARD {
                bail!("thread {} has {} messages and is paused; start a new thread, or tell your manager", t.id, t.messages.len());
            }
        }
        // Closing the board's objective pauses the firm: not while work it
        // asked for is still open anywhere in the firm.
        if d.close && !system {
            if let Some(t) = &thread_id {
                if self.closes_objective(t, &d.from) {
                    let open: Vec<String> = self
                        .obligations
                        .values()
                        .filter(|o| {
                            o.expects == Expects::Done
                                && Some(o.message.as_str()) != self.kickoff.as_deref()
                        })
                        .map(|o| format!("\"{}\" ({} owes {})", o.subject, o.debtor, o.creditor))
                        .take(8)
                        .collect();
                    if !open.is_empty() {
                        bail!("the board's objective can't close while assignments are open: {}. The firm pauses when it closes, so that work would never be done. Send the board a progress report without close, or finish the open work (or take it back) first", open.join("; "));
                    }
                }
            }
        }
        if !system && d.from != BOARD {
            let fanout = fan_to || fan_cc;
            if fanout && !Self::rate_ok(&mut self.fanout_log, &d.from, self.fanout_per_hour, now) {
                bail!(
                    "you sent {} messages to all or team: in the last hour; that's the limit",
                    self.fanout_per_hour
                );
            }
            if !Self::rate_ok(&mut self.sent_log, &d.from, self.mail_per_hour, now) {
                bail!(
                    "you sent {} messages in the last hour; that's the limit",
                    self.mail_per_hour
                );
            }
            // Two workers bouncing mail back and forth: a loop.
            for r in to
                .iter()
                .chain(cc.iter())
                .filter(|r| chart.worker(r).is_some())
            {
                let key = if d.from < *r {
                    (d.from.clone(), r.clone())
                } else {
                    (r.clone(), d.from.clone())
                };
                let log = self.pair_log.entry(key.clone()).or_default();
                while log.front().is_some_and(|t| now - t > 600.0) {
                    log.pop_front();
                }
                if self.pair_per_10min > 0 && log.len() >= self.pair_per_10min {
                    let n = log.len();
                    if self.pair_told.get(&key).is_none_or(|t| now - t > 3600.0) {
                        self.pair_told.insert(key.clone(), now);
                        journal.append(
                            "loop_guard",
                            json!({ "workers": [key.0, key.1], "messages": n }),
                        );
                        let body = format!(
                            "[Automatic notice] {} and {} exchanged {n} messages in ten minutes, which looks like a loop. The post office refuses more mail between them for now; it resumes as the ten-minute window moves. Check their threads, and pause the firm if they are stuck.\n",
                            key.0, key.1
                        );
                        let mut notice = Draft::new(
                            OFFICE,
                            vec![BOARD.into()],
                            &format!("Loop? {} and {}", key.0, key.1),
                            &body,
                            Kind::Fyi,
                        );
                        notice.automatic = true;
                        let _ = self.send_notice(journal, notice, now);
                    }
                    bail!("you and {r} exchanged {n} messages in the last ten minutes; the post office stops this as a likely loop. If you are stuck, tell your manager or the board, or end your turn");
                }
            }
        }

        let id = self.next_id("m");
        let thread = thread_id.unwrap_or_else(|| format!("t-{}", id.trim_start_matches("m-")));
        let subject = match (&parent, d.subject.trim()) {
            (Some(p), "") => self
                .threads
                .get(&p.thread)
                .map(|t| t.subject.clone())
                .unwrap_or_default(),
            (_, s) => s.to_string(),
        };
        let expects = d.expects.unwrap_or(d.kind.default_expects());
        let at = journal.append(
            "mail",
            json!({
                "id": id, "thread": thread, "from": d.from, "to": to, "cc": cc, "subject": subject,
                "kind": d.kind, "expects": expects, "due": d.due, "in_reply_to": d.in_reply_to,
                "attachments": d.attachments, "automatic": d.automatic, "close": d.close,
                "context": d.context, "body": d.body,
            }),
        );
        let meta = Meta {
            id: id.clone(),
            thread: thread.clone(),
            from: d.from.clone(),
            to: to.clone(),
            cc: cc.clone(),
            subject: subject.clone(),
            kind: d.kind,
            expects,
            due: d.due,
            created: now,
            in_reply_to: d.in_reply_to.clone(),
            attachments: d.attachments.clone(),
            automatic: d.automatic,
            close: d.close,
            preview: preview(&d.body),
            at,
        };
        let t = self
            .threads
            .entry(thread.clone())
            .or_insert_with(|| Thread {
                id: thread.clone(),
                subject: subject.clone(),
                participants: BTreeSet::new(),
                messages: vec![],
                context: d.context.clone(),
                paused: false,
                created: now,
                updated: now,
            });
        t.participants.insert(d.from.clone());
        t.participants.extend(to.iter().cloned());
        t.participants.extend(cc.iter().cloned());
        t.messages.push(id.clone());
        t.updated = now;
        if t.messages.len() >= self.max_thread && !t.paused {
            t.paused = true;
            journal.append(
                "thread_paused",
                json!({ "thread": thread, "messages": t.messages.len() }),
            );
        }

        // The sender's answers settle what it owed in this thread.
        let mut objective_done = false;
        if d.kind != Kind::FollowUp {
            let settled: Vec<(String, String)> = self
                .obligations
                .iter()
                .filter(|(_, o)| o.thread == thread && o.debtor == d.from)
                .filter(|(_, o)| match o.expects {
                    Expects::Reply => true,
                    // The board's objective is done only when closed: a
                    // report on it without close is progress.
                    Expects::Done if self.kickoff.as_deref() == Some(o.message.as_str()) => d.close,
                    Expects::Done => d.close || d.kind == Kind::Report,
                    Expects::Nothing => true,
                })
                .map(|(k, _)| k.clone())
                .collect();
            for k in settled {
                if let Some(o) = self.obligations.remove(&k) {
                    objective_done |= o.creditor == BOARD
                        && self.kickoff.as_deref() == Some(o.message.as_str())
                        && d.close;
                    journal.append("settled", json!({ "message": o.message, "thread": o.thread, "debtor": o.debtor, "creditor": o.creditor, "by": id }));
                }
            }
        }
        if expects != Expects::Nothing && d.kind != Kind::FollowUp {
            for r in &to {
                if r == BOARD {
                    continue;
                }
                // The board's requests are answered in time even when it
                // gives no date: it is followed up like any due request.
                let due = d
                    .due
                    .or_else(|| (d.from == BOARD).then_some(now + BOARD_ANSWER_SECS));
                let every = match (d.follow_up.every, due) {
                    (Some(e), _) => e.max(MIN_FOLLOW_UP_SECS),
                    (None, Some(due)) => (due - now).max(900.0),
                    (None, None) => 0.0,
                };
                let max = if d.follow_up.off {
                    0
                } else {
                    d.follow_up.max.unwrap_or(DEFAULT_FOLLOW_UPS)
                };
                self.obligations.insert(
                    (id.clone(), r.clone()),
                    Obligation {
                        message: id.clone(),
                        thread: thread.clone(),
                        subject: subject.clone(),
                        debtor: r.clone(),
                        creditor: d.from.clone(),
                        expects,
                        kind: d.kind,
                        created: now,
                        due,
                        follow_ups: 0,
                        max_follow_ups: max,
                        every,
                        next: due,
                        escalated: false,
                    },
                );
            }
        }
        let recipients: Vec<String> = to.iter().chain(cc.iter()).cloned().collect();
        for r in &recipients {
            self.unread.entry(r.clone()).or_default().insert(id.clone());
            if !system && chart.worker(r).is_some() && chart.worker(&d.from).is_some() {
                let key = if d.from < *r {
                    (d.from.clone(), r.clone())
                } else {
                    (r.clone(), d.from.clone())
                };
                self.pair_log.entry(key).or_default().push_back(now);
            }
        }
        if d.from == BOARD && d.kind == Kind::Assignment && self.kickoff.is_none() {
            self.kickoff = Some(id.clone());
        }
        self.messages.insert(id.clone(), meta);
        Ok(Sent {
            id,
            thread,
            recipients,
            objective_done,
        })
    }

    /// The text of a message, from the journal.
    pub fn body(&self, journal: &Journal, id: &str) -> String {
        self.messages
            .get(id)
            .and_then(|m| journal.read(m.at).ok())
            .and_then(|v| v["body"].as_str().map(str::to_string))
            .unwrap_or_default()
    }

    pub fn can_read(&self, viewer: &str, thread: &str) -> bool {
        viewer == BOARD
            || self
                .threads
                .get(thread)
                .is_some_and(|t| t.participants.contains(viewer))
    }

    /// A message with its body, if `viewer` may read it.
    pub fn message(&self, journal: &Journal, id: &str, viewer: &str) -> Result<Value> {
        let m = self
            .messages
            .get(id)
            .ok_or_else(|| anyhow!("no message {id}"))?;
        if !self.can_read(viewer, &m.thread) {
            bail!("no message {id}");
        }
        let mut v = serde_json::to_value(m)?;
        v["body"] = json!(self.body(journal, id));
        Ok(v)
    }

    /// A thread, oldest first, with bodies.
    pub fn thread(&self, journal: &Journal, id: &str, viewer: &str) -> Result<Value> {
        let t = self
            .threads
            .get(id)
            .ok_or_else(|| anyhow!("no thread {id}"))?;
        if !self.can_read(viewer, id) {
            bail!("no thread {id}");
        }
        let messages: Vec<Value> = t
            .messages
            .iter()
            .filter_map(|m| self.message(journal, m, viewer).ok())
            .collect();
        Ok(json!({ "thread": t, "messages": messages }))
    }

    pub fn mark_read(&mut self, who: &str, ids: &[String]) {
        if let Some(u) = self.unread.get_mut(who) {
            for id in ids {
                u.remove(id);
            }
        }
    }

    pub fn unread(&self, who: &str) -> Vec<String> {
        self.unread
            .get(who)
            .map(|u| u.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Messages to `who`, newest first.
    pub fn inbox(&self, who: &str, unread_only: bool, limit: usize) -> Vec<Value> {
        let unread = self.unread.get(who);
        self.messages
            .values()
            .rev()
            .filter(|m| m.to.iter().chain(&m.cc).any(|r| r == who))
            .filter(|m| !unread_only || unread.is_some_and(|u| u.contains(&m.id)))
            .take(limit)
            .map(|m| {
                let mut v = serde_json::to_value(m).unwrap();
                v["unread"] = json!(unread.is_some_and(|u| u.contains(&m.id)));
                v
            })
            .collect()
    }

    /// Settles what `debtor` owes in `thread`: an answer given another way
    /// (accepting a contract's quote sends the job in a new thread).
    pub fn settle_thread(&mut self, journal: &Journal, thread: &str, debtor: &str, by: &str) {
        let keys: Vec<(String, String)> = self
            .obligations
            .iter()
            .filter(|(_, o)| o.thread == thread && o.debtor == debtor)
            .map(|(k, _)| k.clone())
            .collect();
        for k in keys {
            if let Some(o) = self.obligations.remove(&k) {
                journal.append("settled", json!({ "message": o.message, "thread": o.thread, "debtor": o.debtor, "creditor": o.creditor, "by": by }));
            }
        }
    }

    /// Whether this draft would close the board's objective: its sender
    /// owes it in the thread it answers, and it closes.
    /// A task: `thread` and the threads delegated from it, however deep
    /// (each new thread's `context` is the one its sender was working in).
    pub fn task_threads(&self, thread: &str) -> BTreeSet<String> {
        let mut task = BTreeSet::from([thread.to_string()]);
        loop {
            let more: Vec<String> = self
                .threads
                .values()
                .filter(|t| !task.contains(&t.id))
                .filter(|t| t.context.as_ref().is_some_and(|c| task.contains(c)))
                .map(|t| t.id.clone())
                .collect();
            if more.is_empty() {
                return task;
            }
            task.extend(more);
        }
    }

    pub fn would_close_objective(&self, d: &Draft) -> bool {
        d.close
            && d.in_reply_to
                .as_ref()
                .and_then(|id| self.messages.get(id))
                .is_some_and(|m| self.closes_objective(&m.thread, &d.from))
    }

    /// Whether `who`, answering in `thread`, would close the board's
    /// objective: they owe it there.
    fn closes_objective(&self, thread: &str, who: &str) -> bool {
        let Some(k) = self.kickoff.as_deref() else {
            return false;
        };
        self.obligations
            .values()
            .any(|o| o.message == k && o.thread == thread && o.debtor == who && o.creditor == BOARD)
    }

    pub fn desk(&self, who: &str) -> Desk {
        let mut desk = Desk::default();
        for o in self.obligations.values() {
            if o.debtor == who {
                desk.owed.push(o.clone());
            }
            if o.creditor == who {
                desk.waiting.push(o.clone());
            }
        }
        desk
    }

    /// Follow-ups and escalations that are due.
    pub fn tick(&mut self, journal: &Journal, now: f64) -> Vec<Sent> {
        let due: Vec<(String, String)> = self
            .obligations
            .iter()
            .filter(|(_, o)| o.next.is_some_and(|n| now >= n))
            .map(|(k, _)| k.clone())
            .collect();
        let mut out = Vec::new();
        for key in due {
            let Some(o) = self.obligations.get(&key).cloned() else {
                continue;
            };
            if o.follow_ups < o.max_follow_ups {
                let overdue = now - o.due.unwrap_or(now);
                let original = self.body(journal, &o.message);
                let quoted: String = original
                    .lines()
                    .take(20)
                    .map(|l| format!("> {l}\n"))
                    .collect();
                let what = if o.expects == Expects::Done {
                    "report"
                } else {
                    "reply"
                };
                let body = format!(
                    "[Automatic follow-up on behalf of {creditor}]\n{creditor} is still waiting for your {what} on \"{subject}\", due {due} ({overdue} ago). This is follow-up {n} of {max}.\n\nTheir request:\n{quoted}\nIf you can't finish, reply with what you have, what's left, and when you expect to finish; or explain what's blocking you.\n",
                    creditor = o.creditor,
                    subject = o.subject,
                    due = super::prompts::when(o.due.unwrap_or(now)),
                    overdue = super::prompts::ago(overdue),
                    n = o.follow_ups + 1,
                    max = o.max_follow_ups,
                );
                let mut d = Draft::new(
                    &o.creditor,
                    vec![o.debtor.clone()],
                    "",
                    &body,
                    Kind::FollowUp,
                );
                d.in_reply_to = Some(o.message.clone());
                d.automatic = true;
                if let Ok(sent) = self.send(journal, d, now) {
                    out.push(sent);
                }
                if let Some(ob) = self.obligations.get_mut(&key) {
                    ob.follow_ups += 1;
                    ob.next = Some(now + ob.every.max(MIN_FOLLOW_UP_SECS));
                }
            } else if !o.escalated {
                if let Some(ob) = self.obligations.get_mut(&key) {
                    ob.escalated = true;
                    ob.next = None;
                }
                let manager = self
                    .chart()
                    .manager_of(&o.debtor)
                    .unwrap_or(BOARD)
                    .to_string();
                let overdue = super::prompts::ago(now - o.due.unwrap_or(now));
                let notice = format!(
                    "[Automatic notice] A request to {debtor} (your report) is stuck.\n\"{subject}\" from {creditor}, due {due}, followed up {max} times without an answer. Thread: {thread}. You may want to check on {debtor} or reassign it.\n",
                    debtor = o.debtor,
                    subject = o.subject,
                    creditor = o.creditor,
                    due = super::prompts::when(o.due.unwrap_or(now)),
                    max = o.max_follow_ups,
                    thread = o.thread,
                );
                let mut d = Draft::new(
                    OFFICE,
                    vec![manager.clone()],
                    &format!("Stuck: {}", o.subject),
                    &notice,
                    Kind::Fyi,
                );
                d.automatic = true;
                if manager != o.creditor {
                    if let Ok(sent) = self.send_notice(journal, d, now) {
                        out.push(sent);
                    }
                }
                let note = format!(
                    "Your request to {debtor}, \"{subject}\", is {overdue} overdue and wasn't answered after {max} follow-ups. {manager} has been told.\nYou can keep waiting, ask someone else, or tell whoever assigned it to you.\n",
                    debtor = o.debtor,
                    subject = o.subject,
                    max = o.max_follow_ups,
                );
                let mut d = Draft::new(
                    OFFICE,
                    vec![o.creditor.clone()],
                    &format!("Unanswered: {}", o.subject),
                    &note,
                    Kind::Fyi,
                );
                d.automatic = true;
                if let Ok(sent) = self.send_notice(journal, d, now) {
                    out.push(sent);
                }
                journal.append("escalated", json!({ "message": o.message, "thread": o.thread, "debtor": o.debtor, "creditor": o.creditor, "manager": manager }));
            }
        }
        out
    }

    /// A notice from the post office itself: it isn't a worker, so it skips
    /// address and authority checks on the sender.
    pub fn send_notice(&mut self, journal: &Journal, d: Draft, now: f64) -> Result<Sent> {
        let mut d = d;
        d.automatic = true;
        d.from = OFFICE.into();
        self.send(journal, d, now)
    }

    /// The org chart for `directory()`.
    pub fn directory(&self) -> Value {
        let chart = self.chart();
        let workers: Vec<Value> = chart
            .workers
            .iter()
            .map(|w| {
                json!({
                    "name": w.name, "title": chart.title_of(&w.name), "role": w.role,
                    "reports_to": w.reports_to.clone().unwrap_or_else(|| BOARD.into()),
                    "reports": chart.reports_of(&w.name),
                    "security_officer": chart.is_officer(&w.name),
                })
            })
            .collect();
        let left: Vec<Value> = self
            .departed
            .iter()
            .map(|(w, m)| json!({ "name": w, "mail_goes_to": self.forward(m) }))
            .collect();
        json!({ "board": { "address": BOARD, "name": chart.board.name }, "workers": workers, "left": left })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zapper_policy::Config;

    fn office() -> (PostOffice, Journal, std::path::PathBuf) {
        let c = Config::from_yaml(
            "profiles: { w: {} }\nfirms:\n  acme:\n    roles: { r: { profile: w, agent: x } }\n    workers:\n      - { name: ceo, role: r }\n      - { name: cto, role: r, reports_to: ceo }\n      - { name: ada, role: r, reports_to: cto }\n      - { name: ken, role: r, reports_to: ceo }\n",
        )
        .unwrap();
        let dir =
            std::env::temp_dir().join(format!("zapper-mail-{}", zapper_core::util::random_id(6)));
        std::fs::create_dir_all(&dir).unwrap();
        let j = Journal::create(&dir.join("journal.jsonl")).unwrap();
        let f = crate::firm::get(&c, "acme").unwrap();
        (
            PostOffice::new(&f, Arc::new(RwLock::new(f.clone()))),
            j,
            dir,
        )
    }

    fn draft(from: &str, to: &str, kind: Kind) -> Draft {
        Draft::new(from, vec![to.into()], "Subject", "Body line\nmore", kind)
    }

    #[test]
    fn the_boards_requests_are_followed_up_without_a_date() {
        let (mut po, j, dir) = office();
        po.send(&j, draft(BOARD, "ceo", Kind::Assignment), 100.0)
            .unwrap();
        let due = po.obligations.values().next().unwrap().due;
        assert_eq!(due, Some(100.0 + BOARD_ANSWER_SECS));
        // The ceo's own assignment without a date is left alone.
        po.send(&j, draft("ceo", "cto", Kind::Assignment), 100.0)
            .unwrap();
        assert!(po
            .obligations
            .values()
            .any(|o| o.debtor == "cto" && o.due.is_none()));
        // Unanswered, the post office follows up for the board.
        let sent = po.tick(&j, 100.0 + BOARD_ANSWER_SECS + 1.0);
        assert!(sent
            .iter()
            .any(|s| s.recipients.contains(&"ceo".to_string())));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_first_board_assignment_is_the_objective() {
        let (mut po, j, dir) = office();
        let a = po
            .send(&j, draft(BOARD, "ceo", Kind::Assignment), 1.0)
            .unwrap();
        let b = po
            .send(&j, draft(BOARD, "ceo", Kind::Assignment), 2.0)
            .unwrap();
        let mut r = draft("ceo", BOARD, Kind::Report);
        r.in_reply_to = Some(b.id.clone());
        r.close = true;
        assert!(
            !po.send(&j, r, 3.0).unwrap().objective_done,
            "a later assignment is not the objective"
        );
        // A progress report on it, without close, isn't the end.
        let mut r = draft("ceo", BOARD, Kind::Report);
        r.in_reply_to = Some(a.id.clone());
        assert!(!po.send(&j, r, 3.5).unwrap().objective_done);
        // Nor can it close while work the firm owes is open.
        let job = po
            .send(&j, draft("ceo", "ada", Kind::Assignment), 3.6)
            .unwrap();
        let mut r = draft("ceo", BOARD, Kind::Report);
        r.in_reply_to = Some(a.id.clone());
        r.close = true;
        let e = po.send(&j, r.clone(), 3.7).unwrap_err().to_string();
        assert!(e.contains("can't close while assignments are open"), "{e}");
        let mut done = draft("ada", "ceo", Kind::Report);
        done.in_reply_to = Some(job.id.clone());
        done.close = true;
        po.send(&j, done, 3.8).unwrap();
        assert!(po.send(&j, r, 4.0).unwrap().objective_done);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn leaving_drops_what_the_manager_asked() {
        let (mut po, j, dir) = office();
        // ceo asks cto; cto asks ada.
        po.send(&j, draft("ceo", "cto", Kind::Assignment), 1.0)
            .unwrap();
        po.send(&j, draft("cto", "ada", Kind::Assignment), 2.0)
            .unwrap();
        assert_eq!(po.obligations.len(), 2);
        // ada also asks cto, who never read it.
        let q = po
            .send(&j, draft("ada", "cto", Kind::Question), 3.0)
            .unwrap();
        // cto leaves: what ceo asked them is nobody's debt (ceo can't owe or
        // wait for themselves); what they asked ada is ceo's to wait for;
        // ada's question is ceo's to answer, and ceo reads it now.
        let (moved, read) = po.leave("cto", "ceo");
        assert_eq!(moved.len(), 1);
        assert_eq!(read, vec![q.id.clone()]);
        assert!(po.unread("ceo").contains(&q.id));
        assert!(po.unread("cto").is_empty());
        let mut left: Vec<_> = po
            .obligations
            .values()
            .map(|o| (o.creditor.as_str(), o.debtor.as_str()))
            .collect();
        left.sort();
        assert_eq!(left, vec![("ada", "ceo"), ("ceo", "ada")]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn subjects_are_optional() {
        assert_eq!(
            subject_from("\n## **Build** the `widget`\nmore"),
            "Build the widget"
        );
        assert_eq!(subject_from("   "), "(no subject)");
        assert_eq!(subject_from(&"x".repeat(200)).chars().count(), 81);
        let (mut po, j, dir) = office();
        let sent = po
            .send(
                &j,
                Draft::new(
                    "ceo",
                    vec!["cto".into()],
                    "",
                    "Ship it by Friday.\nDetails…",
                    Kind::Assignment,
                ),
                1.0,
            )
            .unwrap();
        assert_eq!(po.threads[&sent.thread].subject, "Ship it by Friday.");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn authority_flows_down() {
        let (mut po, j, dir) = office();
        assert!(po
            .send(&j, draft("board", "ada", Kind::Assignment), 0.0)
            .is_ok());
        assert!(po
            .send(&j, draft("ceo", "ada", Kind::Assignment), 0.0)
            .is_ok());
        assert!(po
            .send(&j, draft("cto", "ada", Kind::Assignment), 0.0)
            .is_ok());
        let e = po
            .send(&j, draft("ada", "cto", Kind::Assignment), 0.0)
            .unwrap_err()
            .to_string();
        assert!(e.contains("can't give cto an assignment"), "{e}");
        assert!(
            po.send(&j, draft("ken", "ada", Kind::Assignment), 0.0)
                .is_err(),
            "a peer's report is not yours"
        );
        assert!(
            po.send(&j, draft("ada", "ken", Kind::Question), 0.0)
                .is_ok(),
            "anyone may ask anyone"
        );
        assert!(po
            .send(&j, draft("ada", "cto", Kind::FollowUp), 0.0)
            .is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn addresses() {
        let (mut po, j, dir) = office();
        let s = po
            .send(
                &j,
                Draft::new("ceo", vec!["all".into()], "Hi", "x", Kind::Fyi),
                0.0,
            )
            .unwrap();
        assert_eq!(s.recipients, vec!["cto", "ada", "ken"]);
        let s = po
            .send(
                &j,
                Draft::new("ceo", vec!["team:ceo".into()], "Hi", "x", Kind::Fyi),
                0.0,
            )
            .unwrap();
        assert_eq!(s.recipients, vec!["cto", "ken"]);
        assert!(po
            .send(
                &j,
                Draft::new("ceo", vec!["nobody".into()], "Hi", "x", Kind::Fyi),
                0.0
            )
            .is_err());
        assert!(
            po.send(
                &j,
                Draft::new("ceo", vec!["ceo".into()], "Hi", "x", Kind::Fyi),
                0.0
            )
            .is_err(),
            "no recipients"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn obligations_and_desks() {
        let (mut po, j, dir) = office();
        let a = po
            .send(&j, draft("cto", "ada", Kind::Assignment), 0.0)
            .unwrap();
        assert_eq!(po.desk("ada").owed.len(), 1);
        assert_eq!(po.desk("cto").waiting.len(), 1);
        // A question back doesn't settle an assignment.
        let mut q = Draft::new(
            "ada",
            vec!["cto".into()],
            "",
            "which colour?",
            Kind::Question,
        );
        q.in_reply_to = Some(a.id.clone());
        let q = po.send(&j, q, 1.0).unwrap();
        assert_eq!(q.thread, a.thread);
        assert_eq!(po.desk("ada").owed.len(), 1);
        assert_eq!(po.desk("ada").waiting.len(), 1);
        // The answer settles the question.
        let mut r = Draft::new("cto", vec!["ada".into()], "", "blue", Kind::Reply);
        r.in_reply_to = Some(q.id.clone());
        po.send(&j, r, 2.0).unwrap();
        assert_eq!(po.desk("ada").waiting.len(), 0);
        assert_eq!(po.desk("cto").owed.len(), 0);
        // A report settles the assignment.
        let mut done = Draft::new("ada", vec!["cto".into()], "", "done", Kind::Report);
        done.in_reply_to = Some(a.id.clone());
        done.close = true;
        po.send(&j, done, 3.0).unwrap();
        assert!(po.desk("ada").owed.is_empty());
        assert!(po.desk("cto").waiting.is_empty());
        assert_eq!(
            po.thread(&j, &a.thread, "ada").unwrap()["messages"]
                .as_array()
                .unwrap()
                .len(),
            4
        );
        assert!(
            po.thread(&j, &a.thread, "ken").is_err(),
            "not a participant"
        );
        assert!(
            po.thread(&j, &a.thread, "board").is_ok(),
            "the board reads everything"
        );
        assert_eq!(
            po.message(&j, &a.id, "ada").unwrap()["body"],
            "Body line\nmore"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn follow_ups_then_escalation() {
        let (mut po, j, dir) = office();
        let mut d = draft("cto", "ada", Kind::Assignment);
        d.due = Some(100.0);
        d.follow_up = FollowUp {
            every: Some(10.0),
            max: Some(2),
            off: false,
        };
        let a = po.send(&j, d, 0.0).unwrap();
        assert!(po.tick(&j, 50.0).is_empty());
        let f1 = po.tick(&j, 101.0);
        assert_eq!(f1.len(), 1);
        let m = &po.messages[&f1[0].id];
        assert_eq!(
            (m.from.as_str(), m.kind, m.automatic, m.thread.as_str()),
            ("cto", Kind::FollowUp, true, a.thread.as_str())
        );
        assert!(po.body(&j, &f1[0].id).contains("follow-up 1 of 2"));
        assert!(po.tick(&j, 105.0).is_empty());
        assert_eq!(po.tick(&j, 112.0).len(), 1);
        // Escalation: ada's manager is cto, who is also the author, so only
        // the author's note goes out.
        let e = po.tick(&j, 123.0);
        assert_eq!(e.len(), 1);
        assert_eq!(po.messages[&e[0].id].from, OFFICE);
        assert!(po.tick(&j, 500.0).is_empty(), "escalated once");
        // The follow-up didn't settle anything; ada's report does.
        let mut done = Draft::new("ada", vec!["cto".into()], "", "done", Kind::Report);
        done.in_reply_to = Some(a.id.clone());
        po.send(&j, done, 600.0).unwrap();
        assert!(po.desk("ada").owed.is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn loop_guard() {
        let (mut po, j, dir) = office();
        po.pair_per_10min = 4;
        for i in 0..4 {
            let (a, b) = if i % 2 == 0 {
                ("ada", "ken")
            } else {
                ("ken", "ada")
            };
            po.send(&j, draft(a, b, Kind::Fyi), i as f64).unwrap();
        }
        let e = po
            .send(&j, draft("ada", "ken", Kind::Fyi), 5.0)
            .unwrap_err()
            .to_string();
        assert!(e.contains("likely loop"), "{e}");
        assert_eq!(
            po.inbox("board", false, 10).len(),
            1,
            "the board is told once"
        );
        assert!(po.send(&j, draft("ken", "ada", Kind::Fyi), 6.0).is_err());
        assert_eq!(po.inbox("board", false, 10).len(), 1);
        assert!(
            po.send(&j, draft("ada", "ceo", Kind::Fyi), 7.0).is_ok(),
            "other pairs are fine"
        );
        assert!(
            po.send(&j, draft("ada", "ken", Kind::Fyi), 700.0).is_ok(),
            "the window moves"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn limits() {
        let (mut po, j, dir) = office();
        po.mail_per_hour = 2;
        assert!(po.send(&j, draft("ada", "ken", Kind::Fyi), 0.0).is_ok());
        assert!(po.send(&j, draft("ada", "ken", Kind::Fyi), 1.0).is_ok());
        assert!(po.send(&j, draft("ada", "ken", Kind::Fyi), 2.0).is_err());
        assert!(po.send(&j, draft("ada", "ken", Kind::Fyi), 3700.0).is_ok());
        let mut big = draft("ken", "ada", Kind::Fyi);
        big.body = "x".repeat(MAX_BODY + 1);
        assert!(po.send(&j, big, 0.0).is_err());
        po.max_thread = 2;
        let a = po
            .send(&j, draft("ceo", "ken", Kind::Question), 0.0)
            .unwrap();
        let mut r = Draft::new("ken", vec!["ceo".into()], "", "a", Kind::Reply);
        r.in_reply_to = Some(a.id.clone());
        po.send(&j, r.clone(), 1.0).unwrap();
        assert!(po
            .send(&j, r, 2.0)
            .unwrap_err()
            .to_string()
            .contains("paused"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
