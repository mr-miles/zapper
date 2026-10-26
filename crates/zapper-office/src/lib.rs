//! The office: firms of workers, each a Claude Code in its own session, who
//! talk by mail, publish to a shared drive, and whose network access security
//! officers decide. `docs/office.md` is the design.
//!
//! A run starts fresh with every `firm up` and is written to
//! `state/firms/<firm>/runs/<id>/` as it happens; the daemon keeps indexes.

pub(crate) mod access;
pub(crate) mod contracts;
pub(crate) mod drive;
pub(crate) mod facility;
pub mod firm;
pub(crate) mod foreman;
pub(crate) mod git;
pub(crate) mod http;
pub(crate) mod journal;
pub(crate) mod mail;
pub(crate) mod ops;
pub(crate) mod plan;
pub(crate) mod product;
pub(crate) mod prompts;
pub(crate) mod scenario;
pub(crate) mod security;
pub mod skills;
pub(crate) mod workforce;

mod history;
mod run;

pub(crate) use history::{close_orphans, list_runs, prune, run_config, run_dir, set_note};
#[cfg(test)]
pub(crate) use run::test_run;
pub(crate) use run::{
    board_send, default_company, down, effective_model, pause, up_with, Agents, StartOptions,
};
pub(crate) use run::{chart, mem_available_mib, pause_for, worker_json};

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use crate::firm::{Firm, BOARD};
use anyhow::{anyhow, bail, Result};
use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::{Notify, Semaphore};
use zapper_policy::Policy;

use zapper_core::util::{now, random_id};
use zapper_core::Core;

/// The office's state in the core: its registry of runs.
pub(crate) trait OfficeOf {
    fn office(&self) -> Arc<Office>;
}

impl OfficeOf for Core {
    fn office(&self) -> Arc<Office> {
        self.client()
    }
}
use journal::Journal;
use mail::{Draft, Kind};
use zapper_core::Session;

/// Every firm's current or last run.
#[derive(Default)]
pub(crate) struct Office {
    pub runs: RwLock<BTreeMap<String, Arc<FirmRun>>>,
}

impl Office {
    pub fn run(&self, firm: &str) -> Result<Arc<FirmRun>> {
        self.runs
            .read()
            .unwrap()
            .get(firm)
            .cloned()
            .ok_or_else(|| anyhow!("no run of firm {firm}; start it with `zapper firm up {firm}`"))
    }

    pub fn active(&self, firm: &str) -> Result<Arc<FirmRun>> {
        let r = self.run(firm)?;
        if r.ended() {
            bail!("firm {firm} isn't running; start it with `zapper firm up {firm}`");
        }
        Ok(r)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum RunState {
    Starting,
    Running,
    Paused,
    Stopping,
    Ended,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum WorkerState {
    Off,
    Booting,
    Idle,
    Waiting,
    Queued,
    Working,
    Reviewing,
    OverBudget,
    Error,
}

/// A worker of a running firm.
pub(crate) struct WorkerRt {
    pub name: String,
    pub role: String,
    pub title: String,
    /// A security officer now: officers change during a run (see
    /// `FirmRun::set_officers`).
    officer: AtomicBool,
    pub session: Mutex<Option<Arc<Session>>>,
    pub state: Mutex<(WorkerState, Option<String>)>,
    /// Messages not yet shown to the worker.
    pub queue: Mutex<Vec<String>>,
    /// Texts to put before the next wake prompt.
    pub notes: Mutex<Vec<String>>,
    /// Access requests waiting for this officer.
    pub access_queue: Mutex<Vec<String>>,
    /// The officer has a traffic digest to read.
    pub digest_due: AtomicBool,
    /// Times the Stop hook extended the current turn with new mail.
    pub extended: std::sync::atomic::AtomicU32,
    pub wake: Notify,
    /// The thread the current turn is about, for new threads it starts.
    pub context: Mutex<Option<String>>,
}

impl WorkerRt {
    pub fn new(firm: &Firm, w: &crate::firm::Worker) -> WorkerRt {
        WorkerRt {
            name: w.name.clone(),
            role: w.role.clone(),
            title: firm.title_of(&w.name),
            officer: AtomicBool::new(firm.is_officer(&w.name)),
            session: Mutex::new(None),
            state: Mutex::new((WorkerState::Off, None)),
            queue: Mutex::new(vec![]),
            notes: Mutex::new(vec![]),
            access_queue: Mutex::new(vec![]),
            digest_due: AtomicBool::new(false),
            extended: std::sync::atomic::AtomicU32::new(0),
            wake: Notify::new(),
            context: Mutex::new(None),
        }
    }

    pub fn state(&self) -> WorkerState {
        self.state.lock().unwrap().0
    }

    pub fn officer(&self) -> bool {
        self.officer.load(Ordering::Relaxed)
    }

    pub fn session(&self) -> Option<Arc<Session>> {
        self.session.lock().unwrap().clone()
    }
}

/// Turn and token use, reset at midnight UTC.
#[derive(Debug, Default, Serialize)]
pub(crate) struct Usage {
    pub day: i64,
    pub turns: u32,
    pub tokens: u64,
    pub worker_turns: HashMap<String, u32>,
    pub notified: bool,
}

pub(crate) struct FirmRun {
    /// A handle to the run's own `Arc`, so a `&self` method that must call an
    /// API taking `&Arc<FirmRun>` (provisioning a hire from a plan) can recover
    /// one.
    pub me: std::sync::Weak<FirmRun>,
    /// The core, held weakly (the core owns the office that owns this run), for
    /// the same provisioning path, which needs a `&Core`.
    pub core: zapper_core::WeakCore,
    pub firm: String,
    pub id: String,
    pub dir: PathBuf,
    /// The firm's config as the run started: roles, budgets, limits,
    /// services. The chart is in `chart`.
    pub config: Firm,
    /// The live org chart: the founders, and whoever was hired since. A leaf
    /// lock: nothing else is locked while it is held.
    pub chart: Arc<RwLock<Firm>>,
    pub domain: String,
    pub started: f64,
    pub state: Mutex<RunState>,
    pub journal: Journal,
    pub post: mail::Post,
    workers: RwLock<BTreeMap<String, Arc<WorkerRt>>>,
    /// The run's budget: the board's grants, balances, budget requests.
    pub ledger: workforce::Books,
    /// The model each role's workers run, as the run started.
    pub models: BTreeMap<String, Option<String>>,
    /// Each office profile's agent and model, for workers on their own
    /// profile (a founder's, or a scenario slot's).
    pub agents: Agents,
    /// The company's name for this run.
    pub company: String,
    /// The board's extra instructions for founders, by name.
    pub prompts: BTreeMap<String, String>,
    /// The board's own articles of the charter, for every worker.
    pub articles: String,
    /// The scenario the run plays, if the board picked one.
    pub scenario: Option<scenario::Play>,
    /// Who tried to close the board's objective while their team was busy:
    /// told once the team is idle, so they wake to close it.
    pub close_waiter: Mutex<Option<String>>,
    /// Names that were in the firm and left: they can't be hired again.
    pub departed: Mutex<BTreeMap<String, String>>,
    /// Hires to boot: the run's booter boots their VMs and starts their
    /// foremen, outside the office request that hired them.
    pub boots: tokio::sync::mpsc::UnboundedSender<Arc<WorkerRt>>,
    pub drive: Mutex<drive::Drive>,
    pub security: Mutex<security::Security>,
    /// Contract offers, negotiations and contracts.
    pub contracts: Mutex<contracts::Contracts>,
    /// Credential grants and requests.
    pub access: Mutex<access::Access>,
    /// Officers' standing approvals per role.
    pub roles: BTreeMap<String, Arc<RwLock<Policy>>>,
    pub turns: Arc<Semaphore>,
    pub usage: Mutex<Usage>,
    pub facility: facility::Facilities,
    /// The office's tools' registration in the gateway.
    pub tools: Mutex<Option<u64>>,
    /// Staged plans a worker authored with `plan`, released stage by stage.
    pub plans: Mutex<plan::Plans>,
    pub stopping: Notify,
}

/// The labels of a session that belongs to a firm: the firm, its run, the
/// member (a worker's or facility's name) and which of the two it is.
pub(crate) fn labels(
    firm: &str,
    run: &str,
    member: &str,
    worker: bool,
) -> BTreeMap<String, String> {
    [
        ("firm", firm),
        ("run", run),
        ("member", member),
        ("office", if worker { "worker" } else { "facility" }),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

/// The run a session belongs to, from its labels: the firm's run, if it is
/// the one that started the session.
pub(crate) fn run_of(office: &Office, s: &Session) -> Option<Arc<FirmRun>> {
    let run = office.runs.read().unwrap().get(s.label("firm")?)?.clone();
    (Some(run.id.as_str()) == s.label("run")).then_some(run)
}

/// `20261016-140501-k3z`, sortable by time.
fn run_id() -> String {
    let t = time::OffsetDateTime::now_utc();
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}-{}",
        t.year(),
        t.month() as u8,
        t.day(),
        t.hour(),
        t.minute(),
        t.second(),
        random_id(3)
    )
}

/// Whether `s` has the form `run_id` makes, so it can go into a path.
pub(crate) fn is_run_id(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 19
        && b[8] == b'-'
        && b[15] == b'-'
        && b[..8].iter().chain(&b[9..15]).all(u8::is_ascii_digit)
        && b[16..]
            .iter()
            .all(|c| zapper_core::util::ID_CHARS.contains(c))
}

/// A run's sessions, by their labels: what the run registers services, its
/// zone and its decider for.
pub(crate) fn run_sessions(run: &FirmRun) -> BTreeMap<String, String> {
    [("firm", &run.firm), ("run", &run.id)]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

pub(crate) fn firm_dir(d: &Core, firm: &str) -> PathBuf {
    d.state_dir().join("firms").join(firm)
}

fn utc_day(ts: f64) -> i64 {
    (ts / 86400.0).floor() as i64
}

/// How a plan stage's child reaches the worker its slot resolves to.
enum Dispatch {
    /// An existing (named or already-on-the-chart) worker: the plan sends the
    /// child to them as an assignment.
    Mail(String),
    /// A worker just hired for the slot: the hire already sent this child as
    /// their first brief, on the returned thread.
    Briefed(String),
}

/// A child a stage could not send: its slot and why. Reported in bulk so a
/// stage does the work it can and the planner (or owner) sees all the gaps at
/// once, rather than the release stopping at the first.
#[derive(Serialize)]
pub(crate) struct PlanProblem {
    pub slot: String,
    pub error: String,
}

impl PlanProblem {
    /// The bulk as `[{slot, error}, …]` for a tool response or the journal.
    pub(crate) fn json(problems: &[Self]) -> Value {
        serde_json::to_value(problems).unwrap_or_else(|_| json!([]))
    }

    /// The bulk as a Markdown bullet list for a notice.
    pub(crate) fn bullets(problems: &[Self]) -> String {
        problems
            .iter()
            .map(|p| format!("- {}: {}", p.slot, p.error))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

impl FirmRun {
    pub fn ended(&self) -> bool {
        matches!(
            *self.state.lock().unwrap(),
            RunState::Ended | RunState::Stopping
        )
    }

    pub fn worker(&self, name: &str) -> Result<Arc<WorkerRt>> {
        self.rt(name).ok_or_else(|| anyhow!("no worker {name}"))
    }

    /// A worker's runtime, if they are in the firm now.
    pub fn rt(&self, name: &str) -> Option<Arc<WorkerRt>> {
        self.workers.read().unwrap().get(name).cloned()
    }

    /// Every worker in the firm now.
    pub fn rts(&self) -> Vec<Arc<WorkerRt>> {
        self.workers.read().unwrap().values().cloned().collect()
    }

    /// A copy of the live chart.
    pub fn chart(&self) -> Firm {
        self.chart.read().unwrap().clone()
    }

    pub fn set_state(&self, worker: &WorkerRt, state: WorkerState, detail: Option<String>) {
        let mut cur = worker.state.lock().unwrap();
        if cur.0 == state && cur.1 == detail {
            return;
        }
        *cur = (state, detail.clone());
        drop(cur);
        self.journal.append(
            "worker",
            json!({ "worker": worker.name, "state": state, "detail": detail }),
        );
    }

    /// Who below `who` is still busy: in a turn, queued for one, booting,
    /// or with mail to read.
    pub fn team_busy(&self, who: &str) -> Vec<String> {
        let chart = self.chart();
        self.rts()
            .iter()
            .filter(|w| w.name != who && chart.is_above(who, &w.name))
            .filter_map(|w| {
                let st = w.state();
                let mail = w.queue.lock().unwrap().len();
                if matches!(
                    st,
                    WorkerState::Working
                        | WorkerState::Reviewing
                        | WorkerState::Queued
                        | WorkerState::Booting
                ) {
                    let name = serde_json::to_value(st)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_string))
                        .unwrap_or_default();
                    Some(format!("{} ({name})", w.name))
                } else if mail > 0 {
                    Some(format!("{} ({mail} unread)", w.name))
                } else {
                    None
                }
            })
            .collect()
    }

    /// Tells whoever waits to close the board's objective that their team
    /// is idle now. Called by the run's ticker.
    fn tell_close_waiter(&self) {
        let Some(who) = self.close_waiter.lock().unwrap().clone() else {
            return;
        };
        if self.rt(&who).is_none() {
            *self.close_waiter.lock().unwrap() = None;
            self.journal
                .append("objective", json!({ "event": "idle", "worker": who }));
            return;
        }
        if self.team_busy(&who).is_empty() {
            *self.close_waiter.lock().unwrap() = None;
            self.journal
                .append("objective", json!({ "event": "idle", "worker": who }));
            self.notice(&who, "Your team is idle: close the board's objective", "[Automatic notice] Everyone below you is idle now: no turns running, no mail waiting. If the board's objective is done, close it now (mail_reply to the board's assignment with close: true); if not, keep it open and assign what's left.\n");
        }
    }

    /// Sends mail and hands it to the recipients' foremen.
    pub fn send(&self, d: Draft) -> Result<mail::Sent> {
        contracts::may_mail(
            self,
            &d.from,
            &d.to.iter().chain(&d.cc).cloned().collect::<Vec<_>>(),
        )?;
        // Closing the board's objective pauses the firm: only once everyone
        // below the closer is idle, not mid-turn or with mail to read.
        if !d.automatic && self.post.with(|po| po.would_close_objective(&d)) {
            let busy = self.team_busy(&d.from);
            if !busy.is_empty() {
                let fresh = self
                    .close_waiter
                    .lock()
                    .unwrap()
                    .replace(d.from.clone())
                    .is_none();
                if fresh {
                    self.journal.append(
                        "objective",
                        json!({ "event": "waiting", "worker": d.from, "busy": busy }),
                    );
                }
                anyhow::bail!("the board's objective can't close while your team is still busy: {}. The firm pauses when it closes and their work would stop half done. End your turn: the post office tells you as soon as they are all idle, and then you close", busy.join(", "));
            }
        }
        let closing = d.close;
        let sent = self.post.send(&self.journal, d, now())?;
        self.deliver(&sent);
        if closing {
            self.on_plan_child_closed(&sent.thread);
        }
        if sent.objective_done {
            // The board's task is delivered: the simulation holds until the
            // board decides what comes next.
            let _ = pause_for(self, true, Some("the CEO completed the board's objective"));
            self.notice(BOARD, "Objective complete: the firm is paused", "[Automatic notice] The CEO delivered what you asked for, so the firm is paused: no worker starts a turn until you resume it (`zapper firm resume`, or the UI), and workers finish the turn they are in. Read the report, then resume or stop the firm.\n");
        }
        Ok(sent)
    }

    pub fn deliver(&self, sent: &mail::Sent) {
        for r in &sent.recipients {
            if let Some(w) = self.rt(r) {
                w.queue.lock().unwrap().push(sent.id.clone());
                w.wake.notify_one();
            }
        }
    }

    /// Release a plan stage, best-effort: send every child it can staff from
    /// the plan's owner, parented under the plan's source task, and collect the
    /// ones it can't as problems rather than stopping at the first. Each child's
    /// slot is resolved to a worker (bound on first use): a worker already on
    /// the chart is mailed the child as an ordinary assignment, so authority is
    /// enforced in `send` as for any delegation; a role slot with no free worker
    /// and a budget hires one, whose first brief is the child itself; a role
    /// slot with no free worker and no budget is left pending and reported.
    /// Returns the stage's sent threads (this release and any before) and the
    /// problems from this release. Idempotent to re-run: already-sent children
    /// aren't pending, so a retry only revisits the ones still missing.
    pub(crate) fn release_stage(
        &self,
        plan: &str,
        stage: usize,
    ) -> (Vec<String>, Vec<PlanProblem>) {
        let (owner, source) = match self.plans.lock().unwrap().get(plan) {
            Some(p) => (p.owner.clone(), p.source.clone()),
            None => return (Vec::new(), Vec::new()),
        };
        let mut problems = Vec::new();
        // Collect the pending children first, releasing the `plans` lock before
        // the loop: `dispatch_slot`, `send` and `sent` all re-lock it, and the
        // std mutex is not reentrant. (A lock held in a `for` iterator
        // expression lives for the whole loop body.)
        let pending = self.plans.lock().unwrap().pending(plan, stage);
        for (idx, spec) in pending {
            let sent = match self.dispatch_slot(plan, &spec, &owner, &source) {
                Ok(Dispatch::Mail(worker)) => {
                    let mut d = Draft::new(
                        &owner,
                        vec![worker],
                        &spec.subject,
                        &spec.body,
                        Kind::Assignment,
                    );
                    d.due = spec.due;
                    d.context = Some(source.clone());
                    self.send(d).map(|s| s.thread)
                }
                Ok(Dispatch::Briefed(thread)) => Ok(thread),
                Err(e) => Err(e),
            };
            match sent {
                Ok(thread) => self.plans.lock().unwrap().sent(plan, stage, idx, &thread),
                Err(e) => problems.push(PlanProblem {
                    slot: spec.slot,
                    error: format!("{e:#}"),
                }),
            }
        }
        let threads = self.plans.lock().unwrap().stage_threads(plan, stage);
        (threads, problems)
    }

    /// Resolve a plan slot to a worker and say how its child reaches them,
    /// binding the slot on first use (reused after, for continuity): an
    /// existing worker for a name, a free worker of a role (distinct from the
    /// plan's other bindings, for separation of duties), or — when no worker of
    /// the role is free — a worker hired for the slot and funded by its budget,
    /// whose first brief is this very child. A role slot with no free worker
    /// and no budget can't be staffed, so the stage stops and says why.
    fn dispatch_slot(
        &self,
        plan: &str,
        spec: &plan::ChildSpec,
        owner: &str,
        source: &str,
    ) -> Result<Dispatch> {
        let slot = &spec.slot;
        if let Some(w) = self.plans.lock().unwrap().binding(plan, slot) {
            return Ok(Dispatch::Mail(w));
        }
        let def = self
            .plans
            .lock()
            .unwrap()
            .slot_def(plan, slot)
            .ok_or_else(|| anyhow!("plan {plan}: no slot {slot}"))?;
        if let Some(name) = def.name {
            if self.chart().worker(&name).is_none() {
                bail!("slot {slot}: no worker named {name}");
            }
            self.plans.lock().unwrap().bind_slot(plan, slot, &name);
            return Ok(Dispatch::Mail(name));
        }
        let Some(role) = def.role else {
            bail!("slot {slot}: give a role or a name");
        };
        let bound = self.plans.lock().unwrap().bound_workers(plan);
        if let Some(name) = self
            .chart()
            .workers
            .iter()
            .find(|w| w.role == role && !bound.contains(&w.name))
            .map(|w| w.name.clone())
        {
            self.plans.lock().unwrap().bind_slot(plan, slot, &name);
            return Ok(Dispatch::Mail(name));
        }
        // No worker of the role is free: hire one for the slot, funded by its
        // budget. Without a budget the plan can't staff the role, so it stops
        // here and says so rather than silently leaving the child unsent.
        let Some(budget) = def.budget else {
            bail!("slot {slot}: no free worker of role {role} to staff it, and the slot has no budget to hire one; give the slot a budget, or name a worker, or hire one first");
        };
        let (worker, thread) = self.provision_slot(plan, spec, owner, source, &role, budget)?;
        self.plans.lock().unwrap().bind_slot(plan, slot, &worker);
        Ok(Dispatch::Briefed(thread))
    }

    /// Hire a worker for a role slot the chart can't staff, funded by the
    /// slot's budget, with the child as their first brief (parented under the
    /// plan's source task, so the plan tracks it). Returns the worker and the
    /// brief's thread. The hire itself enforces authority, the officer-first
    /// rule, the budget and the background check — this is just the plan's way
    /// in. Needs the live core and this run's `Arc` (a plan hire goes through
    /// `workforce::hire`), reachable while the firm runs.
    fn provision_slot(
        &self,
        plan: &str,
        spec: &plan::ChildSpec,
        owner: &str,
        source: &str,
        role: &str,
        budget: i64,
    ) -> Result<(String, String)> {
        let slot = &spec.slot;
        let shutting = || anyhow!("the firm is shutting down");
        let core = self.core.upgrade().ok_or_else(shutting)?;
        let run = self.me.upgrade().ok_or_else(shutting)?;
        let brief = if spec.body.trim().is_empty() {
            spec.subject.clone()
        } else {
            spec.body.clone()
        };
        let hired = workforce::hire(
            &core,
            &run,
            owner,
            workforce::HireArgs {
                role: role.to_string(),
                name: None,
                title: None,
                budget,
                subject: spec.subject.clone(),
                brief,
                reason: format!("Staffing plan {plan}'s {slot} slot as a {role}."),
                due: spec.due,
                attachments: Vec::new(),
                context: Some(source.to_string()),
            },
        )?;
        let field = |k: &str| {
            hired[k]
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| anyhow!("hire for slot {slot} returned no {k}"))
        };
        Ok((field("worker")?, field("thread")?))
    }

    /// A closing reply landed on `thread`; if it is a plan child, advance the
    /// plan, releasing the next stage when the current one is complete.
    pub(crate) fn on_plan_child_closed(&self, thread: &str) {
        let advance = self.plans.lock().unwrap().complete(thread);
        self.apply_plan_advance(advance);
    }

    /// A worker left the firm (`let_go`): hand any plan they owned to `manager`
    /// (so a long-running plan survives its planner leaving), free their slots
    /// and reopen the in-flight children, then tell each affected plan's (new)
    /// owner what changed and to resume. Their budget returns to the manager in
    /// `let_go` as for any departure; here we only mend the plans.
    pub(crate) fn plans_release_worker(&self, worker: &str, manager: &str) {
        // Bind the changes before the loop so the `plans` lock is released: the
        // body re-locks it (and the std mutex isn't reentrant).
        let changes = self.plans.lock().unwrap().release_worker(worker, manager);
        for change in changes {
            let plan = &change.plan;
            let (owner, stage) = {
                let plans = self.plans.lock().unwrap();
                (
                    plans.owner_of(plan).map(str::to_string),
                    plans.active_stage(plan),
                )
            };
            let (Some(owner), Some(stage)) = (owner, stage) else {
                continue;
            };
            self.journal.append(
                "plan",
                json!({ "event": "departure", "plan": plan, "stage": stage, "worker": worker,
                        "reassigned": change.reassigned, "reopened": change.reopened, "owner": owner }),
            );
            // One notice to the (new) owner, naming whichever of the two things
            // the departure did to the plan.
            let mut body = format!("[Automatic notice] {worker} left the firm. ");
            if change.reassigned {
                body += &format!("You now own plan {plan} — it was theirs. ");
            }
            if change.reopened {
                body += &format!("A slot they were staffing in plan {plan} is free again and its child is pending. ");
            }
            body += "Resume the plan (plan_resume) once the role can be staffed — a free or newly hired worker picks it up — or pause and close it.\n";
            self.notice(
                &owner,
                &format!("Plan {plan} needs you ({worker} left)"),
                &body,
            );
        }
    }

    /// Act on a plan state change: release the next stage (sending what it can)
    /// or record completion. A stage that couldn't staff some of its children
    /// is left holding them and the plan's owner is told, in bulk — the planner
    /// isn't in this path (a child's close woke it), so no one else would see.
    pub(crate) fn apply_plan_advance(&self, advance: plan::Advance) {
        match advance {
            plan::Advance::Release { plan, stage } => {
                let (_, problems) = self.release_stage(&plan, stage);
                if !problems.is_empty() {
                    self.plan_stuck_notice(&plan, stage, &problems);
                }
            }
            plan::Advance::Completed { plan } => {
                self.journal
                    .append("plan", json!({ "event": "completed", "plan": plan }));
            }
            plan::Advance::None => {}
        }
    }

    /// Journal a stage's unstaffed children and tell the plan's owner, in bulk,
    /// what to fix. The stage holds those children until a retry (the owner's
    /// next `plan_resume`) sends them.
    fn plan_stuck_notice(&self, plan: &str, stage: usize, problems: &[PlanProblem]) {
        self.journal.append(
            "plan",
            json!({ "event": "stage_stuck", "plan": plan, "stage": stage,
                    "problems": PlanProblem::json(problems) }),
        );
        let owner = self
            .plans
            .lock()
            .unwrap()
            .owner_of(plan)
            .map(str::to_string);
        if let Some(owner) = owner {
            self.notice(
                &owner,
                &format!("Plan {plan}: some of stage {stage} could not start"),
                &format!("[Automatic notice] Your plan {plan} sent the children it could, but these could not be staffed:\n\n{}\n\nFix each (give the slot a budget, or free, name or hire a worker) and resume the plan (plan_resume) to send them; or pause and close the plan.\n", PlanProblem::bullets(problems)),
            );
        }
    }

    /// A notice from the post office.
    pub fn notice(&self, to: &str, subject: &str, body: &str) {
        let d = Draft::new(mail::OFFICE, vec![to.to_string()], subject, body, Kind::Fyi);
        // The post office's lock ends with this statement, before the
        // workers' queues are touched.
        let sent = self.post.send_notice(&self.journal, d, now());
        match sent {
            Ok(sent) => self.deliver(&sent),
            Err(e) => tracing::warn!(firm = %self.firm, "notice to {to}: {e:#}"),
        }
    }

    /// Whether `worker` may start a turn under the budget; tells the board
    /// once a day when it may not.
    pub fn budget_allows(&self, worker: &str) -> bool {
        if !workforce::allows(self, worker) {
            return false;
        }
        let b = &self.config.budget;
        let mut u = self.usage.lock().unwrap();
        let today = utc_day(now());
        if u.day != today {
            *u = Usage {
                day: today,
                ..Default::default()
            };
        }
        let worker_turns = u.worker_turns.get(worker).copied().unwrap_or(0);
        let firm_over = (b.daily_turns > 0 && u.turns >= b.daily_turns)
            || (b.daily_tokens > 0 && u.tokens >= b.daily_tokens);
        let worker_over = b.worker_daily_turns > 0 && worker_turns >= b.worker_daily_turns;
        if !(firm_over || worker_over) {
            return true;
        }
        if firm_over && !u.notified {
            u.notified = true;
            let body = format!(
                "[Automatic notice] The firm reached its budget: {} of {} turns and {} tokens today. Workers sleep until the reset at midnight UTC, with their mail kept. Raise the budget in the config (firms.{}.budget) or wait.\n",
                u.turns,
                b.daily_turns,
                u.tokens,
                self.firm
            );
            drop(u);
            self.journal.append("budget", json!({ "over": true }));
            self.notice(BOARD, "Budget reached", &body);
        }
        false
    }

    pub fn count_turn(&self, worker: &str, tokens: u64) {
        self.ledger.spend(worker, tokens);
        let mut u = self.usage.lock().unwrap();
        let today = utc_day(now());
        if u.day != today {
            *u = Usage {
                day: today,
                ..Default::default()
            };
        }
        u.turns += 1;
        u.tokens += tokens;
        *u.worker_turns.entry(worker.to_string()).or_default() += 1;
    }

    pub fn status(&self) -> Value {
        let ledger = self.ledger.status();
        let workers: Vec<Value> = self
            .rts()
            .iter()
            .map(|w| {
                let (state, detail) = w.state.lock().unwrap().clone();
                let (desk, unread) = self
                    .post
                    .with(|po| (po.desk(&w.name), po.unread(&w.name).len()));
                json!({
                    "name": w.name, "role": w.role, "title": w.title, "officer": w.officer(),
                    "state": state, "detail": detail,
                    "session": w.session().map(|s| s.id.clone()),
                    "unread": unread,
                    "owed": desk.owed.len(), "waiting": desk.waiting.len(),
                    "queued": w.queue.lock().unwrap().len(),
                })
            })
            .collect();
        let chart = chart(&self.firm, &self.chart(), &self.models, &self.agents);
        let u = self.usage.lock().unwrap();
        json!({
            "firm": self.firm, "company": self.company, "run": self.id, "state": *self.state.lock().unwrap(), "started": self.started,
            "domain": self.domain, "repos": self.config.git_repos(), "workers": workers, "chart": chart, "funds": ledger,
            "usage": { "day": u.day, "turns": u.turns, "tokens": u.tokens, "worker_turns": u.worker_turns },
            "budget": self.config.budget,
            "board_unread": self.post.with(|po| po.unread(BOARD).len()),
            "facilities": self.facility.status(),
            "contracts": contracts::list(self),
            "scenario": self.scenario.as_ref().map(|p| p.status()),
        })
    }
}

/// The office's API routes (firms, mail, security, the drive), for the API
/// and the UI to serve.
pub fn routes(d: &Core) -> axum::Router {
    http::read_routes()
        .merge(http::board_routes())
        .with_state(d.clone())
}

/// The office's page: the UI's front page.
pub const PAGE: &str = include_str!("office.html");

/// The page's stylesheet and scripts, in the order it loads them: served
/// beside it under `/mount/`. The scripts share one scope, as one script.
pub const ASSETS: &[(&str, &str)] = &[
    ("office.css", include_str!("ui/office.css")),
    ("common.js", include_str!("ui/common.js")),
    ("model.js", include_str!("ui/model.js")),
    ("org.js", include_str!("ui/org.js")),
    ("compose.js", include_str!("ui/compose.js")),
    ("transcripts.js", include_str!("ui/transcripts.js")),
    ("markdown.js", include_str!("ui/markdown.js")),
    ("messages.js", include_str!("ui/messages.js")),
    ("security.js", include_str!("ui/security.js")),
    ("vms.js", include_str!("ui/vms.js")),
    ("drive.js", include_str!("ui/drive.js")),
    ("git.js", include_str!("ui/git.js")),
    ("kickoff.js", include_str!("ui/kickoff.js")),
    ("runs.js", include_str!("ui/runs.js")),
    ("work.js", include_str!("ui/work.js")),
    ("stats.js", include_str!("ui/stats.js")),
    ("header.js", include_str!("ui/header.js")),
    ("main.js", include_str!("ui/main.js")),
];

/// The office in a starting daemon: runs a previous daemon left running are
/// ended; the firms' role layers and their officers' standing approvals are
/// rules the office keeps; the daemon stopping stops the firms.
pub fn start(d: &Core) {
    d.add_config_check(firm::check);
    close_orphans(d.state_dir());
    d.add_rule_keeper(Arc::new(Keeper(d.office())));
    d.on_shutdown(|d| {
        Box::pin(async move {
            let runs: Vec<_> = d.office().runs.read().unwrap().values().cloned().collect();
            for r in runs {
                down(&d, &r, "daemon shutdown").await;
            }
        })
    });
}

/// The firms' role layers, and their officers' standing approvals, which
/// the operator removes through the officers' records.
struct Keeper(Arc<Office>);

impl zapper_core::RuleKeeper for Keeper {
    fn layers(&self) -> Vec<(zapper_policy::Scope, Policy)> {
        let mut layers = Vec::new();
        for run in self.0.runs.read().unwrap().values() {
            for (role, p) in &run.roles {
                layers.push((
                    zapper_policy::Scope::Role(format!("{}/{role}", run.firm)),
                    p.read().unwrap().clone(),
                ));
            }
        }
        layers
    }

    fn remove(&self, id: &str) -> Option<Result<()>> {
        let runs: Vec<_> = self.0.runs.read().unwrap().values().cloned().collect();
        let run = runs
            .into_iter()
            .find(|r| r.security.lock().unwrap().standing.contains_key(id))?;
        Some(security::revoke(&run, BOARD, id, "removed by the operator").map(|_| ()))
    }
}

/// A core without VMs with the office started on it, for tests.
#[cfg(test)]
pub(crate) fn test_core(yaml: &str) -> zapper_core::testing::TestCore {
    let t = zapper_core::testing::TestCore::new(yaml);
    start(&t.core);
    t
}

#[cfg(test)]
mod approval_tests;

#[cfg(test)]
mod names_tests;

#[cfg(test)]
mod run_tests;

#[cfg(test)]
mod http_tests;

#[cfg(test)]
mod foreman_tests;

#[cfg(test)]
mod tools_tests;

#[cfg(test)]
mod start_tests;

#[cfg(test)]
mod board_tests;

#[cfg(test)]
mod tests {
    use super::{OfficeOf, PlanProblem};

    #[test]
    fn plan_problems_render_as_json_and_bullets() {
        let problems = [
            PlanProblem {
                slot: "second".into(),
                error: "no budget to hire".into(),
            },
            PlanProblem {
                slot: "third".into(),
                error: "no worker named kim".into(),
            },
        ];
        assert_eq!(
            PlanProblem::json(&problems),
            serde_json::json!([
                { "slot": "second", "error": "no budget to hire" },
                { "slot": "third", "error": "no worker named kim" },
            ])
        );
        assert_eq!(
            PlanProblem::bullets(&problems),
            "- second: no budget to hire\n- third: no worker named kim"
        );
        // Nothing to report renders as an empty array / empty string.
        assert_eq!(PlanProblem::json(&[]), serde_json::json!([]));
        assert_eq!(PlanProblem::bullets(&[]), "");
    }

    #[test]
    fn a_session_finds_no_run_its_firm_doesnt_have() {
        let t = super::test_core("profiles:\n  w: {}\n");
        let mut new = zapper_core::NewSession::plain("w".into(), t.workdir(), Default::default());
        new.labels = super::labels("acme", "r1", "pat", true);
        let worker = t.core.test_session(new, true).unwrap();
        let plain = t
            .core
            .test_session(
                zapper_core::NewSession::plain("w".into(), t.workdir(), Default::default()),
                true,
            )
            .unwrap();
        // No run of acme in the registry, and a session with no firm.
        assert!(super::run_of(&t.core.office(), &worker).is_none());
        assert!(super::run_of(&t.core.office(), &plain).is_none());
    }

    #[test]
    fn a_firms_sessions_are_labelled_with_the_run() {
        let w = super::labels("acme", "20261022-101500-k3z", "pat", true);
        let v: Vec<(&str, &str)> = w.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        assert_eq!(
            v,
            [
                ("firm", "acme"),
                ("member", "pat"),
                ("office", "worker"),
                ("run", "20261022-101500-k3z")
            ]
        );
        let f = super::labels("acme", "r", "cluster", false);
        assert_eq!(f["office"], "facility");
    }

    #[test]
    fn run_ids() {
        let id = super::run_id();
        assert!(super::is_run_id(&id), "{id}");
        assert!(!super::is_run_id("../../etc"));
        assert!(!super::is_run_id("20261016-140501-K3Z"));
    }

    /// The page loads exactly the assets there are, in their order.
    #[test]
    fn the_page_loads_its_assets() {
        let refs: Vec<&str> = super::PAGE
            .split("\"/mount/")
            .skip(1)
            .map(|s| s.split('"').next().unwrap())
            .collect();
        let names: Vec<&str> = super::ASSETS.iter().map(|(n, _)| *n).collect();
        assert_eq!(refs, names);
        for (name, body) in super::ASSETS {
            if name.ends_with(".js") {
                assert!(body.starts_with("'use strict';"), "{name}");
            }
        }
    }
}
