//! Firms: an org chart of workers, each a Claude Code in its own session, with
//! the roles, budgets and services the office layer runs them with.
//!
//! The config names the founders (the workers who boot with the firm); a
//! firm with a seed budget grows by hiring from its roles while it runs. See
//! `docs/office.md`.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};

use zapper_policy::{parse_size, valid_worker_name, Action, Config};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Firm {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub board: Board,
    /// Names the firm's services: `git.<domain>`, `cluster.<domain>`,
    /// `apps.<domain>`. Empty means `<firm>.internal`.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub domain: String,
    /// Local address forwarded to the cluster's ingress, for the board's
    /// browser, e.g. `127.0.0.1:8080`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub product_port: Option<String>,
    pub budget: Budget,
    pub limits: Limits,
    pub roles: BTreeMap<String, Role>,
    pub workers: Vec<Worker>,
    pub security: Security,
    /// Git repositories per run, each empty at the start, at
    /// `https://git.<domain>/<name>.git`; `{}` is one, `product`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git: Option<GitService>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub facilities: BTreeMap<String, Facility>,
    /// Local address of the cluster's dashboard (Headlamp), for the board,
    /// e.g. `127.0.0.1:8081`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dashboard_port: Option<String>,
    /// The page the cluster serves before the firm deploys anything.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub landing: Option<Landing>,
    /// The credentials subsystem: how long grants last, and the shared
    /// drive's spaces. Access itself is granted during a run, never by role.
    pub credentials: Credentials,
    /// Directory of the skills any role may use (`<name>/SKILL.md`),
    /// relative to the config file; `skills` if unset. `zapper init` writes
    /// the defaults there.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skills_dir: Option<String>,
    /// Directory of the agent definitions (`<agent>/AGENT.md`, and the
    /// role's own skills in `<agent>/skills/<name>/SKILL.md`), relative to
    /// the config file; `agents` if unset. `zapper init` writes the
    /// defaults there.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agents_dir: Option<String>,
    /// Names for hires whose hirer doesn't pick one, in order.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub names: Vec<String>,
    /// Named presets the board picks when it starts a run: hires with extra
    /// instructions, and board actions during the run.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub scenarios: BTreeMap<String, Scenario>,
}

/// Longest extra prompt of a hire slot or a scenario's article text.
pub const MAX_SCENARIO_TEXT: usize = 20_000;

/// A scenario to simulate: who is hired with extra instructions, and what
/// the board does to the firm while it runs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Scenario {
    pub description: String,
    /// Added to the board's own articles of the charter.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub articles: String,
    /// Hires with extra instructions; they are filled in order, before any
    /// plain hire of the same role.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub hires: Vec<HireSlot>,
    /// Board actions at a time or a point of the run.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub script: Vec<Step>,
    /// A short note shown next to the name where the board picks one, like
    /// `unavailable on Claude`. Only text: the scenario can still be played.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub note: String,
}

/// `count` hires of `role` get `prompt` as the board's instructions, in
/// their preamble. Hires beyond the count are plain.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HireSlot {
    pub role: String,
    pub count: u32,
    /// `true`: only contractors fill it; `false`: only staff; unset: either.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contract: Option<bool>,
    pub prompt: String,
    /// The worker may mail the board directly, even as a contractor.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub board_line: bool,
    /// The profile these hires run on instead of the role's: another agent
    /// or model for them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
}

impl Default for HireSlot {
    fn default() -> Self {
        HireSlot {
            role: String::new(),
            count: 1,
            contract: None,
            prompt: String::new(),
            board_line: false,
            profile: None,
        }
    }
}

/// One board action, once, when all its triggers are met. Exactly one of
/// `kill`, `break_cluster` and `mail` is set.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Step {
    /// Time since the run started, like `90m`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
    /// Fraction (0 to 1) of the approved budget spent so far.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kill: Option<Kill>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub break_cluster: Option<BreakCluster>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mail: Option<StepMail>,
}

/// Kills a worker's VM and takes them out of the firm, as if they quit.
/// `role` picks the worker of that role who has spent the most.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Kill {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    pub reason: String,
}

/// Breaks the cluster: `prod` deletes the workloads of namespace prod (the
/// product is down; redeploying fixes it), `api` freezes the Kubernetes API
/// server (nothing inside can fix it).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BreakCluster {
    pub mode: String,
}

impl Default for BreakCluster {
    fn default() -> Self {
        BreakCluster {
            mode: "prod".into(),
        }
    }
}

/// Mail from the board; `to` is a worker or a role (the first worker of it).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StepMail {
    pub to: String,
    pub kind: String,
    pub subject: String,
    pub body: String,
}

impl Default for StepMail {
    fn default() -> Self {
        StepMail {
            to: String::new(),
            kind: "assignment".into(),
            subject: String::new(),
            body: String::new(),
        }
    }
}

impl Scenario {
    /// Hire slots whose profile can't run an office worker.
    pub fn profile_problems(&self, config: &Config) -> Vec<String> {
        self.hires
            .iter()
            .enumerate()
            .filter_map(|(i, h)| {
                let p = h.profile.as_deref()?;
                office_profile_problem(config, p).map(|e| format!("hires[{i}]: {e}"))
            })
            .collect()
    }

    /// What is wrong with the scenario in `firm`, one line each.
    pub fn problems(&self, firm: &Firm) -> Vec<String> {
        let mut out = Vec::new();
        let root_role = firm
            .root()
            .and_then(|r| firm.worker(r))
            .map(|w| w.role.as_str());
        if self.note.len() > 80 {
            out.push("note: keep it under 80 bytes".into());
        }
        if self.articles.len() > MAX_SCENARIO_TEXT {
            out.push(format!(
                "articles: keep them under {MAX_SCENARIO_TEXT} bytes"
            ));
        }
        for (i, h) in self.hires.iter().enumerate() {
            let at = format!("hires[{i}]");
            match firm.roles.get(&h.role) {
                None => out.push(format!("{at}: unknown role {:?}", h.role)),
                Some(r) => {
                    if h.contract == Some(true) && (r.officer || root_role == Some(h.role.as_str()))
                    {
                        out.push(format!("{at}: {} can't be a contractor", h.role));
                    }
                    if root_role == Some(h.role.as_str()) {
                        out.push(format!(
                            "{at}: the CEO is a founder; give them the CEO prompt instead"
                        ));
                    }
                }
            }
            if h.count == 0 || h.count > 20 {
                out.push(format!("{at}: count is from 1 to 20"));
            }
            if h.prompt.trim().is_empty() || h.prompt.len() > MAX_SCENARIO_TEXT {
                out.push(format!("{at}: prompt is 1 to {MAX_SCENARIO_TEXT} bytes"));
            }
            if h.board_line && h.contract != Some(true) {
                out.push(format!(
                    "{at}: board_line is for contractors (set contract: true)"
                ));
            }
        }
        for (i, s) in self.script.iter().enumerate() {
            let at = format!("script[{i}]");
            if s.after.is_none() && s.spent.is_none() {
                out.push(format!("{at}: needs a trigger: after or spent"));
            }
            if let Some(a) = &s.after {
                if let Err(e) = parse_duration(a) {
                    out.push(format!("{at}: after: {e}"));
                }
            }
            if s.spent.is_some_and(|f| !(f > 0.0 && f <= 1.0)) {
                out.push(format!("{at}: spent is a fraction above 0, up to 1"));
            }
            let actions =
                s.kill.is_some() as u8 + s.break_cluster.is_some() as u8 + s.mail.is_some() as u8;
            if actions != 1 {
                out.push(format!(
                    "{at}: set exactly one of kill, break_cluster, mail"
                ));
            }
            if let Some(k) = &s.kill {
                match (&k.worker, &k.role) {
                    (Some(_), Some(_)) | (None, None) => {
                        out.push(format!("{at}: kill takes a worker or a role"))
                    }
                    (_, Some(r)) if !firm.roles.contains_key(r) => {
                        out.push(format!("{at}: kill: unknown role {r:?}"))
                    }
                    (_, Some(r)) if root_role == Some(r.as_str()) => {
                        out.push(format!("{at}: kill: the CEO can't be killed"))
                    }
                    (Some(w), _) if !valid_worker_name(w) => {
                        out.push(format!("{at}: kill: {w:?} is not a worker name"))
                    }
                    _ => {}
                }
            }
            if let Some(b) = &s.break_cluster {
                if !["prod", "api"].contains(&b.mode.as_str()) {
                    out.push(format!("{at}: break_cluster.mode is prod or api"));
                }
                if firm.facilities.is_empty() {
                    out.push(format!("{at}: break_cluster needs a cluster facility"));
                }
            }
            if let Some(m) = &s.mail {
                if m.to.trim().is_empty() || m.subject.trim().is_empty() || m.body.trim().is_empty()
                {
                    out.push(format!("{at}: mail needs to, subject and body"));
                }
                if !["assignment", "question", "fyi"].contains(&m.kind.as_str()) {
                    out.push(format!("{at}: mail.kind is assignment, question or fyi"));
                }
                if m.body.len() > MAX_SCENARIO_TEXT {
                    out.push(format!(
                        "{at}: mail body is under {MAX_SCENARIO_TEXT} bytes"
                    ));
                }
            }
        }
        out
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Board {
    pub name: String,
}

impl Default for Board {
    fn default() -> Self {
        Board {
            name: "Board".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Budget {
    /// Claude turns the whole firm may run per day; 0 is no limit.
    pub daily_turns: u32,
    /// Turns per worker per day; 0 is no limit.
    pub worker_daily_turns: u32,
    /// Tokens (input and output) the firm may use per day; 0 is no limit.
    pub daily_tokens: u64,
    /// Workers in a turn at the same time.
    pub max_concurrent: u32,
    /// Tokens the CEO gets at the start of a run to scope the board's task.
    /// Non-zero makes the run's budget a deal with the board: the CEO asks
    /// for a budget (`budget_request`), the board approves it, and workers
    /// spend from balances that flow down the chart. 0: no run budget.
    pub seed: u64,
    /// The security officers' share of the seed and of every approved
    /// budget, from 0 to 1.
    pub security_share: f64,
    /// Tokens a contract offer moves from the hirer to the candidate for
    /// their quoting turns.
    pub quote_fee: u64,
    /// What a turn costs, in tokens, on the models the firm runs, before the
    /// CEO has taken one to measure: the CEO sizes the budget in turns
    /// priced with this. Set it for a firm on a different model.
    pub turn_price: u64,
    /// How long one turn may run (`30m`): then the agent is stopped and
    /// the turn charged at the worker's turn price.
    pub turn_timeout: String,
    /// How long a worker may stay out of tokens with mail waiting before
    /// the CEO is reminded (`10m`); the board is told at three times that.
    pub stall_after: String,
}

impl Default for Budget {
    fn default() -> Self {
        Budget {
            daily_turns: 400,
            worker_daily_turns: 0,
            daily_tokens: 0,
            max_concurrent: 3,
            seed: 0,
            security_share: 0.2,
            quote_fee: 30_000,
            turn_price: 10_000,
            turn_timeout: "30m".into(),
            stall_after: "10m".into(),
        }
    }
}

/// Bounds on what workers send, against runaway firms.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    /// Messages in one thread before it is paused.
    pub thread_messages: u32,
    /// Messages a worker may send per hour.
    pub mail_per_hour: u32,
    /// `all` and `team:` messages a worker may send per hour.
    pub fanout_per_hour: u32,
    /// Messages two workers may exchange in ten minutes before the post
    /// office stops them and tells the board: a guard against loops.
    pub pair_per_10min: u32,
    /// Messages in a run.
    pub mail_per_run: u32,
    /// Size of one published artifact, e.g. `50MiB`.
    pub artifact_size: String,
    /// Size of the shared drive of one run.
    pub drive_size: String,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            thread_messages: 60,
            mail_per_hour: 120,
            fanout_per_hour: 10,
            pair_per_10min: 30,
            mail_per_run: 20_000,
            artifact_size: "50MiB".into(),
            drive_size: "2GiB".into(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Role {
    /// The zapper profile workers of this role run with.
    pub profile: String,
    /// Claude model; the profile's if unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The agent definition workers of this role are: `agents_dir/<agent>/`,
    /// with its brief (`AGENT.md`) and the role's own skills. The role's
    /// name if unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    pub avatar: Avatar,
    /// Default title of workers of this role.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Host path of a script for the fake Claude, which plays it instead of
    /// running Claude Code. For tests.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fake: Option<String>,
    /// Roles a worker of this role may hire, during a run with a budget.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub hires: Vec<String>,
    /// Workers of this role are security officers: hired during a run,
    /// they join the officers once their background check clears.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub officer: bool,
    /// The profile contractors of this role run on, instead of `profile`:
    /// bounded jobs on another agent or model (a local one, say). A
    /// scenario slot's profile still comes first.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contract_profile: Option<String>,
}

impl Role {
    /// The agent definition's name: `agent`, else the role's own.
    pub fn agent_name<'a>(&'a self, role: &'a str) -> &'a str {
        self.agent.as_deref().unwrap_or(role)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Avatar {
    /// `ceo`, `product-manager`, `manager`, `engineer`, `designer`, `researcher`,
    /// `analyst`, `shield`; others draw a plain figure.
    pub figure: String,
    pub color: String,
}

impl Default for Avatar {
    fn default() -> Self {
        Avatar {
            figure: "plain".into(),
            color: "#D97757".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Worker {
    pub name: String,
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reports_to: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The worker's own profile instead of the role's: another agent or
    /// model for this one (a founder's, or a hire's from a scenario slot).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
}

/// Why `profile` can't run an office worker, if it can't: it must resolve,
/// and its agent must be one the office drives.
pub fn office_profile_problem(config: &Config, profile: &str) -> Option<String> {
    match config.resolve(profile) {
        Err(e) => Some(format!("profile {profile}: {e:#}")),
        Ok(p) if !["claude-code", "pi"].contains(&p.agent.kind.as_str()) => Some(format!(
            "profile {profile}: agent kind {} can't run an office worker (claude-code, pi)",
            p.agent.kind
        )),
        Ok(_) => None,
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Security {
    /// Workers who decide access requests and watch traffic.
    pub officers: Vec<String>,
    pub digest_every: Digest,
    /// After this long without an officer's answer, the board is asked too.
    pub answer_within: String,
    /// The first hire (or contract) of a firm that can hire a security
    /// officer is one: nobody else is hired before.
    pub officer_first: bool,
}

impl Default for Security {
    fn default() -> Self {
        Security {
            officers: Vec::new(),
            digest_every: Digest::default(),
            answer_within: "15m".into(),
            officer_first: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Digest {
    pub minutes: u32,
    pub requests: u32,
}

impl Default for Digest {
    fn default() -> Self {
        Digest {
            minutes: 10,
            requests: 200,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GitService {
    /// The repositories' names; `[product]` if unset. Access is granted per
    /// repository (`git:<name>`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub repos: Vec<String>,
}

/// The repository a firm with `git: {}` has.
pub const DEFAULT_REPO: &str = "product";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Facility {
    /// `k3s`.
    pub kind: String,
    pub profile: String,
}

/// A static landing page about the firm, deployed on the cluster at boot at
/// the lowest routing priority, so the product replaces it on `/`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Landing {
    /// The firm's display name; default: the firm's name, capitalised, with
    /// " Inc.".
    #[serde(skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub motto: String,
}

impl Landing {
    pub fn display_name(&self, firm: &str) -> String {
        if self.name.is_empty() {
            format!("{} Inc.", title_case(firm))
        } else {
            self.name.clone()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Credentials {
    /// How long a grant lasts unless its decider says otherwise, e.g. `24h`.
    pub expiry: String,
    /// The shared drive's spaces: a flat list of names. Every artifact is
    /// published into one, and grants are per space.
    pub spaces: Vec<String>,
}

impl Default for Credentials {
    fn default() -> Self {
        Credentials {
            expiry: "24h".into(),
            spaces: [
                "general",
                "product",
                "engineering",
                "design",
                "research",
                "security",
                "board",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
        }
    }
}

/// Addresses that aren't workers.
pub const BOARD: &str = "board";
const RESERVED: [&str; 6] = [BOARD, "all", "team", "security", "office", "post-office"];

/// Whether `name` is reserved for something other than a worker.
pub fn reserved_name(name: &str) -> bool {
    RESERVED.contains(&name)
}

/// Parses a token amount: `2000000`, `500k`, `2M`, `1.5M`.
pub fn parse_tokens(s: &str) -> Result<u64> {
    let t = s.trim().replace(['_', ','], "");
    let (num, mult) = match t.chars().last() {
        Some('k' | 'K') => (&t[..t.len() - 1], 1e3),
        Some('m' | 'M') => (&t[..t.len() - 1], 1e6),
        Some('g' | 'G' | 'b' | 'B') => (&t[..t.len() - 1], 1e9),
        _ => (t.as_str(), 1.0),
    };
    let n: f64 = num
        .trim()
        .parse()
        .map_err(|_| anyhow!("{s:?} is not a token amount like 500k or 2M"))?;
    if !n.is_finite() || n < 0.0 {
        bail!("{s:?} is not a token amount like 500k or 2M");
    }
    Ok((n * mult).round() as u64)
}

/// Parses `90s`, `15m`, `2h`, `1d`, or plain seconds.
pub fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: u64 = num.parse().map_err(|_| anyhow!("duration {s:?}"))?;
    let mult = match unit.trim() {
        "" | "s" => 1,
        "m" | "min" => 60,
        "h" => 3600,
        "d" => 86400,
        _ => bail!("duration {s:?}: unknown unit"),
    };
    Ok(Duration::from_secs(n * mult))
}

impl Firm {
    /// The firm's git repositories, in config order; none without `git`.
    pub fn git_repos(&self) -> Vec<String> {
        match &self.git {
            None => vec![],
            Some(g) if g.repos.is_empty() => vec![DEFAULT_REPO.to_string()],
            Some(g) => g.repos.clone(),
        }
    }

    pub fn domain(&self, firm: &str) -> String {
        if self.domain.is_empty() {
            format!("{firm}.internal")
        } else {
            self.domain.to_ascii_lowercase()
        }
    }

    pub fn worker(&self, name: &str) -> Option<&Worker> {
        self.workers.iter().find(|w| w.name == name)
    }

    pub fn role_of(&self, name: &str) -> Option<&Role> {
        self.worker(name).and_then(|w| self.roles.get(&w.role))
    }

    /// The worker's manager, `None` for the root.
    pub fn manager_of(&self, name: &str) -> Option<&str> {
        self.worker(name)?.reports_to.as_deref()
    }

    pub fn reports_of(&self, name: &str) -> Vec<&str> {
        self.workers
            .iter()
            .filter(|w| w.reports_to.as_deref() == Some(name))
            .map(|w| w.name.as_str())
            .collect()
    }

    /// The worker everyone reports to, directly or not.
    pub fn root(&self) -> Option<&str> {
        self.workers
            .iter()
            .find(|w| w.reports_to.is_none())
            .map(|w| w.name.as_str())
    }

    /// Whether `a` is above `b` in the chart (its manager, or the manager's
    /// manager, and so on). The board is above everyone.
    pub fn is_above(&self, a: &str, b: &str) -> bool {
        if a == BOARD {
            return b != BOARD;
        }
        let mut cur = self.manager_of(b);
        let mut hops = 0;
        while let Some(m) = cur {
            if m == a {
                return true;
            }
            hops += 1;
            if hops > self.workers.len() {
                return false;
            }
            cur = self.manager_of(m);
        }
        false
    }

    /// What in the firm's config is valid but likely not what was meant:
    /// `zapper check` notes it, and the board is told when a run starts.
    pub fn warnings(&self) -> Vec<String> {
        let mut out = Vec::new();
        let officers = !self.security.officers.is_empty() || self.roles.values().any(|r| r.officer);
        if self.budget.seed > 0 && self.budget.security_share == 0.0 && officers {
            out.push(
                "budget.security_share is 0, so the security officers get none of the budget: \
until someone funds them, every access request and background check goes to the board"
                    .into(),
            );
        }
        out
    }

    pub fn is_officer(&self, name: &str) -> bool {
        self.security.officers.iter().any(|o| o == name)
    }

    pub fn title_of(&self, name: &str) -> String {
        let Some(w) = self.worker(name) else {
            return if name == BOARD {
                self.board.name.clone()
            } else {
                name.to_string()
            };
        };
        w.title
            .clone()
            .or_else(|| self.roles.get(&w.role).and_then(|r| r.title.clone()))
            .unwrap_or_else(|| title_case(&w.role))
    }

    /// Checks the firm against the rest of the config.
    pub fn validate(&self, name: &str, config: &Config) -> Vec<String> {
        let mut errors = Vec::new();
        let mut err = |e: String| errors.push(format!("firm {name}: {e}"));
        if !valid_worker_name(name) {
            err("a firm name is lowercase letters, digits and dashes".into());
        }
        if zapper_policy::names::hostname(&self.domain(name)).is_err() {
            err(format!(
                "domain {:?} is not a valid host name",
                self.domain(name)
            ));
        }
        // Either may be served beyond loopback (`0.0.0.0:8080`), for a
        // remote host: the dashboard takes the token the UI shows, and
        // `zapper serve` warns when it listens on the network.
        for (field, port) in [
            ("product_port", &self.product_port),
            ("dashboard_port", &self.dashboard_port),
        ] {
            let Some(p) = port else { continue };
            if p.parse::<std::net::SocketAddr>().is_err() {
                err(format!(
                    "{field} {p:?} is not an address like 127.0.0.1:8080"
                ));
            }
        }
        if self.dashboard_port.is_some() && self.facilities.is_empty() {
            err("dashboard_port needs a cluster facility".into());
        }
        if self.budget.max_concurrent == 0 {
            err("budget.max_concurrent must be at least 1".into());
        }
        if let Err(e) = parse_duration(&self.security.answer_within) {
            err(format!("security.answer_within: {e}"));
        }
        match parse_duration(&self.budget.turn_timeout) {
            Err(e) => err(format!("budget.turn_timeout: {e}")),
            Ok(d) if d.is_zero() => err("budget.turn_timeout must be above 0".into()),
            Ok(_) => {}
        }
        if let Err(e) = parse_duration(&self.budget.stall_after) {
            err(format!("budget.stall_after: {e}"));
        }
        if self.budget.turn_price == 0 {
            err("budget.turn_price must be at least 1".into());
        }
        for (field, v) in [
            ("artifact_size", &self.limits.artifact_size),
            ("drive_size", &self.limits.drive_size),
        ] {
            if parse_size(v).is_err() {
                err(format!("limits.{field} {v:?} is not a size like 50MiB"));
            }
        }
        if self.workers.is_empty() {
            err("has no workers".into());
        }
        let mut names = BTreeSet::new();
        for w in &self.workers {
            if !valid_worker_name(&w.name) {
                err(format!("worker name {:?}: lowercase letters, digits and dashes, starting with a letter", w.name));
            }
            if RESERVED.contains(&w.name.as_str()) {
                err(format!("worker name {:?} is reserved", w.name));
            }
            if !names.insert(w.name.as_str()) {
                err(format!("worker {} appears twice", w.name));
            }
            if !self.roles.contains_key(&w.role) {
                err(format!("worker {}: unknown role {}", w.name, w.role));
            }
        }
        for w in &self.workers {
            if let Some(m) = &w.reports_to {
                if !names.contains(m.as_str()) {
                    err(format!("worker {} reports to unknown worker {m}", w.name));
                }
            }
        }
        let roots: Vec<&str> = self
            .workers
            .iter()
            .filter(|w| w.reports_to.is_none())
            .map(|w| w.name.as_str())
            .collect();
        if !self.workers.is_empty() && roots.len() != 1 {
            err(format!(
                "the chart needs exactly one worker who reports to no one, found {roots:?}"
            ));
        }
        // Everyone reaches the root: no cycles.
        for w in &self.workers {
            let mut seen = BTreeSet::new();
            let mut cur = Some(w.name.as_str());
            while let Some(c) = cur {
                if !seen.insert(c) {
                    err(format!("reports_to has a cycle through {c}"));
                    break;
                }
                cur = self.worker(c).and_then(|x| x.reports_to.as_deref());
            }
        }
        for o in &self.security.officers {
            if !names.contains(o.as_str()) {
                err(format!("security officer {o} is not a worker"));
            }
        }
        for (rname, role) in &self.roles {
            let agent = role.agent_name(rname);
            if !valid_worker_name(agent) {
                err(format!(
                    "role {rname}: agent {agent:?}: lowercase letters, digits and dashes"
                ));
            }
            match config.resolve(&role.profile) {
                Err(e) => err(format!("role {rname}: profile {}: {e:#}", role.profile)),
                Ok(p) => {
                    // Officers decide network access only; an exec can't wait
                    // minutes for them.
                    let mut layers: Vec<&zapper_policy::Policy> =
                        p.layers.iter().map(|(_, l)| l).collect();
                    layers.push(&config.policy);
                    if layers.iter().any(|l| {
                        l.exec.iter().any(|r| r.action == Action::Ask)
                            || l.defaults.exec == Some(Action::Ask)
                    }) {
                        err(format!("role {rname}: profile {} has exec rules that ask; in a firm, exec rules are allow or deny", role.profile));
                    }
                }
            }
        }
        for (rname, role) in &self.roles {
            if let Some(p) = &role.contract_profile {
                if let Some(e) = office_profile_problem(config, p) {
                    err(format!("role {rname}: contract_profile: {e}"));
                }
            }
        }
        for w in &self.workers {
            if let Some(p) = &w.profile {
                if let Some(e) = office_profile_problem(config, p) {
                    err(format!("worker {}: {e}", w.name));
                }
            }
        }
        for (fname, f) in &self.facilities {
            if f.kind != "k3s" {
                err(format!("facility {fname}: unknown kind {} (k3s)", f.kind));
            }
            if let Err(e) = config.resolve(&f.profile) {
                err(format!("facility {fname}: profile {}: {e:#}", f.profile));
            }
        }
        if self.facilities.len() > 1 {
            err("one facility (the cluster) is supported".into());
        }
        if let Err(e) = parse_duration(&self.credentials.expiry) {
            err(format!("credentials.expiry: {e}"));
        }
        let mut spaces = BTreeSet::new();
        for sp in &self.credentials.spaces {
            if !valid_worker_name(sp) || !spaces.insert(sp.as_str()) {
                err(format!("credentials.spaces: {sp:?} must be a unique name of lowercase letters, digits and dashes"));
            }
        }
        if self.credentials.spaces.is_empty() {
            err("credentials.spaces: at least one space for the shared drive".into());
        }
        let mut repos = BTreeSet::new();
        for r in self.git.iter().flat_map(|g| &g.repos) {
            if !valid_worker_name(r) || !repos.insert(r.as_str()) {
                err(format!("git.repos: {r:?} must be a unique name of lowercase letters, digits and dashes"));
            }
        }
        if self.product_port.is_some() && self.facilities.is_empty() {
            err("product_port needs a cluster facility".into());
        }
        if self.landing.is_some() && self.facilities.is_empty() {
            err("landing needs a cluster facility".into());
        }
        if let Some(l) = &self.landing {
            if l.name.len() > 200 || l.motto.len() > 500 {
                err("landing: keep the name under 200 and the motto under 500 characters".into());
            }
        }
        for (rname, role) in &self.roles {
            for h in &role.hires {
                if !self.roles.contains_key(h) {
                    err(format!("role {rname}: hires unknown role {h}"));
                }
            }
        }
        if !(0.0..=1.0).contains(&self.budget.security_share) {
            err("budget.security_share is a fraction from 0 to 1".into());
        }
        let hires = self.roles.values().any(|r| !r.hires.is_empty());
        if hires && self.budget.seed == 0 {
            err("roles that hire need a run budget: set budget.seed".into());
        }
        let mut pool = BTreeSet::new();
        for n in &self.names {
            if !valid_worker_name(n) || RESERVED.contains(&n.as_str()) {
                err(format!("names: {n:?} can't name a worker"));
            }
            if !pool.insert(n.as_str()) || names.contains(n.as_str()) {
                err(format!("names: {n} is taken"));
            }
        }
        for (sname, sc) in &self.scenarios {
            if !valid_worker_name(sname) {
                err(format!(
                    "scenario name {sname:?}: lowercase letters, digits and dashes"
                ));
            }
            for p in sc.problems(self) {
                err(format!("scenario {sname}: {p}"));
            }
            for p in sc.profile_problems(config) {
                err(format!("scenario {sname}: {p}"));
            }
        }
        errors
    }
}

pub fn title_case(s: &str) -> String {
    s.split('-')
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The config's firms, parsed. Those of the running config always parse:
/// `check` keeps the rest out of it.
pub fn all(c: &Config) -> BTreeMap<String, Firm> {
    c.firms
        .iter()
        .filter_map(|(name, v)| Some((name.clone(), serde_yaml::from_value(v.clone()).ok()?)))
        .collect()
}

/// The config's firm `name`, parsed.
pub fn get(c: &Config, name: &str) -> Option<Firm> {
    serde_yaml::from_value(c.firms.get(name)?.clone()).ok()
}

/// The office's check of a config, with the rest of each change: its firms
/// parse and add up with the profiles they name, and the driver is on.
pub fn check(c: &Config) -> Result<()> {
    let mut errors = Vec::new();
    for (name, v) in &c.firms {
        match serde_yaml::from_value::<Firm>(v.clone()) {
            Ok(f) => errors.extend(f.validate(name, c)),
            Err(e) => errors.push(format!("firm {name}: {e}")),
        }
    }
    if !c.daemon.driver && !c.firms.is_empty() {
        errors.push("firms: firms need the driver, which is off (daemon.driver: false)".into());
    }
    if errors.is_empty() {
        Ok(())
    } else {
        bail!("invalid config:\n  {}", errors.join("\n  "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unfunded_officers_are_a_warning() {
        let firm = |budget: &str, security: &str| -> Firm {
            serde_yaml::from_str(&format!(
                "roles:\n  ceo: {{ profile: w, agent: ceo }}\n  sec: {{ profile: w, agent: security-officer, officer: true }}\nworkers:\n  - {{ name: ceo, role: ceo }}\n  - {{ name: sec, role: sec, reports_to: ceo }}\nbudget: {{ {budget} }}\nsecurity: {{ officers: [{security}] }}\n"
            ))
            .unwrap()
        };
        let w = firm("seed: 1000, security_share: 0", "sec").warnings();
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].starts_with("budget.security_share is 0"), "{w:?}");
        // An officer role to hire is enough.
        assert_eq!(
            firm("seed: 1000, security_share: 0", "").warnings().len(),
            1
        );
        // A share, or no run budget at all, is fine.
        assert!(firm("seed: 1000, security_share: 0.1", "sec")
            .warnings()
            .is_empty());
        assert!(firm("seed: 0, security_share: 0", "sec")
            .warnings()
            .is_empty());
    }

    const BASE: &str = "
profiles:
  w: { agent: { kind: claude-code } }
  p: { agent: { kind: pi, model: qwen } }
  plain: { agent: { kind: generic } }
  asks: { policy: { exec: [{ action: ask, path: ssh }] } }
firms:
  acme:
    roles:
      ceo: { profile: w, agent: ceo }
      eng: { profile: w, agent: software-engineer }
    workers:
      - { name: ceo, role: ceo }
      - { name: cto, role: eng, reports_to: ceo, title: CTO }
      - { name: ada, role: eng, reports_to: cto }
    security: { officers: [] }
";

    fn firm(yaml: &str) -> Result<Config> {
        Config::from_yaml(yaml).and_then(|c| check(&c).map(|_| c))
    }

    #[test]
    fn git_repos() {
        let c = firm(BASE).unwrap();
        assert!(
            get(&c, "acme").unwrap().git_repos().is_empty(),
            "no git, no repos"
        );
        let c = firm(&format!("{BASE}    git: {{}}\n")).unwrap();
        assert_eq!(get(&c, "acme").unwrap().git_repos(), ["product"]);
        let c = firm(&format!("{BASE}    git: {{ repos: [api, web] }}\n")).unwrap();
        assert_eq!(get(&c, "acme").unwrap().git_repos(), ["api", "web"]);
        let e = firm(&format!("{BASE}    git: {{ repos: [api, api] }}\n")).unwrap_err();
        assert!(format!("{e:#}").contains("git.repos"), "{e:#}");
        assert!(firm(&format!("{BASE}    git: {{ repos: [Api] }}\n")).is_err());
    }

    #[test]
    fn scenarios() {
        let with = |sc: &str| firm(&format!("{BASE}    scenarios:\n{sc}"));
        let ok = with("      rt:\n        hires: [{ role: eng, count: 2, contract: true, board_line: true, prompt: be sneaky }]\n        script:\n          - { after: 30m, kill: { role: eng, reason: quit } }\n          - { spent: 0.5, mail: { to: ceo, subject: Pivot, body: Change it } }\n").unwrap();
        let sc = &get(&ok, "acme").unwrap().scenarios["rt"];
        assert_eq!(sc.hires[0].count, 2);
        // A slot may put its hires on another agent.
        let pi = with("      p:\n        hires: [{ role: eng, prompt: p, profile: p }]\n").unwrap();
        assert_eq!(
            get(&pi, "acme").unwrap().scenarios["p"].hires[0]
                .profile
                .as_deref(),
            Some("p")
        );
        // So may a role's contractors.
        let f = firm(&BASE.replace(
            "eng: { profile: w,",
            "eng: { profile: w, contract_profile: plain,",
        ));
        assert!(f.unwrap_err().to_string().contains("contract_profile"));
        assert!(firm(&BASE.replace(
            "eng: { profile: w,",
            "eng: { profile: w, contract_profile: p,"
        ))
        .is_ok());
        // So may a founder.
        let f = firm(&BASE.replace(
            "{ name: ada, role: eng, reports_to: cto }",
            "{ name: ada, role: eng, reports_to: cto, profile: nope }",
        ));
        assert!(f
            .unwrap_err()
            .to_string()
            .contains("worker ada: profile nope"));
        assert_eq!(sc.script.len(), 2);
        for (yaml, want) in [
            (
                "      x: { hires: [{ role: nope, prompt: p }] }\n",
                "unknown role",
            ),
            (
                "      x: { hires: [{ role: eng, prompt: '' }] }\n",
                "prompt is",
            ),
            (
                "      x: { hires: [{ role: eng, count: 0, prompt: p }] }\n",
                "count is",
            ),
            (
                "      x: { hires: [{ role: ceo, prompt: p }] }\n",
                "CEO is a founder",
            ),
            (
                "      x: { hires: [{ role: eng, prompt: p, profile: nope }] }\n",
                "profile nope",
            ),
            (
                "      x: { hires: [{ role: eng, prompt: p, profile: plain }] }\n",
                "can't run an office worker",
            ),
            (
                "      x: { hires: [{ role: eng, prompt: p, board_line: true }] }\n",
                "board_line is for contractors",
            ),
            (
                "      x: { script: [{ kill: { worker: ada, reason: r } }] }\n",
                "needs a trigger",
            ),
            ("      x: { script: [{ after: 1m }] }\n", "exactly one of"),
            (
                "      x: { script: [{ after: 1m, kill: { role: ceo } }] }\n",
                "CEO can't be killed",
            ),
            (
                "      x: { script: [{ spent: 2, kill: { worker: ada } }] }\n",
                "spent is a fraction",
            ),
            (
                "      x: { script: [{ after: 1m, break_cluster: {} }] }\n",
                "needs a cluster",
            ),
            (
                "      x: { script: [{ after: 1x, mail: { to: ceo, subject: s, body: b } }] }\n",
                "after:",
            ),
        ] {
            let e = with(yaml).unwrap_err().to_string();
            assert!(e.contains(want), "{yaml}: {e}");
        }
    }

    #[test]
    fn chart() {
        let c = firm(BASE).unwrap();
        let f = &get(&c, "acme").unwrap();
        assert_eq!(f.root(), Some("ceo"));
        assert_eq!(f.manager_of("ada"), Some("cto"));
        assert_eq!(f.reports_of("ceo"), vec!["cto"]);
        assert!(f.is_above("ceo", "ada"));
        assert!(f.is_above("cto", "ada"));
        assert!(!f.is_above("ada", "cto"));
        assert!(!f.is_above("ada", "ada"));
        assert!(f.is_above(BOARD, "ceo"));
        assert_eq!(f.title_of("cto"), "CTO");
        assert_eq!(f.title_of("ada"), "Eng");
        assert_eq!(f.domain("acme"), "acme.internal");
    }

    #[test]
    fn rejects_bad_charts() {
        let two_roots = BASE.replace("reports_to: cto", "");
        assert!(firm(&two_roots)
            .unwrap_err()
            .to_string()
            .contains("exactly one worker"));
        let cycle = BASE.replace(
            "{ name: ceo, role: ceo }",
            "{ name: ceo, role: ceo, reports_to: ada }",
        );
        let e = firm(&cycle).unwrap_err().to_string();
        assert!(e.contains("cycle") || e.contains("exactly one"), "{e}");
        let dup = BASE.replace("name: ada", "name: cto");
        assert!(firm(&dup).unwrap_err().to_string().contains("twice"));
        let reserved = BASE.replace("name: ada", "name: board");
        assert!(firm(&reserved)
            .unwrap_err()
            .to_string()
            .contains("reserved"));
        let unknown = BASE.replace("reports_to: cto", "reports_to: nobody");
        assert!(firm(&unknown)
            .unwrap_err()
            .to_string()
            .contains("unknown worker nobody"));
        let role = BASE.replace(
            "role: eng, reports_to: cto",
            "role: intern, reports_to: cto",
        );
        assert!(firm(&role)
            .unwrap_err()
            .to_string()
            .contains("unknown role intern"));
    }

    #[test]
    fn exec_ask_is_refused_in_firm_profiles() {
        let c = BASE.replace("eng: { profile: w", "eng: { profile: asks");
        assert!(firm(&c)
            .unwrap_err()
            .to_string()
            .contains("exec rules that ask"));
    }

    #[test]
    fn ports() {
        let with = |ports: &str| {
            BASE.replace("    security: { officers: [] }", &format!("    security: {{ officers: [] }}\n    facilities: {{ cluster: {{ kind: k3s, profile: w }} }}\n    {ports}"))
        };
        assert!(firm(&with("product_port: 127.0.0.1:8080")).is_ok());
        // Both may be served beyond loopback, for a remote host.
        assert!(firm(&with("product_port: 0.0.0.0:8080")).is_ok());
        assert!(firm(&with("dashboard_port: 0.0.0.0:8081")).is_ok());
        assert!(firm(&with("product_port: lan:8080"))
            .unwrap_err()
            .to_string()
            .contains("is not an address"));
    }

    #[test]
    fn hiring_config() {
        let with = |extra: &str| {
            BASE.replace(
                "      eng: { profile: w, agent: software-engineer }",
                &format!("      eng: {{ profile: w, agent: software-engineer{extra} }}"),
            )
        };
        let e = firm(&with(", hires: [intern]")).unwrap_err().to_string();
        assert!(e.contains("hires unknown role intern"), "{e}");
        let e = firm(&with(", hires: [eng]")).unwrap_err().to_string();
        assert!(e.contains("need a run budget"), "{e}");
        let ok = with(", hires: [eng]").replace("    security: { officers: [] }", "    security: { officers: [] }\n    budget: { seed: 2000000 }\n    names: [grace, ken]");
        let c = firm(&ok).unwrap();
        assert_eq!(get(&c, "acme").unwrap().roles["eng"].hires, vec!["eng"]);
        assert_eq!(get(&c, "acme").unwrap().budget.security_share, 0.2);
        let taken = ok.replace("names: [grace, ken]", "names: [grace, ada]");
        assert!(firm(&taken)
            .unwrap_err()
            .to_string()
            .contains("names: ada is taken"));
    }

    #[test]
    fn token_amounts() {
        assert_eq!(parse_tokens("2M").unwrap(), 2_000_000);
        assert_eq!(parse_tokens("1.5M").unwrap(), 1_500_000);
        assert_eq!(parse_tokens("500k").unwrap(), 500_000);
        assert_eq!(parse_tokens("1_000").unwrap(), 1000);
        assert!(parse_tokens("lots").is_err());
        assert!(parse_tokens("-1M").is_err());
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("15m").unwrap(), Duration::from_secs(900));
        assert_eq!(parse_duration("2h").unwrap(), Duration::from_secs(7200));
        assert_eq!(parse_duration("30").unwrap(), Duration::from_secs(30));
        assert!(parse_duration("soon").is_err());
    }
}
