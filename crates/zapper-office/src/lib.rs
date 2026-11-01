//! The office: firms of workers, each a Claude Code in its own session, who
//! talk by mail, publish to a shared drive, and whose network access security
//! officers decide. `docs/office.md` is the design.
//!
//! A run starts fresh with every `firm up` and is written to
//! `state/firms/<firm>/runs/<id>/` as it happens; the daemon keeps indexes.

pub(crate) mod access;
pub mod client;
pub(crate) mod contracts;
pub(crate) mod drive;
pub(crate) mod facility;
pub mod firm;
pub(crate) mod foreman;
pub(crate) mod git;
pub(crate) mod http;
pub(crate) mod journal;
mod kb;
pub(crate) mod mail;
pub(crate) mod ops;
pub(crate) mod plan;
pub(crate) mod product;
pub(crate) mod prompts;
mod remote;
pub(crate) mod scenario;
pub(crate) mod security;
pub mod skills;
pub(crate) mod workforce;

mod history;
mod run;

pub(crate) use history::{close_orphans, list_runs, prune, run_config, run_dir, set_note};
#[cfg(test)]
pub(crate) use run::test_run;
#[cfg(test)]
pub(crate) use run::test_run_remote;
pub(crate) use run::{
    board_send, default_company, down, effective_model, pause, up_with, Agents, StartOptions,
};
pub(crate) use run::{chart, pause_for, worker_json};

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use zapper_core::prelude::*;

use crate::firm::{Firm, BOARD};
use anyhow::{anyhow, bail, Result};
use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::{Notify, Semaphore};

use zapper_core::util::{now, random_id};
use zapper_core::{Core, EventBody};

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
    /// The daemon through the office's client interface, set once in `start`.
    /// In-process it wraps `Core`; once the office is a process of its own it
    /// is the socket client. How the office reaches the daemon from places
    /// that have no `run` in hand (run creation, the read endpoints), so they
    /// need no `&Core`.
    client: std::sync::OnceLock<Arc<dyn client::OfficeClient>>,
}

impl Office {
    /// The office's client of the daemon (set in `start`).
    pub fn client(&self) -> &Arc<dyn client::OfficeClient> {
        self.client
            .get()
            .expect("the office's client is set in start()")
    }

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

    /// What the foreman drives: the worker's session id and the harness kind
    /// its agent runs, once its VM is up. The foreman goes through this (and
    /// `run.client.attach`) rather than the `Arc<Session>`, so it holds no
    /// session handle and reaches the guest over the client — the same path
    /// in-process and, in time, over the socket.
    ///
    /// Derived from `session` while that `Arc<Session>` is still held for the
    /// facility, security and drive readers; those move onto the client in
    /// P2-b, after which this becomes stored state and the `Arc` goes.
    pub fn driven(&self) -> Option<(String, String)> {
        self.session
            .lock()
            .unwrap()
            .as_ref()
            .map(|s| (s.id.clone(), s.resolved.agent.kind.clone()))
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
    pub firm: String,
    pub id: String,
    pub dir: PathBuf,
    /// The daemon's `state/services/` directory, where the office binds the
    /// sockets of the facilities it serves (its tools, the officers' decider).
    /// Plain office config (P1-a): the office and daemon are configured to the
    /// same path, assumed aligned — no coordination over the client. In-process
    /// it is `d.socket_dir()`; a co-host office reads it from `office.yaml`.
    pub socket_dir: PathBuf,
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
    /// The daemon, through the office's client interface: how the office's
    /// firm logic changes role layers (and, in time, everything else) so it
    /// does not reach into `Core` directly. In-process now (`client::Local`);
    /// over the API socket once the office is a process of its own.
    pub client: Arc<dyn crate::client::OfficeClient>,
    pub turns: Arc<Semaphore>,
    pub usage: Mutex<Usage>,
    pub facility: facility::Facilities,
    /// The office's tools: their registration in the gateway and the socket
    /// that serves them, undone when the guard drops.
    pub tools: Mutex<Option<client::FacilityGuard>>,
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
    role: &str,
    worker: bool,
) -> BTreeMap<String, String> {
    let mut m: BTreeMap<String, String> = [
        ("firm", firm),
        ("run", run),
        ("member", member),
        ("office", if worker { "worker" } else { "facility" }),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    // A worker of a role attaches that role's shared layer by this label; its
    // value is the layer's name (`Core::named_layer`).
    if !role.is_empty() {
        m.insert("firm-role".to_string(), format!("{firm}/{role}"));
    }
    m
}

/// The run a session belongs to, from its labels: the firm's run, if it is
/// the one that started the session.
#[cfg(test)]
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

    /// Send a plan stage's children as assignments from the plan's owner,
    /// parented under the plan's source task; return their thread ids in spec
    /// order. Each child's slot is resolved to a worker (bound on first use);
    /// each send is an ordinary assignment, so authority is enforced in `send`
    /// as for any delegation.
    pub(crate) fn plan_send_stage(
        &self,
        plan: &str,
        specs: &[plan::ChildSpec],
    ) -> Result<Vec<String>> {
        let (owner, source) = {
            let plans = self.plans.lock().unwrap();
            match plans.get(plan) {
                Some(p) => (p.owner.clone(), p.source.clone()),
                None => return Ok(Vec::new()),
            }
        };
        let mut threads = Vec::with_capacity(specs.len());
        for s in specs {
            let worker = self.resolve_slot(plan, &s.slot)?;
            let mut d = Draft::new(&owner, vec![worker], &s.subject, &s.body, Kind::Assignment);
            d.due = s.due;
            d.context = Some(source.clone());
            threads.push(self.send(d)?.thread);
        }
        Ok(threads)
    }

    /// Resolve a plan slot to a worker, binding it on first use (reused after,
    /// for continuity): an existing worker for a name, or a free worker of a
    /// role, distinct from the plan's other bindings (separation of duties).
    /// Auto-hiring an unstaffed role is not yet wired (it needs the async
    /// provisioning path) — name a worker or hire one first.
    fn resolve_slot(&self, plan: &str, slot: &str) -> Result<String> {
        if let Some(w) = self.plans.lock().unwrap().binding(plan, slot) {
            return Ok(w);
        }
        let def = self
            .plans
            .lock()
            .unwrap()
            .slot_def(plan, slot)
            .ok_or_else(|| anyhow!("plan {plan}: no slot {slot}"))?;
        let chart = self.chart();
        let worker = if let Some(name) = def.name {
            if chart.worker(&name).is_none() {
                bail!("slot {slot}: no worker named {name}");
            }
            name
        } else if let Some(role) = def.role {
            let bound = self.plans.lock().unwrap().bound_workers(plan);
            chart
                .workers
                .iter()
                .find(|w| w.role == role && !bound.contains(&w.name))
                .map(|w| w.name.clone())
                .ok_or_else(|| anyhow!("slot {slot}: no free worker of role {role} to staff it (auto-hiring an absent role is not yet wired; name a worker or hire one first)"))?
        } else {
            bail!("slot {slot}: give a role or a name");
        };
        self.plans.lock().unwrap().bind_slot(plan, slot, &worker);
        Ok(worker)
    }

    /// A closing reply landed on `thread`; if it is a plan child, advance the
    /// plan, releasing the next stage when the current one is complete.
    pub(crate) fn on_plan_child_closed(&self, thread: &str) {
        let advance = self.plans.lock().unwrap().complete(thread);
        self.apply_plan_advance(advance);
    }

    /// Act on a plan state change: release the next stage (send and bind its
    /// children) or record completion.
    pub(crate) fn apply_plan_advance(&self, advance: plan::Advance) {
        match advance {
            plan::Advance::Release { plan, stage, specs } => {
                match self.plan_send_stage(&plan, &specs) {
                    Ok(threads) => self.plans.lock().unwrap().bind(&plan, stage, threads),
                    Err(e) => {
                        self.journal.append(
                            "plan",
                            json!({ "event": "release_failed", "plan": plan, "stage": stage, "error": format!("{e:#}") }),
                        );
                    }
                }
            }
            plan::Advance::Completed { plan } => {
                self.journal
                    .append("plan", json!({ "event": "completed", "plan": plan }));
            }
            plan::Advance::None => {}
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
        // Overspend goes up at once; the CEO, if it reached them, hears how
        // near their reserve they are.
        let took = workforce::settle_overspend(self, worker);
        workforce::after_turn(self, worker);
        if let Some(ceo) = self
            .chart()
            .root()
            .filter(|c| *c != worker && took.iter().any(|t| t == c))
        {
            workforce::after_turn(self, ceo);
        }
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
            "domain": self.domain, "repos": self.config.git_resources().iter().map(|r| r.trim_start_matches("git:")).collect::<Vec<_>>(), "workers": workers, "chart": chart, "funds": ledger,
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
    let _ = d.office().client.set(Arc::new(client::Local(d.clone())));
    d.add_config_check(firm::check);
    close_orphans(d.state_dir());
    follow_rules(d);
    d.on_shutdown(|d| {
        Box::pin(async move {
            let runs: Vec<_> = d.office().runs.read().unwrap().values().cloned().collect();
            for r in runs {
                down(&d, &r, "daemon shutdown").await;
            }
        })
    });
}

/// Follows the core's `rule` events: when a role's standing approval is
/// removed from its layer through the core — the operator, or `zapper`
/// without the office — the office drops its own record of it, so the two
/// agree. This is how the office keeps up once it is a process of its own;
/// in-process it follows the same stream. The office's own revocations keep
/// their records directly, so a `removed` event for a rule it no longer
/// tracks is a no-op.
fn follow_rules(d: &Core) {
    let Ok(h) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let mut events = d.subscribe();
    let office = d.office();
    h.spawn(async move {
        use tokio::sync::broadcast::error::RecvError;
        loop {
            match events.recv().await {
                Ok(e) => {
                    let EventBody::Rule {
                        state, id, scope, ..
                    } = &e.body
                    else {
                        continue;
                    };
                    if state != "removed" {
                        continue;
                    }
                    let Some(firm) = scope
                        .strip_prefix("role:")
                        .and_then(|r| r.split('/').next())
                    else {
                        continue;
                    };
                    let run = office.runs.read().unwrap().get(firm).cloned();
                    if let Some(run) = run {
                        security::forget_standing(&run, id);
                    }
                }
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => break,
            }
        }
    });
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
mod remote_tests;

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
    use super::OfficeOf;

    #[test]
    fn a_session_finds_no_run_its_firm_doesnt_have() {
        let t = super::test_core("profiles:\n  w: {}\n");
        let mut new = zapper_core::NewSession::plain("w".into(), t.workdir(), Default::default());
        new.labels = super::labels("acme", "r1", "pat", "", true);
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
        let w = super::labels("acme", "20261022-101500-k3z", "pat", "eng", true);
        let v: Vec<(&str, &str)> = w.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        assert_eq!(
            v,
            [
                ("firm", "acme"),
                ("firm-role", "acme/eng"),
                ("member", "pat"),
                ("office", "worker"),
                ("run", "20261022-101500-k3z")
            ]
        );
        let f = super::labels("acme", "r", "cluster", "", false);
        assert_eq!(f["office"], "facility");
        assert!(!f.contains_key("firm-role"), "a facility has no role");
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
