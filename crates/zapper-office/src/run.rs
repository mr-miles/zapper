//! A firm's run: starting it (the chart, its facilities and founders),
//! its ticker, pausing it, the board's mail, and taking it down.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use zapper_core::prelude::*;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use tokio::sync::{Notify, Semaphore};
use zapper_core::util::now;
use zapper_core::{Core, NewSession};
use zapper_policy::proto::GuestFile;
use zapper_policy::{Config, Policy};

use crate::firm::{Firm, BOARD};
use crate::history::RUN_CONFIG;
use crate::journal::Journal;
use crate::mail::{self, PostOffice};
use crate::{
    access, contracts, drive, facility, firm, firm_dir, foreman, labels, ops, prompts, run_id,
    scenario, security, skills, utc_day, workforce, FirmRun, OfficeOf, RunState, Usage, WorkerRt,
    WorkerState,
};

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
pub(crate) fn chart(
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
    w: &crate::firm::Worker,
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
pub fn effective_model(role: &crate::firm::Role, config: &Config) -> Option<String> {
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
    let running = d.sessions().iter().filter(|s| !s.ended()).count();
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
    /// The GitHub repositories to work in, `owner/name`, instead of the
    /// config's (a firm with `git.github`). Each is checked against the
    /// owner's GitHub App when the run starts.
    pub repos: Option<Vec<String>>,
    /// The charter's terms, instead of the config's.
    pub security_share: Option<f64>,
    pub max_concurrent: Option<u32>,
    pub daily_turns: Option<u32>,
    /// A scenario of the firm's config to play.
    pub scenario: Option<String>,
    /// Hire slots and script steps instead of the scenario's own.
    pub hires: Option<Vec<crate::firm::HireSlot>>,
    pub script: Option<Vec<crate::firm::Step>>,
    /// The board's task for the CEO, sent as the run's first assignment
    /// once it is up: its first line is the subject, the rest the message.
    pub task: Option<String>,
    /// Files the board attaches to the task: `{name, data}`, data in
    /// base64. They go on the shared drive as one artifact by the board,
    /// attached to the task's mail.
    #[serde(default)]
    pub attachments: Vec<Attachment>,
}

/// A file the board attaches to its task.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct Attachment {
    pub name: String,
    pub data: String,
}

/// The board's files, decoded and checked.
fn task_files(opts: &StartOptions) -> Result<Vec<(String, Vec<u8>)>> {
    use base64::Engine;
    let files = opts
        .attachments
        .iter()
        .map(|a| {
            let data = base64::engine::general_purpose::STANDARD
                .decode(a.data.as_bytes())
                .map_err(|_| anyhow!("{}: data is not base64", a.name))?;
            Ok((a.name.clone(), data))
        })
        .collect::<Result<Vec<_>>>()?;
    if !files.is_empty() {
        super::drive::check_board_files(&files)?;
    }
    Ok(files)
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
    scenario: Option<(String, crate::firm::Scenario)>,
}

/// Longest extra prompt the board gives a founder.
const MAX_PROMPT: usize = 20_000;

/// Renames a founder for this run, everywhere the chart names them.
fn rename(firm: &mut Firm, old: &str, new: &str) -> Result<()> {
    if new == old {
        return Ok(());
    }
    if !zapper_policy::valid_worker_name(new) || crate::firm::reserved_name(new) {
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
        .unwrap_or_else(|| format!("{} Inc.", crate::firm::title_case(name)))
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
    if let Some(repos) = o.repos.as_ref().filter(|r| !r.is_empty()) {
        // Repositories given outright are checked against the App when the
        // run starts; without them it lists the account's from GitHub.
        let Some(gh) = firm.git.as_mut().and_then(|g| g.github.as_mut()) else {
            bail!("repos is for a firm whose git is in GitHub (git.github)");
        };
        let mut named: Vec<String> = Vec::new();
        for r in repos {
            let r = r.trim();
            zapper_github::split_repo(r)?;
            if !named.iter().any(|n| n.eq_ignore_ascii_case(r)) {
                named.push(r.to_string());
            }
        }
        gh.repos = named;
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
            Value::String(s) => crate::firm::parse_tokens(s)?,
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
        post: mail::Post::new(PostOffice::new(&firm, live.clone())),
        chart: live,
        ledger: workforce::Books::new(&firm),
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
        plans: Mutex::new(crate::plan::Plans::default()),
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
    for w in run.config.warnings() {
        run.notice(
            BOARD,
            "A warning about the firm's config",
            &format!("[Automatic notice] firm {name}: {w}.\n"),
        );
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
    test_run_with(d, name, &StartOptions::default())
}

/// `test_run` with the board's start options.
#[cfg(test)]
pub(crate) fn test_run_with(d: &Core, name: &str, o: &StartOptions) -> Arc<FirmRun> {
    let config = d.config();
    let firm = firm::get(&config, name).unwrap();
    let started = with_options(name, firm, &config, o).unwrap();
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
    // Checked before anything boots: a bad attachment starts nothing.
    let files = task_files(opts)?;
    if !files.is_empty() && task.is_none() {
        bail!("attachments go with the board's task: give one");
    }
    if let Ok(r) = d.office().run(name) {
        if !r.ended() {
            bail!("firm {name} is already running (run {})", r.id);
        }
    }
    let config = d.config();
    let firm = firm::get(&config, name).ok_or_else(|| anyhow!("no firm {name}"))?;
    let Started {
        firm,
        company,
        prompts,
        articles,
        scenario,
    } = with_options(name, firm, &config, opts)?;
    check_resources(d, &config, &firm)?;
    let firm = super::github::with_repositories(firm).await?;
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
    if let Some((subject, body)) = task {
        if let Err(e) = send_task(&run, &subject, &body, files) {
            tracing::warn!(firm = %name, "the board's task: {e:#}");
            run.journal.append(
                "op_error",
                json!({ "op": "task", "error": format!("{e:#}") }),
            );
        }
    }
    Ok(run)
}

/// The board's task to the CEO, its files attached: they go on the drive
/// first, as one artifact by the board.
pub(crate) fn send_task(
    run: &Arc<FirmRun>,
    subject: &str,
    body: &str,
    files: Vec<(String, Vec<u8>)>,
) -> Result<mail::Sent> {
    let ceo = run
        .chart()
        .root()
        .map(str::to_string)
        .ok_or_else(|| anyhow!("the firm has no CEO"))?;
    let attachments = if files.is_empty() {
        vec![]
    } else {
        let v = super::drive::publish_board(run, files, &format!("{subject}: attachments"), None)?;
        vec![v["ref"].as_str().unwrap_or_default().to_string()]
    };
    board_send(
        run,
        &json!({ "to": [ceo], "kind": "assignment", "subject": subject, "body": body, "attachments": attachments }),
    )
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
    role: &crate::firm::Role,
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
        &skills::brief(run, &w.role),
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
        let sent = run.post.tick(&run.journal, now());
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
pub(crate) fn pause_for(run: &FirmRun, paused: bool, reason: Option<&str>) -> Result<()> {
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
    let d = run.post.with(|po| ops::draft_from(BOARD, v, None, po))?;
    run.send(d)
}

#[cfg(test)]
mod tests {
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
            "profiles: { w: {} }\nfirms:\n  acme:\n    budget: { seed: 1000 }\n    landing: {}\n    facilities: { cluster: { kind: k3s, profile: w } }\n    roles: { ceo: { profile: w, agent: ceo, hires: [sec] }, sec: { profile: w, agent: security-officer, officer: true } }\n    workers:\n      - { name: barbara, role: ceo }\n    names: [alice, grace]\n",
        )
        .unwrap();
        let firm = crate::firm::get(&c, "acme").unwrap();
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
            "profiles: { w: {} }\nfirms:\n  acme:\n    budget: { seed: 1000 }\n    roles: { ceo: { profile: w, agent: ceo, hires: [eng] }, eng: { profile: w, agent: software-engineer } }\n    workers:\n      - { name: barbara, role: ceo }\n    scenarios:\n      rt:\n        articles: Exercises happen.\n        hires: [{ role: eng, contract: true, board_line: true, prompt: red }]\n        script: [{ after: 1h, kill: { role: eng } }]\n",
        )
        .unwrap();
        let firm = crate::firm::get(&c, "acme").unwrap();
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
        o.hires = Some(vec![crate::firm::HireSlot {
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
    fn repositories_may_be_given_outright() {
        let yaml = |git: &str| {
            format!("profiles: {{ w: {{}} }}\nfirms:\n  acme:\n    roles: {{ ceo: {{ profile: w, agent: ceo }} }}\n    workers: [{{ name: barbara, role: ceo }}]\n{git}")
        };
        let github = "    git: { github: { config: acme.yaml } }\n    credentials:\n      github:\n        owner_profile: read\n        profiles: { read: { permissions: { contents: read } } }\n";
        let with = |y: String, repos: Option<Vec<&str>>| {
            let c = zapper_policy::Config::from_yaml(&y).unwrap();
            let firm = crate::firm::get(&c, "acme").unwrap();
            let o = super::StartOptions {
                repos: repos.map(|r| r.into_iter().map(String::from).collect()),
                ..Default::default()
            };
            super::with_options("acme", firm, &c, &o).map(|s| s.firm.github_repos())
        };
        // Given, they are used as they are (once each); a run with none lists the
        // owner's from GitHub when it starts, so none here is fine.
        assert_eq!(
            with(
                yaml(github),
                Some(vec![" acme/api ", "ACME/API", "Acme/web"])
            )
            .unwrap(),
            ["acme/api", "Acme/web"]
        );
        assert!(with(yaml(github), None).unwrap().is_empty());
        // Only OWNER/NAME (whose owner is checked against the App's account
        // when the run starts).
        assert!(with(yaml(github), Some(vec!["api"])).is_err());
        // A firm with its own git server has none to name.
        let e = with(yaml("    git: {}\n"), Some(vec!["acme/api"]))
            .unwrap_err()
            .to_string();
        assert!(e.contains("git.github"), "{e}");
    }
}
