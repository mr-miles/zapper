//! The office: firms of workers, each a Claude Code in its own session, who
//! talk by mail, publish to a shared drive, and whose network access security
//! officers decide. `docs/office.md` is the design.
//!
//! A run starts fresh with every `firm up` and is written to
//! `state/firms/<firm>/runs/<id>/` as it happens; the daemon keeps indexes.

pub mod access;
pub mod contracts;
pub mod drive;
pub mod facility;
pub mod foreman;
pub mod git;
pub mod http;
pub mod journal;
pub mod mail;
pub mod ops;
pub mod product;
pub mod prompts;
pub mod scenario;
pub mod security;
pub mod skills;
pub mod workforce;

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::{Notify, Semaphore};
use zapper_policy::proto::GuestFile;
use zapper_policy::{Config, Firm, Policy, BOARD};

use zapper_core::util::{now, random_id};
use zapper_core::Core;

/// The office's state in the core: its registry of runs.
pub trait OfficeOf {
    fn office(&self) -> Arc<Office>;
}

impl OfficeOf for Core {
    fn office(&self) -> Arc<Office> {
        self.client()
    }
}
use journal::Journal;
use mail::{Draft, Kind, PostOffice};
use zapper_core::{NewSession, Session, Status};

/// Every firm's current or last run.
#[derive(Default)]
pub struct Office {
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
pub enum RunState {
    Starting,
    Running,
    Paused,
    Stopping,
    Ended,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkerState {
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
pub struct WorkerRt {
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
    pub fn new(firm: &Firm, w: &zapper_policy::Worker) -> WorkerRt {
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
pub struct Usage {
    pub day: i64,
    pub turns: u32,
    pub tokens: u64,
    pub worker_turns: HashMap<String, u32>,
    pub notified: bool,
}

pub struct FirmRun {
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
    pub post: Mutex<PostOffice>,
    workers: RwLock<BTreeMap<String, Arc<WorkerRt>>>,
    /// The run's budget: the board's grants, balances, budget requests.
    pub ledger: Mutex<workforce::Ledger>,
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
    pub stopping: Notify,
}

/// The labels of a session that belongs to a firm: the firm, its run, the
/// member (a worker's or facility's name) and which of the two it is.
pub fn labels(firm: &str, run: &str, member: &str, worker: bool) -> BTreeMap<String, String> {
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
pub fn run_of(office: &Office, s: &Session) -> Option<Arc<FirmRun>> {
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
pub fn is_run_id(s: &str) -> bool {
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

pub fn firm_dir(d: &Core, firm: &str) -> PathBuf {
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
        if !d.automatic && self.post.lock().unwrap().would_close_objective(&d) {
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
        let sent = self.post.lock().unwrap().send(&self.journal, d, now())?;
        self.deliver(&sent);
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

    /// A notice from the post office.
    pub fn notice(&self, to: &str, subject: &str, body: &str) {
        let d = Draft::new(mail::OFFICE, vec![to.to_string()], subject, body, Kind::Fyi);
        // The post office's lock ends with this statement, before the
        // workers' queues are touched.
        let sent = self
            .post
            .lock()
            .unwrap()
            .send_notice(&self.journal, d, now());
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
        self.ledger.lock().unwrap().spend(worker, tokens);
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
        let ledger = self.ledger.lock().unwrap().status();
        let workers: Vec<Value> = self
            .rts()
            .iter()
            .map(|w| {
                let (state, detail) = w.state.lock().unwrap().clone();
                let po = self.post.lock().unwrap();
                let desk = po.desk(&w.name);
                json!({
                    "name": w.name, "role": w.role, "title": w.title, "officer": w.officer(),
                    "state": state, "detail": detail,
                    "session": w.session().map(|s| s.id.clone()),
                    "unread": po.unread(&w.name).len(),
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
            "board_unread": self.post.lock().unwrap().unread(BOARD).len(),
            "facilities": self.facility.status(),
            "contracts": contracts::list(self),
            "scenario": self.scenario.as_ref().map(|p| p.status()),
        })
    }
}

/// Each profile's agent kind and model, by profile name.
pub type Agents = BTreeMap<String, (String, Option<String>)>;

fn profile_agents(config: &Config) -> Agents {
    config
        .profiles
        .keys()
        .filter_map(|n| {
            let p = config.resolve(n).ok()?;
            Some((n.clone(), (p.agent.kind, p.agent.model)))
        })
        .collect()
}

/// The chart as the UI draws it: at the start of a run, and live.
fn chart(
    name: &str,
    firm: &Firm,
    models: &BTreeMap<String, Option<String>>,
    agents: &Agents,
) -> Value {
    let workers: Vec<Value> = firm
        .workers
        .iter()
        .map(|w| worker_json(firm, models, agents, w))
        .collect();
    json!({ "firm": name, "description": firm.description, "board": firm.board.name, "domain": firm.domain(name), "repos": firm.git_repos(), "workers": workers })
}

/// One worker of the chart, as the UI draws it.
pub(crate) fn worker_json(
    firm: &Firm,
    models: &BTreeMap<String, Option<String>>,
    agents: &Agents,
    w: &zapper_policy::Worker,
) -> Value {
    let role = firm.roles.get(&w.role);
    // On their own profile, its agent and model; else the role's.
    let profile = w
        .profile
        .clone()
        .or_else(|| role.map(|r| r.profile.clone()))
        .unwrap_or_default();
    let (agent, own_model) = agents.get(&profile).cloned().unwrap_or_default();
    let model = if w.profile.is_some() {
        own_model
    } else {
        models.get(&w.role).cloned().flatten()
    };
    json!({
        "name": w.name, "role": w.role, "title": firm.title_of(&w.name),
        "reports_to": w.reports_to, "officer": firm.is_officer(&w.name),
        "avatar": role.map(|r| json!(r.avatar)), "model": model,
        "agent": agent, "profile": profile,
    })
}

/// The model each role's workers run.
fn role_models(firm: &Firm, config: &Config) -> BTreeMap<String, Option<String>> {
    firm.roles
        .iter()
        .map(|(n, r)| (n.clone(), effective_model(r, config)))
        .collect()
}

/// The model a role's workers run: the role's, else its profile's.
pub fn effective_model(role: &zapper_policy::Role, config: &Config) -> Option<String> {
    role.model.clone().or_else(|| {
        config
            .resolve(&role.profile)
            .ok()
            .and_then(|p| p.agent.model)
    })
}

/// Memory still available on the host, in MiB.
pub(crate) fn mem_available_mib() -> Option<u64> {
    let info = std::fs::read_to_string("/proc/meminfo").ok()?;
    info.lines()
        .find_map(|l| {
            l.strip_prefix("MemAvailable:")?
                .trim()
                .strip_suffix("kB")?
                .trim()
                .parse::<u64>()
                .ok()
        })
        .map(|k| k / 1024)
}

/// Checks that the firm fits before booting anything.
fn check_resources(d: &Core, config: &Config, firm: &Firm) -> Result<()> {
    let mut mib = 0u64;
    let mut count = 0usize;
    for w in &firm.workers {
        let role = &firm.roles[&w.role];
        mib += config.resolve(&role.profile)?.engine.memory_mib as u64;
        count += 1;
    }
    for f in firm.facilities.values() {
        mib += config.resolve(&f.profile)?.engine.memory_mib as u64;
        count += 1;
    }
    let running = d
        .sessions()
        .iter()
        .filter(|s| !matches!(*s.status.borrow(), Status::Stopped(_) | Status::Failed(_)))
        .count();
    if running + count > config.daemon.max_sessions {
        bail!(
            "the firm needs {count} sessions and {running} are running; daemon.max_sessions is {}",
            config.daemon.max_sessions
        );
    }
    if let Some(avail) = mem_available_mib() {
        if mib > avail {
            bail!("the firm needs {mib} MiB of memory for its VMs and the host has {avail} MiB available");
        }
    }
    Ok(())
}

/// Starts a fresh run of a firm: boots its facilities, then its workers.
/// The board's parameters for one run; the config stays as it is.
#[derive(Debug, Default, Clone, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StartOptions {
    /// The company's name, shown in the UI and on the landing page.
    pub company: Option<String>,
    /// The CEO's name, instead of the config's.
    pub ceo: Option<String>,
    /// The seed budget, in tokens or text like `500k`.
    pub seed: Option<serde_json::Value>,
    /// Extra instructions for the CEO, in their preamble, taking
    /// precedence over their skill.
    pub ceo_prompt: Option<String>,
    /// The board's own articles for the charter: every worker reads them.
    pub articles: Option<String>,
    /// The charter's terms, instead of the config's.
    pub security_share: Option<f64>,
    pub max_concurrent: Option<u32>,
    pub daily_turns: Option<u32>,
    /// A scenario of the firm's config to play.
    pub scenario: Option<String>,
    /// Hire slots and script steps instead of the scenario's own.
    pub hires: Option<Vec<zapper_policy::HireSlot>>,
    pub script: Option<Vec<zapper_policy::Step>>,
    /// The board's task for the CEO, sent as the run's first assignment
    /// once it is up: its first line is the subject, the rest the message.
    pub task: Option<String>,
}

/// A task's subject (its first line) and message (the rest, or the subject
/// when there is no more).
fn task_parts(task: &str) -> Option<(String, String)> {
    let t = task.trim();
    if t.is_empty() {
        return None;
    }
    let (first, rest) = t.split_once('\n').unwrap_or((t, ""));
    let subject: String = first
        .trim()
        .trim_start_matches('#')
        .trim()
        .chars()
        .take(120)
        .collect();
    let body = if rest.trim().is_empty() {
        t
    } else {
        rest.trim()
    };
    Some((subject, body.to_string()))
}

/// A run's firm as the board started it.
#[derive(Debug)]
struct Started {
    firm: Firm,
    company: String,
    prompts: BTreeMap<String, String>,
    articles: String,
    scenario: Option<(String, zapper_policy::Scenario)>,
}

/// Longest extra prompt the board gives a founder.
const MAX_PROMPT: usize = 20_000;

/// Renames a founder for this run, everywhere the chart names them.
fn rename(firm: &mut Firm, old: &str, new: &str) -> Result<()> {
    if new == old {
        return Ok(());
    }
    if !zapper_policy::valid_worker_name(new) || zapper_policy::reserved_name(new) {
        bail!(
            "{new:?} can't be a name: lowercase letters, digits and dashes, starting with a letter"
        );
    }
    if firm.worker(new).is_some() {
        bail!("{new} is already in the firm");
    }
    for w in firm.workers.iter_mut() {
        if w.name == old {
            w.name = new.to_string();
        }
        if w.reports_to.as_deref() == Some(old) {
            w.reports_to = Some(new.to_string());
        }
    }
    for o in firm.security.officers.iter_mut() {
        if o == old {
            *o = new.to_string();
        }
    }
    firm.names.retain(|n| n != new);
    Ok(())
}

/// The company's name when the board gives none.
pub fn default_company(name: &str, firm: &Firm) -> String {
    firm.landing
        .as_ref()
        .map(|l| l.display_name(name))
        .unwrap_or_else(|| format!("{} Inc.", zapper_policy::title_case(name)))
}

/// The firm as this run starts it: the config's, with the board's
/// parameters applied and the result checked like the config.
fn with_options(name: &str, mut firm: Firm, config: &Config, o: &StartOptions) -> Result<Started> {
    let company = o
        .company
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(|c| c.chars().take(80).collect::<String>())
        .unwrap_or_else(|| default_company(name, &firm));
    if company.chars().any(char::is_control) {
        bail!("the company's name can't have control characters");
    }
    if let Some(l) = firm.landing.as_mut() {
        l.name = company.clone();
    }
    if let Some(ceo) = o
        .ceo
        .as_deref()
        .map(|c| c.trim().to_ascii_lowercase())
        .filter(|c| !c.is_empty())
    {
        let old = firm
            .root()
            .map(str::to_string)
            .ok_or_else(|| anyhow!("the firm has no CEO"))?;
        rename(&mut firm, &old, &ceo)?;
    }
    // A company starts with its CEO, who holds security until they hire an
    // officer; only a config's own founding officers (for tests) do it
    // otherwise.
    if firm.security.officers.is_empty() {
        let root = firm
            .root()
            .map(str::to_string)
            .ok_or_else(|| anyhow!("the firm has no CEO"))?;
        firm.security.officers = vec![root];
    }
    // The board's extra instructions for the CEO.
    let mut prompts = BTreeMap::new();
    if let Some(text) = o
        .ceo_prompt
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        if text.len() > MAX_PROMPT {
            bail!("keep the CEO's extra prompt under {MAX_PROMPT} bytes");
        }
        let root = firm.root().ok_or_else(|| anyhow!("the firm has no CEO"))?;
        prompts.insert(root.to_string(), text.to_string());
    }
    // The scenario: the config's preset with the board's changes to its
    // slots and steps, checked like the config.
    let scenario = match o.scenario.as_deref().filter(|n| !n.is_empty()) {
        Some(n) => {
            let mut sc = firm
                .scenarios
                .get(n)
                .cloned()
                .ok_or_else(|| anyhow!("no scenario {n} in firm {name}"))?;
            if let Some(h) = &o.hires {
                sc.hires = h.clone();
            }
            if let Some(x) = &o.script {
                sc.script = x.clone();
            }
            let mut problems = sc.problems(&firm);
            problems.extend(sc.profile_problems(config));
            if !problems.is_empty() {
                bail!("scenario {n}: {}", problems.join("; "));
            }
            Some((n.to_string(), sc))
        }
        None if o.hires.is_some() || o.script.is_some() => {
            bail!("hires and script change a scenario: pick one")
        }
        None => None,
    };
    let mut articles = o
        .articles
        .as_deref()
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    if let Some((_, sc)) = &scenario {
        if !sc.articles.trim().is_empty() {
            if !articles.is_empty() {
                articles.push_str("\n\n");
            }
            articles.push_str(sc.articles.trim());
        }
    }
    if articles.len() > MAX_PROMPT {
        bail!("keep the board's articles under {MAX_PROMPT} bytes");
    }
    if let Some(share) = o.security_share {
        firm.budget.security_share = share;
    }
    if let Some(n) = o.max_concurrent {
        firm.budget.max_concurrent = n;
    }
    if let Some(n) = o.daily_turns {
        firm.budget.daily_turns = n;
    }
    if let Some(seed) = &o.seed {
        firm.budget.seed = match seed {
            Value::Number(n) => n
                .as_u64()
                .ok_or_else(|| anyhow!("seed: a whole number of tokens"))?,
            Value::String(s) => zapper_policy::parse_tokens(s)?,
            Value::Null => firm.budget.seed,
            _ => bail!("seed: tokens, like 500k"),
        };
    }
    let errors = firm.validate(name, config);
    if !errors.is_empty() {
        bail!("{}", errors.join("; "));
    }
    Ok(Started {
        firm,
        company,
        prompts,
        articles,
        scenario,
    })
}

/// Builds a firm's run: its directory, journal, post office, ledger, drive
/// and grants, in the registry as starting. `up_with` then boots it.
fn create_run(
    d: &Core,
    name: &str,
    config: &Config,
    started: Started,
) -> Result<(
    Arc<FirmRun>,
    tokio::sync::mpsc::UnboundedReceiver<Arc<WorkerRt>>,
)> {
    let Started {
        firm,
        company,
        prompts,
        articles,
        scenario,
    } = started;
    let id = run_id();
    let dir = firm_dir(d, name).join("runs").join(&id);
    for sub in ["transcripts", "drive", "skills", "empty"] {
        std::fs::create_dir_all(dir.join(sub))?;
    }
    let journal = Journal::create(&dir.join("journal.jsonl"))?;
    let domain = firm.domain(name);
    let roles = firm
        .roles
        .keys()
        .map(|r| (r.clone(), Arc::new(RwLock::new(Policy::default()))))
        .collect();
    let workers = firm
        .workers
        .iter()
        .map(|w| (w.name.clone(), Arc::new(WorkerRt::new(&firm, w))))
        .collect();
    let started = now();
    let models = role_models(&firm, config);
    let agents = profile_agents(config);
    let live = Arc::new(RwLock::new(firm.clone()));
    let (boots, boot_rx) = tokio::sync::mpsc::unbounded_channel();
    let run = Arc::new(FirmRun {
        boots,
        firm: name.to_string(),
        id: id.clone(),
        dir: dir.clone(),
        post: Mutex::new(PostOffice::new(&firm, live.clone())),
        chart: live,
        ledger: Mutex::new(workforce::Ledger::new(&firm)),
        models: models.clone(),
        agents: agents.clone(),
        company: company.clone(),
        prompts: prompts.clone(),
        articles: articles.clone(),
        scenario: scenario.as_ref().map(|(n, sc)| scenario::Play::new(n, sc)),
        close_waiter: Mutex::new(None),
        departed: Mutex::new(BTreeMap::new()),
        drive: Mutex::new(drive::Drive::new(&firm)?),
        security: Mutex::new(security::Security::default()),
        contracts: Mutex::new(contracts::Contracts::default()),
        access: Mutex::new(access::Access::default()),
        roles,
        turns: Arc::new(Semaphore::new(firm.budget.max_concurrent as usize)),
        usage: Mutex::new(Usage {
            day: utc_day(started),
            ..Default::default()
        }),
        facility: facility::Facilities::default(),
        config: firm.clone(),
        domain: domain.clone(),
        started,
        state: Mutex::new(RunState::Starting),
        journal,
        workers: RwLock::new(workers),
        tools: Mutex::new(None),
        stopping: Notify::new(),
    });
    let info = json!({ "firm": name, "company": company, "prompts": prompts, "articles": articles, "scenario": scenario.as_ref().map(|(n, sc)| json!({ "name": n, "scenario": sc })), "run": id, "started": started, "chart": chart(name, &firm, &models, &agents), "budget": firm.budget, "agent_version": env!("CARGO_PKG_VERSION") });
    zapper_core::util::write_private(
        &dir.join("run.json"),
        serde_json::to_string_pretty(&info)?.as_bytes(),
    )?;
    // The whole config the run started with, for replay: profiles, rules,
    // firms. Inline secret values are hidden, as in `zapper config startup`.
    zapper_core::util::write_private(
        &dir.join(RUN_CONFIG),
        zapper_core::util::redact(config.clone())
            .to_yaml()
            .as_bytes(),
    )?;
    run.journal.append("run", json!({ "state": "starting", "company": company, "prompts": prompts, "articles": articles, "scenario": scenario.as_ref().map(|(n, sc)| json!({ "name": n, "scenario": sc })), "chart": chart(name, &firm, &models, &agents), "budget": firm.budget }));
    workforce::seed(&run);
    // The policy owners (the CEO, any founding officers) hold admin on
    // everything; everyone else starts with nothing.
    for o in access::owners(&run) {
        access::owner_access(d, &run, &o, BOARD);
    }
    facility::register(d, &run)?;
    security::register(d, &run);
    ops::register(d, &run)?;
    d.office()
        .runs
        .write()
        .unwrap()
        .insert(name.to_string(), run.clone());
    Ok((run, boot_rx))
}

/// A run of `name` without VMs, for tests: built as `up_with` builds it,
/// running, each founder on a session registered with their labels and
/// role layer, and their grants applied.
#[cfg(test)]
pub(crate) fn test_run(d: &Core, name: &str) -> Arc<FirmRun> {
    let config = d.config();
    let firm = config.firms[name].clone();
    let started = with_options(name, firm, &config, &StartOptions::default()).unwrap();
    let (run, _boots) = create_run(d, name, &config, started).unwrap();
    *run.state.lock().unwrap() = RunState::Running;
    for w in run.rts() {
        let profile = run
            .chart()
            .worker(&w.name)
            .and_then(|x| x.profile.clone())
            .unwrap_or_else(|| run.config.roles[&w.role].profile.clone());
        let mut new = NewSession::plain(profile, run.dir.join("empty"), Policy::default());
        new.labels = labels(&run.firm, &run.id, &w.name, true);
        new.shared_layers = vec![(
            zapper_policy::Scope::Role(format!("{}/{}", run.firm, w.role)),
            run.roles[&w.role].clone(),
        )];
        *w.session.lock().unwrap() = Some(d.test_session(new, true).unwrap());
        access::reapply(d, &run, &w.name);
    }
    run
}

pub async fn up_with(d: &Core, name: &str, opts: &StartOptions) -> Result<Arc<FirmRun>> {
    let task = opts.task.as_deref().and_then(task_parts);
    if let Some((_, body)) = &task {
        if body.len() > mail::MAX_BODY {
            bail!("the task is too long: at most {} bytes", mail::MAX_BODY);
        }
    }
    if let Ok(r) = d.office().run(name) {
        if !r.ended() {
            bail!("firm {name} is already running (run {})", r.id);
        }
    }
    let config = d.config();
    let firm = config
        .firms
        .get(name)
        .cloned()
        .ok_or_else(|| anyhow!("no firm {name}"))?;
    let Started {
        firm,
        company,
        prompts,
        articles,
        scenario,
    } = with_options(name, firm, &config, opts)?;
    check_resources(d, &config, &firm)?;
    for r in firm.roles.values() {
        harness_driver(&config.resolve(&r.profile)?)?;
    }
    let (run, boot_rx) = create_run(
        d,
        name,
        &config,
        Started {
            firm,
            company,
            prompts,
            articles,
            scenario,
        },
    )?;

    if let Err(e) = boot(d, &run, &config).await {
        let msg = format!("{e:#}");
        run.journal
            .append("run", json!({ "state": "failed", "error": msg }));
        down(d, &run, "boot failed").await;
        bail!("firm {name}: {msg}");
    }
    *run.state.lock().unwrap() = RunState::Running;
    run.journal.append("run", json!({ "state": "running" }));
    for w in run.rts() {
        tokio::spawn(foreman::run(d.clone(), run.clone(), w));
    }
    tokio::spawn(ticker(d.clone(), run.clone()));
    tokio::spawn(booter(d.clone(), run.clone(), boot_rx));
    // The board's task, if it gave one: the CEO's first assignment.
    if let (Some((subject, body)), Some(ceo)) = (task, run.chart().root().map(str::to_string)) {
        if let Err(e) = board_send(
            &run,
            &json!({ "to": [ceo], "kind": "assignment", "subject": subject, "body": body }),
        ) {
            tracing::warn!(firm = %name, "the board's task: {e:#}");
            run.journal.append(
                "op_error",
                json!({ "op": "task", "error": format!("{e:#}") }),
            );
        }
    }
    Ok(run)
}

/// Boots the VMs of workers hired during the run, and starts their foremen.
async fn booter(
    d: Core,
    run: Arc<FirmRun>,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<Arc<WorkerRt>>,
) {
    while let Some(w) = rx.recv().await {
        if run.ended() {
            return;
        }
        let Some(role) = run.config.roles.get(&w.role).cloned() else {
            continue;
        };
        let config = d.config();
        let (d, run) = (d.clone(), run.clone());
        tokio::spawn(async move {
            run.set_state(&w, WorkerState::Booting, None);
            match boot_worker(&d, &run, &config, &w, &role).await {
                // Let go while booting: stop the VM again.
                Ok(()) if run.ended() || run.rt(&w.name).is_none() => {
                    if let Some(s) = w.session.lock().unwrap().take() {
                        let d = d.clone();
                        tokio::spawn(async move {
                            d.stop_session(&s, "let go").await;
                            let _ = d.forget_session(&s.id);
                        });
                    }
                }
                Ok(()) => {
                    tokio::spawn(foreman::run(d.clone(), run.clone(), w));
                }
                Err(e) => {
                    run.set_state(&w, WorkerState::Error, Some(format!("{e:#}")));
                    run.journal.append(
                        "op_error",
                        json!({ "worker": w.name, "op": "boot", "error": format!("{e:#}") }),
                    );
                }
            }
        });
    }
}

async fn boot(d: &Core, run: &Arc<FirmRun>, config: &Config) -> Result<()> {
    let firm = &run.config;
    facility::start(d, run, config).await?;
    skills::prepare(run, config)?;
    let mut tasks = Vec::new();
    for w in run.rts() {
        let (d, run) = (d.clone(), run.clone());
        let role = firm.roles[&w.role].clone();
        let config = config.clone();
        tasks.push(tokio::spawn(async move {
            run.set_state(&w, WorkerState::Booting, None);
            let r = boot_worker(&d, &run, &config, &w, &role).await;
            if let Err(e) = &r {
                run.set_state(&w, WorkerState::Error, Some(format!("{e:#}")));
            }
            r.with_context(|| format!("worker {}", w.name))
        }));
    }
    let mut first_err = None;
    for t in tasks {
        if let Err(e) = t.await.map_err(|e| anyhow!("{e}")).and_then(|r| r) {
            first_err.get_or_insert(e);
        }
    }
    match first_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

pub(crate) async fn boot_worker(
    d: &Core,
    run: &Arc<FirmRun>,
    config: &Config,
    w: &Arc<WorkerRt>,
    role: &zapper_policy::Role,
) -> Result<()> {
    // The worker's own profile (a founder's, a scenario slot's), or the
    // role's.
    let own = run.chart().worker(&w.name).and_then(|x| x.profile.clone());
    let profile_name = own.clone().unwrap_or_else(|| role.profile.clone());
    let profile = config.resolve(&profile_name)?;
    harness_driver(&profile)?;
    let user = profile.agent.user.clone();
    let home = format!("/home/{user}");
    let services = facility::services_for(run, &w.role);
    let preamble = prompts::preamble(
        &run.firm,
        &run.chart(),
        &run.config,
        &w.name,
        &services,
        run.prompts
            .get(&w.name)
            .cloned()
            .or_else(|| run.scenario.as_ref().and_then(|p| p.prompt_of(&w.name)))
            .as_deref(),
        &run.articles,
    ) + &contracts::of(run, &w.name)
        .map(|c| contracts::preamble(&c))
        .unwrap_or_default()
        + if scenario::board_line(run, &w.name) {
            "\nThe board has also opened its address (board) to you: you may mail it, whatever else limits your mail.\n"
        } else {
            ""
        };
    let mut files = facility::credentials_for(run, &w.name, &w.role, &home)?;
    // A headless browser command, for the image's Playwright.
    files.push(GuestFile {
        path: "/usr/local/bin/browse".into(),
        content: include_str!("browse.js").into(),
        mode: 0o755,
        user_owned: false,
    });
    if let Some(script) = &role.fake {
        let text = std::fs::read_to_string(zapper_policy::expand_home(script))
            .with_context(|| format!("fake script {script}"))?;
        files.push(GuestFile {
            path: format!("{home}/.office/fake.yaml"),
            content: text,
            mode: 0o644,
            user_owned: true,
        });
    }
    // The worker's setup in the harness's neutral terms: the preamble as
    // its instructions, the role's skills, the office's tools, and mail that
    // arrives while the agent works handed over before it ends the turn
    // (foreman::stop_check). The harness puts each where it reads it.
    let skills_dir = run.dir.join("skills").join(&w.role);
    let mut overlay = json!({
        "instructions": preamble,
        "skills": skills_dir.to_string_lossy(),
        "mcp_servers": { "office": { "command": ops::AGENT_PATH, "args": ["office", "mcp"] } },
        "turn_end": format!("{} office stop-hook", ops::AGENT_PATH),
    });
    // The role's model, unless the worker runs on a profile of their own,
    // which brings its model.
    if let (Some(m), None) = (&role.model, &own) {
        overlay["model"] = json!(m);
    }
    let new = NewSession {
        profile: profile_name,
        workdir: run.dir.join("empty"),
        policy: Policy::default(),
        labels: labels(&run.firm, &run.id, &w.name, true),
        // The role's layer, which every worker of the role shares.
        shared_layers: vec![(
            zapper_policy::Scope::Role(format!("{}/{}", run.firm, w.role)),
            run.roles[&w.role].clone(),
        )],
        files,
        mounts: vec![],
        agent_overlay: Some(overlay),
        forwards: vec![],
    };
    let s = d.create_session(new).await?;
    run.journal.append(
        "worker_session",
        json!({ "worker": w.name, "session": s.id }),
    );
    *w.session.lock().unwrap() = Some(s);
    // Grants made before the VM was up: its network rules and kubeconfig.
    access::reapply(d, run, &w.name);
    Ok(())
}

/// How the office drives a profile's agent: its harness's driver.
fn harness_driver(
    profile: &zapper_policy::ResolvedProfile,
) -> Result<&'static dyn zapper_core::harness::Driver> {
    let kind = &profile.agent.kind;
    zapper_core::harness::get(kind)
        .and_then(|h| h.driver())
        .ok_or_else(|| anyhow!("profile {}: agent kind {kind} can't run an office worker (those that can: claude-code, pi)", profile.name))
}

/// Follow-ups, digests and expiries.
async fn ticker(d: Core, run: Arc<FirmRun>) {
    let mut last_digest = now();
    loop {
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(2)) => {}
            _ = run.stopping.notified() => return,
        }
        if run.ended() {
            return;
        }
        let sent = run.post.lock().unwrap().tick(&run.journal, now());
        for s in &sent {
            run.deliver(s);
        }
        security::tick(&run, &mut last_digest);
        contracts::tick(&d, &run);
        access::tick(&d, &run);
        workforce::tick(&run);
        run.tell_close_waiter();
        scenario::tick(&d, &run);
    }
}

/// Stops a run: its workers' sessions, its facilities, its tasks.
pub async fn down(d: &Core, run: &Arc<FirmRun>, reason: &str) {
    {
        let mut st = run.state.lock().unwrap();
        if matches!(*st, RunState::Ended) {
            return;
        }
        *st = RunState::Stopping;
    }
    run.stopping.notify_waiters();
    for w in run.rts() {
        w.wake.notify_one();
    }
    let mut tasks = Vec::new();
    for w in run.rts() {
        if let Some(s) = w.session.lock().unwrap().take() {
            let d = d.clone();
            tasks.push(tokio::spawn(async move {
                d.stop_session(&s, "firm down").await;
                let _ = d.forget_session(&s.id);
            }));
        }
        run.set_state(&w, WorkerState::Off, None);
    }
    for t in tasks {
        let _ = t.await;
    }
    facility::stop(d, run).await;
    security::unregister(d, run);
    ops::unregister(d, run);
    *run.state.lock().unwrap() = RunState::Ended;
    run.journal
        .append("run", json!({ "state": "ended", "reason": reason }));
}

pub fn pause(run: &FirmRun, paused: bool) -> Result<()> {
    pause_for(run, paused, None)
}

/// Pauses or resumes the run, with the reason in the record.
fn pause_for(run: &FirmRun, paused: bool, reason: Option<&str>) -> Result<()> {
    let mut st = run.state.lock().unwrap();
    match (&*st, paused) {
        (RunState::Running, true) => *st = RunState::Paused,
        (RunState::Paused, false) => *st = RunState::Running,
        (RunState::Running, false) | (RunState::Paused, true) => return Ok(()),
        (s, _) => bail!("the run is {s:?}"),
    }
    drop(st);
    run.journal.append(
        "run",
        json!({ "state": if paused { "paused" } else { "running" }, "reason": reason }),
    );
    for w in run.rts() {
        w.wake.notify_one();
    }
    Ok(())
}

/// The board sends mail.
pub fn board_send(run: &FirmRun, v: &Value) -> Result<mail::Sent> {
    let d = ops::draft_from(BOARD, v, None, &run.post.lock().unwrap())?;
    run.send(d)
}

/// Ends the journals of runs a previous daemon left running: they can't be
/// resumed, and a replay should say how they ended.
/// The office's API routes (firms, mail, security, the drive), for the API
/// and the UI to serve.
pub fn routes(d: &Core) -> axum::Router {
    http::read_routes()
        .merge(http::board_routes())
        .with_state(d.clone())
}

/// The office's page: the UI's front page.
pub const PAGE: &str = include_str!("office.html");

/// The office in a starting daemon: runs a previous daemon left running are
/// ended; the firms' role layers and their officers' standing approvals are
/// rules the office keeps; the daemon stopping stops the firms.
pub fn start(d: &Core) {
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

pub fn close_orphans(state: &std::path::Path) {
    let Ok(firms) = std::fs::read_dir(state.join("firms")) else {
        return;
    };
    for firm in firms.flatten() {
        let Ok(runs) = std::fs::read_dir(firm.path().join("runs")) else {
            continue;
        };
        for run in runs.flatten() {
            let path = run.path().join("journal.jsonl");
            let Ok(text) = std::fs::read(zapper_core::record::view(&path)) else {
                continue;
            };
            let tail = &text[text.len().saturating_sub(64 * 1024)..];
            let ended = String::from_utf8_lossy(tail)
                .lines()
                .rev()
                .filter_map(|l| serde_json::from_str::<Value>(l).ok())
                .find(|v| v["type"] == "run")
                .is_some_and(|v| v["state"] == "ended" || v["state"] == "failed");
            if !ended {
                if let Ok(j) = Journal::create(&path) {
                    j.append(
                        "run",
                        json!({ "state": "ended", "reason": "the daemon stopped" }),
                    );
                }
            }
        }
    }
}

/// Past runs of a firm, newest first.
pub fn list_runs(d: &Core, firm: &str) -> Vec<Value> {
    let dir = firm_dir(d, firm).join("runs");
    let mut out: Vec<Value> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if !is_run_id(&name) {
                return None;
            }
            let info: Value = std::fs::read(e.path().join("run.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(json!({}));
            let journal = zapper_core::record::view(&e.path().join("journal.jsonl"));
            let size = std::fs::metadata(&journal).map(|m| m.len()).unwrap_or(0);
            let sum = run_summary(&journal);
            let note = std::fs::read_to_string(e.path().join(RUN_NOTE)).ok();
            Some(json!({
                "run": name, "started": info["started"], "journal_bytes": size, "note": note,
                "kickoff": sum.kickoff, "messages": sum.messages, "last": sum.last, "ended": sum.ended,
            }))
        })
        .collect();
    out.sort_by(|a, b| b["run"].as_str().cmp(&a["run"].as_str()));
    if let Ok(r) = d.office().run(firm) {
        for v in out.iter_mut() {
            if v["run"] == json!(r.id) {
                v["state"] = json!(*r.state.lock().unwrap());
            }
        }
    }
    out
}

/// The config a run started with, in its directory.
pub const RUN_CONFIG: &str = "config.yaml";

/// The board's note on a run, in its directory: free text to find the run
/// again later. Not part of the record.
pub const RUN_NOTE: &str = "note.txt";
/// The longest note, in bytes.
const MAX_NOTE: usize = 4000;

/// Sets the board's note on a run; empty removes it.
pub fn set_note(d: &Core, firm: &str, run: &str, note: &str) -> Result<()> {
    let dir = run_dir(d, firm, run)?;
    let note = note.trim();
    if note.len() > MAX_NOTE {
        bail!("keep the note under {MAX_NOTE} bytes");
    }
    let path = dir.join(RUN_NOTE);
    if note.is_empty() {
        match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => return Ok(()),
        }
    }
    let tmp = dir.join(format!("{RUN_NOTE}.tmp"));
    zapper_core::util::write_private(&tmp, note.as_bytes())?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// A run's config and the skills its workers had: what it was run with.
pub fn run_config(dir: &std::path::Path) -> Value {
    let config = std::fs::read_to_string(dir.join(RUN_CONFIG)).ok();
    // skills/<role>/<skill>/…: SKILL.md as text, other files by name.
    let mut skills = Vec::new();
    let mut roles: Vec<_> = std::fs::read_dir(dir.join("skills"))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_dir())
        .collect();
    roles.sort_by_key(|e| e.file_name());
    for role in roles {
        let mut list: Vec<_> = std::fs::read_dir(role.path())
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().is_dir())
            .collect();
        list.sort_by_key(|e| e.file_name());
        for sk in list {
            let text = std::fs::read(sk.path().join("SKILL.md"))
                .ok()
                .map(|b| String::from_utf8_lossy(&b[..b.len().min(256 << 10)]).into_owned());
            let mut files: Vec<String> = std::fs::read_dir(sk.path())
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n != "SKILL.md")
                .collect();
            files.sort();
            skills.push(json!({ "role": role.file_name().to_string_lossy(), "skill": sk.file_name().to_string_lossy(), "text": text, "files": files }));
        }
    }
    json!({ "config": config, "skills": skills })
}

/// What a run's journal says about it, for the list of runs.
struct RunSummary {
    /// The board's first message: what the firm was asked to build. A run
    /// without one was never kicked off.
    kickoff: Option<Value>,
    messages: u64,
    last: Option<f64>,
    ended: bool,
}

fn run_summary(journal: &std::path::Path) -> RunSummary {
    use std::io::BufRead;
    let mut sum = RunSummary {
        kickoff: None,
        messages: 0,
        last: None,
        ended: false,
    };
    let Ok(f) = std::fs::File::open(journal) else {
        return sum;
    };
    for line in std::io::BufReader::new(f)
        .split(b'\n')
        .map_while(Result::ok)
    {
        // Most lines are not needed whole: look before parsing.
        let mail = has(&line, br#""type":"mail""#);
        let run = has(&line, br#""type":"run""#);
        if !mail && !run && sum.last.is_some() && line.len() > 64 {
            if let Some(ts) = ts_of(&line) {
                sum.last = Some(ts);
            }
            continue;
        }
        let Ok(v) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        if let Some(ts) = v["ts"].as_f64() {
            sum.last = Some(ts);
        }
        if mail {
            sum.messages += 1;
            if sum.kickoff.is_none() && v["from"] == BOARD {
                let body = v["body"].as_str().unwrap_or("");
                let preview: String = body
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .chars()
                    .take(240)
                    .collect();
                sum.kickoff = Some(
                    // The whole message too, for the board to give the
                    // next run the same task.
                    json!({ "subject": v["subject"], "preview": preview, "body": body, "to": v["to"], "ts": v["ts"] }),
                );
            }
        } else if run {
            sum.ended = matches!(v["state"].as_str(), Some("ended" | "failed"));
        }
    }
    sum
}

fn has(line: &[u8], pat: &[u8]) -> bool {
    line.windows(pat.len()).any(|w| w == pat)
}

/// `"ts":<number>` from a journal line, without parsing it all.
fn ts_of(line: &[u8]) -> Option<f64> {
    let key = br#""ts":"#;
    let at = line.windows(key.len()).position(|w| w == key)? + key.len();
    let rest = &line[at..];
    let end = rest
        .iter()
        .position(|b| {
            !(b.is_ascii_digit()
                || *b == b'.'
                || *b == b'-'
                || *b == b'e'
                || *b == b'E'
                || *b == b'+')
        })
        .unwrap_or(rest.len());
    std::str::from_utf8(&rest[..end]).ok()?.parse().ok()
}

/// A run's directory; `current` is the firm's current or last run.
pub fn run_dir(d: &Core, firm: &str, run: &str) -> Result<PathBuf> {
    if !zapper_policy::valid_worker_name(firm) {
        bail!("{firm:?} is not a firm name");
    }
    let id = if run == "current" {
        d.office().run(firm)?.id.clone()
    } else {
        run.to_string()
    };
    if !is_run_id(&id) {
        bail!("{run:?} is not a run id");
    }
    let dir = firm_dir(d, firm).join("runs").join(&id);
    if !dir.is_dir() {
        bail!("no run {id} of firm {firm}");
    }
    Ok(dir)
}

pub fn prune(d: &Core, firm: &str, run: &str) -> Result<()> {
    let dir = run_dir(d, firm, run)?;
    if let Ok(r) = d.office().run(firm) {
        if r.dir == dir && !r.ended() {
            bail!("run {run} is running; stop the firm first");
        }
    }
    std::fs::remove_dir_all(&dir)?;
    Ok(())
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
mod tests {
    use super::OfficeOf;

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
    fn task_parts() {
        assert_eq!(super::task_parts("  "), None);
        assert_eq!(
            super::task_parts("Build a news portal\n\nRSS feeds, ads, a launch plan."),
            Some((
                "Build a news portal".into(),
                "RSS feeds, ads, a launch plan.".into()
            ))
        );
        // One line is both the subject and the message.
        assert_eq!(
            super::task_parts("# Build it"),
            Some(("Build it".into(), "# Build it".into()))
        );
    }

    #[test]
    fn start_options() {
        let c = zapper_policy::Config::from_yaml(
            "profiles: { w: {} }\nfirms:\n  acme:\n    budget: { seed: 1000 }\n    landing: {}\n    facilities: { cluster: { kind: k3s, profile: w } }\n    roles: { ceo: { profile: w, skills: { work: ceo }, hires: [sec] }, sec: { profile: w, skills: { work: security-review }, officer: true } }\n    workers:\n      - { name: barbara, role: ceo }\n    names: [alice, grace]\n",
        )
        .unwrap();
        let firm = c.firms["acme"].clone();
        let o = super::StartOptions {
            company: Some("Widgets Ltd".into()),
            ceo: Some("Alice".into()),
            seed: Some(serde_json::json!("2k")),
            ceo_prompt: Some("Ship in a day.".into()),
            articles: Some("No crypto.".into()),
            max_concurrent: Some(2),
            ..Default::default()
        };
        let super::Started {
            firm: f,
            company,
            prompts,
            articles,
            scenario,
        } = super::with_options("acme", firm.clone(), &c, &o).unwrap();
        assert!(scenario.is_none());
        assert_eq!(articles, "No crypto.");
        assert_eq!(f.budget.max_concurrent, 2);
        assert_eq!(company, "Widgets Ltd");
        assert_eq!(f.landing.as_ref().unwrap().name, "Widgets Ltd");
        assert_eq!(f.root(), Some("alice"));
        assert_eq!(f.workers.len(), 1, "a company starts with its CEO");
        assert_eq!(f.security.officers, vec!["alice"], "who holds security");
        assert_eq!(f.budget.seed, 2000);
        assert!(
            !f.names.contains(&"alice".to_string()),
            "a founder's name leaves the pool"
        );
        assert_eq!(
            prompts.get("alice").map(String::as_str),
            Some("Ship in a day.")
        );
        // Defaults, no officer to name, bad terms and a bad name.
        let s = super::with_options("acme", firm.clone(), &c, &Default::default()).unwrap();
        assert_eq!(s.company, "Acme Inc.");
        assert!(serde_json::from_str::<super::StartOptions>(r#"{"officer":"sam"}"#).is_err());
        let share = super::StartOptions {
            security_share: Some(1.5),
            ..Default::default()
        };
        assert!(super::with_options("acme", firm.clone(), &c, &share).is_err());
        let bad = super::StartOptions {
            ceo: Some("9 lives".into()),
            ..Default::default()
        };
        assert!(super::with_options("acme", firm, &c, &bad).is_err());
    }

    #[test]
    fn start_scenarios() {
        let c = zapper_policy::Config::from_yaml(
            "profiles: { w: {} }\nfirms:\n  acme:\n    budget: { seed: 1000 }\n    roles: { ceo: { profile: w, skills: { work: ceo }, hires: [eng] }, eng: { profile: w, skills: { work: software-engineer } } }\n    workers:\n      - { name: barbara, role: ceo }\n    scenarios:\n      rt:\n        articles: Exercises happen.\n        hires: [{ role: eng, contract: true, board_line: true, prompt: red }]\n        script: [{ after: 1h, kill: { role: eng } }]\n",
        )
        .unwrap();
        let firm = c.firms["acme"].clone();
        let with = |o: super::StartOptions| super::with_options("acme", firm.clone(), &c, &o);
        let opts = |scenario: &str| super::StartOptions {
            scenario: Some(scenario.into()),
            articles: Some("Be kind.".into()),
            ..Default::default()
        };
        let (name, sc) = with(opts("rt")).unwrap().scenario.unwrap();
        assert_eq!(
            (name.as_str(), sc.hires.len(), sc.script.len()),
            ("rt", 1, 1)
        );
        assert_eq!(
            with(opts("rt")).unwrap().articles,
            "Be kind.\n\nExercises happen."
        );
        // The board's slots replace the scenario's; bad ones are refused.
        let mut o = opts("rt");
        o.hires = Some(vec![zapper_policy::HireSlot {
            role: "eng".into(),
            count: 3,
            prompt: "custom".into(),
            ..Default::default()
        }]);
        o.script = Some(vec![]);
        let sc = with(o.clone()).unwrap().scenario.unwrap().1;
        assert_eq!((sc.hires[0].count, sc.script.len()), (3, 0));
        o.hires.as_mut().unwrap()[0].role = "nope".into();
        assert!(with(o).unwrap_err().to_string().contains("unknown role"));
        assert!(with(opts("nope")).is_err());
        let loose = super::StartOptions {
            hires: Some(vec![]),
            ..Default::default()
        };
        assert!(with(loose).is_err());
    }

    #[test]
    fn run_summaries() {
        let dir =
            std::env::temp_dir().join(format!("zapper-runsum-{}", zapper_core::util::random_id(6)));
        std::fs::create_dir_all(&dir).unwrap();
        let j = dir.join("journal.jsonl");
        std::fs::write(&j, concat!(
            r#"{"seq":1,"ts":10.0,"type":"run","state":"running"}"#, "\n",
            r#"{"seq":2,"ts":11.5,"type":"mail","from":"ceo","to":["ada"],"subject":"x","body":"y"}"#, "\n",
            r#"{"seq":3,"ts":12.0,"type":"mail","from":"board","to":["ceo"],"subject":"Build a todo app","body":"An MVP,\n  please."}"#, "\n",
            r#"{"seq":4,"ts":20.25,"type":"worker","worker":"ceo","state":"working","detail":"thinking about the todo list app"}"#, "\n",
            r#"{"seq":5,"ts":30.0,"type":"run","state":"ended"}"#, "\n",
        )).unwrap();
        let s = super::run_summary(&j);
        assert_eq!(s.messages, 2);
        assert_eq!(s.kickoff.as_ref().unwrap()["subject"], "Build a todo app");
        assert_eq!(s.kickoff.as_ref().unwrap()["preview"], "An MVP, please.");
        // The message as sent, to send again.
        assert_eq!(s.kickoff.as_ref().unwrap()["body"], "An MVP,\n  please.");
        assert_eq!(s.last, Some(30.0));
        assert!(s.ended);
        std::fs::write(&j, r#"{"seq":1,"ts":10.0,"type":"run","state":"running"}"#).unwrap();
        assert!(
            super::run_summary(&j).kickoff.is_none(),
            "a run the board never wrote to has no kickoff"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn run_ids() {
        let id = super::run_id();
        assert!(super::is_run_id(&id), "{id}");
        assert!(!super::is_run_id("../../etc"));
        assert!(!super::is_run_id("20261016-140501-K3Z"));
    }
}
