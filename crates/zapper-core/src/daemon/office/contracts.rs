//! Contractors: one job each, on terms negotiated by mail. A hirer offers a
//! job to a role; a candidate boots (after the officers' background check),
//! reads the brief and quotes, paid from a quoting fee; the hirer accepts,
//! counters or withdraws. An accepted contract moves the agreed budget to
//! the contractor, who mails only the hirer and whoever the hirer adds, and
//! ends with their report, when the budget is spent, or at the due date.
//! Whatever way it ends, the VM stops and the rest returns to the hirer.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{anyhow, bail, Result};
use serde::Serialize;
use serde_json::{json, Value};

use super::mail::{Draft, Expects, Kind};
use super::workforce::{fmt_tokens, join, let_go, room};
use super::{now, prompts, FirmRun, WorkerState};
use crate::daemon::Daemon;
use zapper_policy::BOARD;

/// Counter-offers before the hirer must accept or walk away.
const MAX_ROUNDS: u32 = 3;
/// An offer lapses after this long without a move from either side.
const OFFER_TTL: f64 = 3600.0;
/// After the due date, the time a contractor has to hand in what exists.
const GRACE: f64 = 600.0;
/// Longest brief, scope or report note, in bytes.
const MAX_TEXT: usize = 20_000;

#[derive(Debug, Clone, Default, Serialize)]
pub struct Terms {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub due: Option<f64>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub scope: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Contract {
    pub id: String,
    pub worker: String,
    /// The hirer.
    pub by: String,
    pub role: String,
    pub title: String,
    pub brief: String,
    /// `offered`, `quoted`, `countered`, `active`; then `completed`,
    /// `declined`, `withdrawn`, `lapsed` or `ended`.
    pub state: String,
    /// The hirer's terms: the offer, then each counter.
    pub offer: Terms,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quote: Option<Terms>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agreed: Option<Terms>,
    pub rounds: u32,
    /// The negotiation's thread, and once accepted the job's message.
    pub thread: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job: Option<String>,
    /// Colleagues the hirer added to the job, whom the contractor may mail.
    pub added: Vec<String>,
    pub created: f64,
    pub updated: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended: Option<String>,
    #[serde(skip)]
    due_told: bool,
}

impl Contract {
    fn open(&self) -> bool {
        matches!(
            self.state.as_str(),
            "offered" | "quoted" | "countered" | "active"
        )
    }
}

#[derive(Default)]
pub struct Contracts {
    pub list: BTreeMap<String, Contract>,
    counter: u64,
}

fn terms_text(t: &Terms) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "- Budget: {}\n",
        t.budget
            .map(|b| format!("{} tokens", fmt_tokens(b)))
            .unwrap_or_else(|| "open: quote one".into())
    ));
    s.push_str(&format!(
        "- Due: {}\n",
        t.due
            .map(prompts::when)
            .unwrap_or_else(|| "open: propose one".into())
    ));
    if !t.scope.trim().is_empty() {
        s.push_str(&format!(
            "- Scope:\n{}\n",
            t.scope
                .lines()
                .map(|l| format!("  {l}\n"))
                .collect::<String>()
        ));
    }
    s
}

/// The open contract of a worker, if they are a contractor.
pub fn of(run: &FirmRun, worker: &str) -> Option<Contract> {
    run.contracts
        .lock()
        .unwrap()
        .list
        .values()
        .find(|c| c.worker == worker && c.open())
        .cloned()
}

fn get(run: &FirmRun, id: &str) -> Result<Contract> {
    run.contracts
        .lock()
        .unwrap()
        .list
        .get(id)
        .cloned()
        .ok_or_else(|| anyhow!("no contract {id}"))
}

fn update(run: &FirmRun, id: &str, f: impl FnOnce(&mut Contract)) -> Contract {
    let mut cs = run.contracts.lock().unwrap();
    let c = cs.list.get_mut(id).expect("contract exists");
    f(c);
    c.updated = now();
    c.clone()
}

/// The contractor's part of their `CLAUDE.md`.
pub fn preamble(c: &Contract) -> String {
    format!(
        "\n## You are a contractor\n\n\
You were offered one job by {by} (contract {id}), as {title}. You are not staff:\n\
you don't hire, contract or assign work, and you mail only {by} and the\n\
colleagues {by} adds to the job.\n\n\
First, the negotiation: read the offer, then answer with contract_quote (the\n\
scope you will deliver and what is out of it, a budget in tokens, a due date,\n\
and what you need) or contract_decline. {by} may accept, counter or walk away;\n\
quote again on a counter (a few rounds at most). Your quoting turns are paid\n\
from a small fee, so quote in one turn. An offer nobody answers for an hour\n\
lapses. Once accepted, the budget is yours and the job arrives as an\n\
assignment; ask for the access it needs (credentials_request) at once.\n\n\
The contract ends when you call contract_complete with your report (it goes\n\
to {by} and your machine stops), when its budget is spent, or {grace} minutes\n\
after the due date (you are told when it passes). Publish or push what you\n\
deliver before you complete: your machine and its files are gone afterwards.\n\
Don't spend turns waiting: if you are blocked, say so to {by} and end your\n\
turn.\n",
        by = c.by,
        id = c.id,
        title = c.title,
        grace = (GRACE / 60.0) as u64,
    )
}

pub struct OfferArgs {
    pub role: String,
    pub title: String,
    pub brief: String,
    pub budget: Option<i64>,
    pub due: Option<f64>,
    pub name: Option<String>,
}

/// A hirer offers a job: a candidate of the role boots, is checked by the
/// officers, and gets the offer as a question to quote on.
pub async fn offer(d: &Arc<Daemon>, run: &Arc<FirmRun>, by: &str, a: OfferArgs) -> Result<Value> {
    let hirer = run.worker(by)?;
    if run
        .config
        .roles
        .get(&hirer.role)
        .is_none_or(|r| r.hires.is_empty())
        || of(run, by).is_some()
    {
        bail!("only those who hire can contract");
    }
    super::security::officer_first(run, by, &a.role)?;
    let role = run.config.roles.get(&a.role).cloned().ok_or_else(|| {
        anyhow!(
            "no role {} (the catalog: {})",
            a.role,
            run.config
                .roles
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        )
    })?;
    if role.officer
        || run
            .config
            .root()
            .and_then(|r| run.config.worker(r))
            .is_some_and(|w| w.role == a.role)
    {
        bail!("a {} can't be contracted", a.role);
    }
    let title = a.title.trim().chars().take(80).collect::<String>();
    if title.is_empty() || a.brief.trim().is_empty() {
        bail!("give a title (e.g. Security auditor) and a brief: the job, its context, what done looks like");
    }
    if a.brief.len() > MAX_TEXT {
        bail!("the brief is too long");
    }
    if a.budget.is_some_and(|b| b <= 0) {
        bail!("budget: tokens, like 2M; or leave it open for a quote");
    }
    let fee = run.config.budget.quote_fee as i64;
    {
        let l = run.ledger.lock().unwrap();
        if !l.enabled {
            bail!("this firm has no run budget, so it doesn't contract");
        }
        if l.balance(by) < fee {
            bail!(
                "a quote costs a {} token fee; you have {}",
                fmt_tokens(fee),
                fmt_tokens(l.balance(by))
            );
        }
    }
    let config = room(d, &a.role, &role)?;
    let id = {
        let mut cs = run.contracts.lock().unwrap();
        cs.counter += 1;
        format!("k-{}", cs.counter)
    };
    let (name, firm, rt) = join(
        run,
        by,
        &a.role,
        a.name.as_deref(),
        Some(title.clone()),
        fee,
        &format!("contract {id}: {title}"),
        Some(&id),
    )?;
    let offer = Terms {
        budget: a.budget,
        due: a.due,
        scope: String::new(),
    };
    let body = format!(
        "**Contract offer {id}: {title}**\n\n{by} offers you this job:\n\n{}\n\nTerms offered:\n{}\nAnswer with contract_quote(contract: \"{id}\", scope, budget, due) or contract_decline(contract: \"{id}\", reason).\n",
        a.brief.trim(),
        terms_text(&offer)
    );
    let mut draft = Draft::new(
        by,
        vec![name.clone()],
        &format!("Contract offer {id}: {title}"),
        &body,
        Kind::Question,
    );
    draft.expects = Some(Expects::Reply);
    draft.context = hirer.context.lock().unwrap().clone();
    super::prompts::onboard(run, &name);
    let sent = run.send(draft)?;
    let now = now();
    let c = Contract {
        id: id.clone(),
        worker: name.clone(),
        by: by.into(),
        role: a.role.clone(),
        title: title.clone(),
        brief: a.brief.trim().into(),
        state: "offered".into(),
        offer,
        quote: None,
        agreed: None,
        rounds: 0,
        thread: sent.thread.clone(),
        job: None,
        added: vec![],
        created: now,
        updated: now,
        ended: None,
        due_told: false,
    };
    run.journal
        .append("contract", json!({ "event": "offered", "contract": c }));
    run.contracts.lock().unwrap().list.insert(id.clone(), c);
    let access = super::workforce::access_summary(run, &config, &a.role);
    let check = super::security::screen(
        run,
        &name,
        by,
        &a.role,
        &firm.title_of(&name),
        fee,
        &format!("contract {id} (a contractor, quoting first)"),
        a.brief.trim(),
        &access,
    );
    run.set_state(&rt, WorkerState::Booting, None);
    let _ = run.boots.send(rt);
    Ok(json!({
        "contract": id, "candidate": name, "thread": sent.thread, "fee": fee, "background_check": check,
        "note": "their VM is booting and the security officers check them; then they quote in the thread. Accept with contract_accept, counter with contract_counter, or walk away with contract_withdraw",
    }))
}

/// The candidate quotes: scope, budget, due date.
pub fn quote(
    run: &FirmRun,
    me: &str,
    id: &str,
    scope: &str,
    budget: i64,
    due: Option<f64>,
) -> Result<Value> {
    let c = get(run, id)?;
    if c.worker != me {
        bail!("contract {id} isn't yours");
    }
    if !matches!(c.state.as_str(), "offered" | "countered") {
        bail!("contract {id} is {}: nothing to quote on", c.state);
    }
    if budget <= 0 || scope.trim().is_empty() || scope.len() > MAX_TEXT {
        bail!("give the scope (what you deliver, what is out) and a budget in tokens");
    }
    let q = Terms {
        budget: Some(budget),
        due,
        scope: scope.trim().into(),
    };
    let c = update(run, id, |c| {
        c.state = "quoted".into();
        c.quote = Some(q.clone());
    });
    run.journal.append(
        "contract",
        json!({ "event": "quoted", "id": id, "worker": me, "terms": q, "round": c.rounds }),
    );
    let mut draft = Draft::new(me, vec![c.by.clone()], "", &format!("**Quote for contract {id}: {}**\n\n{}\nAccept with contract_accept(contract: \"{id}\"), counter with contract_counter, or walk away with contract_withdraw.\n", c.title, terms_text(&q)), Kind::Reply);
    draft.in_reply_to = last_in_thread(run, &c.thread);
    draft.expects = Some(Expects::Reply);
    let sent = run.send(draft)?;
    Ok(json!({ "contract": id, "state": "quoted", "message": sent.id }))
}

fn last_in_thread(run: &FirmRun, thread: &str) -> Option<String> {
    run.post
        .lock()
        .unwrap()
        .threads
        .get(thread)
        .and_then(|t| t.messages.last().cloned())
}

/// The hirer counters: other terms, within a few rounds.
pub fn counter(
    run: &FirmRun,
    by: &str,
    id: &str,
    budget: Option<i64>,
    due: Option<f64>,
    scope: Option<String>,
    note: &str,
) -> Result<Value> {
    let c = get(run, id)?;
    if c.by != by {
        bail!("contract {id} is {}'s", c.by);
    }
    if c.state != "quoted" {
        bail!("contract {id} is {}: counter a quote", c.state);
    }
    if c.rounds >= MAX_ROUNDS {
        bail!("{MAX_ROUNDS} rounds already: accept the quote or walk away");
    }
    let q = c.quote.clone().unwrap_or_default();
    let t = Terms {
        budget: budget.or(q.budget),
        due: due.or(q.due),
        scope: scope
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or(q.scope),
    };
    let c = update(run, id, |c| {
        c.state = "countered".into();
        c.rounds += 1;
        c.offer = t.clone();
    });
    run.journal.append("contract", json!({ "event": "countered", "id": id, "by": by, "terms": t, "round": c.rounds, "note": note.trim() }));
    let mut draft = Draft::new(by, vec![c.worker.clone()], "", &format!("**Counter-offer for contract {id}** (round {} of {MAX_ROUNDS})\n\n{}\n{}\nQuote again with contract_quote, or contract_decline.\n", c.rounds, note.trim(), terms_text(&t)), Kind::Reply);
    draft.in_reply_to = last_in_thread(run, &c.thread);
    draft.expects = Some(Expects::Reply);
    run.send(draft)?;
    Ok(json!({ "contract": id, "state": "countered", "round": c.rounds }))
}

/// The hirer accepts the quote: the budget moves, and the job arrives as
/// an assignment.
pub fn accept(run: &FirmRun, by: &str, id: &str) -> Result<Value> {
    let c = get(run, id)?;
    if c.by != by {
        bail!("contract {id} is {}'s", c.by);
    }
    if c.state != "quoted" {
        bail!("contract {id} is {}: accept a quote", c.state);
    }
    let q = c.quote.clone().ok_or_else(|| anyhow!("no quote"))?;
    let budget = q.budget.unwrap_or(0);
    {
        let mut l = run.ledger.lock().unwrap();
        if l.approved == 0 {
            bail!("the board hasn't approved a budget yet: you can ask for quotes, not contract");
        }
        if l.balance(by) < budget {
            bail!(
                "you have {} tokens; the quote is {}",
                fmt_tokens(l.balance(by)),
                fmt_tokens(budget)
            );
        }
        l.transfer(Some(by), &c.worker, budget as u64);
    }
    run.journal.append("funds", json!({ "event": "move", "from": by, "to": c.worker, "tokens": budget, "reason": format!("contract {id}: {}", c.title) }));
    let c = update(run, id, |c| {
        c.state = "active".into();
        c.agreed = Some(q.clone());
    });
    let body = format!(
        "**Contract {id} accepted: {}**\n\nThe job:\n\n{}\n\nAgreed terms:\n{}\nWork on it, ask me what you need, and finish with contract_complete(contract: \"{id}\", report): the report comes to me and the contract ends.\n",
        c.title,
        c.brief,
        terms_text(&q)
    );
    let mut draft = Draft::new(
        by,
        vec![c.worker.clone()],
        &format!("Contract {id}: {}", c.title),
        &body,
        Kind::Assignment,
    );
    draft.due = q.due;
    let sent = run.send(draft)?;
    // The job answers the quote: the hirer owes nothing more in the
    // negotiation's thread.
    run.post
        .lock()
        .unwrap()
        .settle_thread(&run.journal, &c.thread, by, &sent.id);
    update(run, id, |c| c.job = Some(sent.id.clone()));
    run.journal.append(
        "contract",
        json!({ "event": "accepted", "id": id, "by": by, "terms": q, "job": sent.id }),
    );
    Ok(json!({ "contract": id, "state": "active", "budget": budget, "job": sent.id }))
}

/// The hirer adds a colleague to the job, whom the contractor may mail.
pub fn add(run: &FirmRun, by: &str, id: &str, worker: &str) -> Result<Value> {
    let c = get(run, id)?;
    if c.by != by {
        bail!("contract {id} is {}'s", c.by);
    }
    if !c.open() {
        bail!("contract {id} is {}", c.state);
    }
    if run.rt(worker).is_none() || worker == c.worker {
        bail!("no colleague {worker}");
    }
    let c = update(run, id, |c| {
        if !c.added.iter().any(|w| w == worker) {
            c.added.push(worker.into());
        }
    });
    run.journal.append(
        "contract",
        json!({ "event": "added", "id": id, "worker": worker, "by": by }),
    );
    run.notice(&c.worker, &format!("{worker} joins contract {id}"), &format!("[Automatic notice] {by} added {worker} ({}) to contract {id}: you may mail them about the job.\n", run.chart().title_of(worker)));
    run.notice(worker, &format!("Contract {id}: {}", c.title), &format!("[Automatic notice] {by} added you to contract {id} ({}, by {}). They may mail you about the job.\n", c.title, c.worker));
    Ok(json!({ "contract": id, "added": c.added }))
}

/// The contractor delivers: the report goes to the hirer, the contract ends.
pub fn complete(
    d: &Arc<Daemon>,
    run: &Arc<FirmRun>,
    me: &str,
    id: &str,
    report: &str,
    attachments: Vec<String>,
) -> Result<Value> {
    let c = get(run, id)?;
    if c.worker != me {
        bail!("contract {id} isn't yours");
    }
    if c.state != "active" {
        bail!("contract {id} is {}", c.state);
    }
    if report.trim().is_empty() {
        bail!("give your report: what you delivered, where it is, what is left");
    }
    // The board is owed its report before the machine stops.
    if super::scenario::owes_report(run, me) {
        bail!("you haven't reported to the board yet: mail it (to: board) your report or debrief first, as the board's instructions to you say, then complete the contract");
    }
    let mut draft = Draft::new(
        me,
        vec![c.by.clone()],
        "",
        &format!(
            "**Contract {id} complete: {}**\n\n{}",
            c.title,
            report.trim()
        ),
        Kind::Report,
    );
    draft.in_reply_to = c.job.clone();
    draft.close = true;
    draft.attachments = attachments;
    let sent = run.send(draft)?;
    let out = end(d, run, id, "completed", "delivered");
    Ok(
        json!({ "contract": id, "state": "completed", "report": sent.id, "returned": out["returned"] }),
    )
}

/// The candidate turns the offer down.
pub fn decline(
    d: &Arc<Daemon>,
    run: &Arc<FirmRun>,
    me: &str,
    id: &str,
    reason: &str,
) -> Result<Value> {
    let c = get(run, id)?;
    if c.worker != me {
        bail!("contract {id} isn't yours");
    }
    if !matches!(c.state.as_str(), "offered" | "countered") {
        bail!("contract {id} is {}", c.state);
    }
    let mut draft = Draft::new(
        me,
        vec![c.by.clone()],
        "",
        &format!("**Declined contract {id}**: {}", reason.trim()),
        Kind::Reply,
    );
    draft.in_reply_to = last_in_thread(run, &c.thread);
    run.send(draft)?;
    end(d, run, id, "declined", reason);
    Ok(json!({ "contract": id, "state": "declined" }))
}

/// The hirer walks away from an offer.
pub fn withdraw(
    d: &Arc<Daemon>,
    run: &Arc<FirmRun>,
    by: &str,
    id: &str,
    reason: &str,
) -> Result<Value> {
    let c = get(run, id)?;
    if c.by != by {
        bail!("contract {id} is {}'s", c.by);
    }
    if !matches!(c.state.as_str(), "offered" | "quoted" | "countered") {
        bail!(
            "contract {id} is {}{}",
            c.state,
            if c.state == "active" {
                ": end it with release"
            } else {
                ""
            }
        );
    }
    end(d, run, id, "withdrawn", reason);
    Ok(json!({ "contract": id, "state": "withdrawn" }))
}

/// A contract ends: the contractor's VM stops, and what is left of their
/// budget returns to the hirer.
fn end(d: &Arc<Daemon>, run: &Arc<FirmRun>, id: &str, state: &str, reason: &str) -> Value {
    let started = get(run, id).is_ok_and(|c| c.state == "active");
    let c = update(run, id, |c| {
        c.state = state.into();
        c.ended = Some(reason.trim().chars().take(2000).collect());
    });
    run.journal.append(
        "contract",
        json!({ "event": state, "id": id, "worker": c.worker, "reason": c.ended }),
    );
    if !started {
        super::scenario::release(run, &c.worker, &format!("contract {id} {state}"));
    }
    let out = let_go(
        d,
        run,
        &format!("contract {id}"),
        &c.worker,
        &format!("contract {id} {state}: {}", reason.trim()),
        true,
    );
    if state != "completed" {
        run.notice(&c.by, &format!("Contract {id} {state}"), &format!("[Automatic notice] Contract {id} ({}, {}) is {state}: {}. {} tokens came back to you.\n", c.title, c.worker, reason.trim(), out["returned"]));
    }
    out
}

/// Someone left by other means (release): their open contract ends.
pub fn left(run: &FirmRun, worker: &str, reason: &str) {
    let open: Vec<(String, bool)> = run
        .contracts
        .lock()
        .unwrap()
        .list
        .values()
        .filter(|c| c.worker == worker && c.open())
        .map(|c| (c.id.clone(), c.state == "active"))
        .collect();
    for (id, started) in open {
        if !started {
            super::scenario::release(run, worker, &format!("contract {id} ended"));
        }
        update(run, &id, |c| {
            c.state = "ended".into();
            c.ended = Some(reason.trim().chars().take(2000).collect());
        });
        run.journal.append(
            "contract",
            json!({ "event": "ended", "id": id, "worker": worker, "reason": reason.trim() }),
        );
    }
}

/// Whether a contractor may mail these people: their hirer and whoever the
/// hirer added; not the board, not lists.
pub fn may_mail(run: &FirmRun, from: &str, recipients: &[String]) -> Result<()> {
    let Some(c) = of(run, from) else {
        return Ok(());
    };
    for r in recipients {
        if r == BOARD && super::scenario::board_line(run, from) {
            super::scenario::note_report(run, from);
            continue;
        }
        if *r != c.by && !c.added.contains(r) {
            bail!("as a contractor you mail {} and the colleagues they add to the job; ask {} to add {r}", c.by, c.by);
        }
    }
    Ok(())
}

/// An active contract ends for want of budget or time. A contractor with a
/// board line first gets a last turn to debrief the board.
fn finish(
    d: &Arc<Daemon>,
    run: &Arc<FirmRun>,
    c: &Contract,
    state: &'static str,
    reason: &'static str,
) {
    if !super::scenario::board_line(run, &c.worker) {
        end(d, run, &c.id, state, reason);
    } else if super::scenario::begin_debrief(run, &c.worker) {
        let (d, run, c) = (d.clone(), run.clone(), c.clone());
        tokio::spawn(async move {
            super::scenario::debrief(&run, &c.worker, reason).await;
            if get(&run, &c.id).is_ok_and(|c| c.open()) {
                end(&d, &run, &c.id, state, reason);
            }
        });
    }
}

/// Offers that went quiet lapse; due dates and spent budgets end contracts.
pub fn tick(d: &Arc<Daemon>, run: &Arc<FirmRun>) {
    let t = now();
    let list: Vec<Contract> = run
        .contracts
        .lock()
        .unwrap()
        .list
        .values()
        .filter(|c| c.open())
        .cloned()
        .collect();
    for c in list {
        if c.state != "active" {
            if t - c.updated > OFFER_TTL {
                end(d, run, &c.id, "lapsed", "no answer within an hour");
            }
            continue;
        }
        let spent = run.ledger.lock().unwrap().balance(&c.worker) <= 0;
        let due = c.agreed.as_ref().and_then(|a| a.due);
        if spent {
            finish(d, run, &c, "ended", "its budget is spent");
        } else if due.is_some_and(|due| t > due + GRACE) {
            finish(d, run, &c, "ended", "the due date passed");
        } else if due.is_some_and(|due| t > due) && !c.due_told {
            update(run, &c.id, |c| c.due_told = true);
            run.notice(&c.worker, &format!("Contract {} is due", c.id), &format!("[Automatic notice] Contract {} was due at {}. Hand in what exists now with contract_complete: the contract ends in {} minutes either way.\n", c.id, prompts::when(due.unwrap()), (GRACE / 60.0) as u64));
        }
    }
}

/// Every contract of the run, for the API and the UI.
pub fn list(run: &FirmRun) -> Vec<Value> {
    run.contracts
        .lock()
        .unwrap()
        .list
        .values()
        .map(|c| serde_json::to_value(c).unwrap())
        .collect()
}

/// A hirer's contracts, for their tool.
pub fn mine(run: &FirmRun, by: &str) -> Vec<Value> {
    run.contracts
        .lock()
        .unwrap()
        .list
        .values()
        .filter(|c| c.by == by || c.worker == by)
        .map(|c| serde_json::to_value(c).unwrap())
        .collect()
}
