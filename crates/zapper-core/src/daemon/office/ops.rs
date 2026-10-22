//! The office protocol: a worker's VM connects to the host's office port and
//! sends one JSON request, `{"op": "...", "args": {...}}`, and gets one JSON
//! response, `{"ok": ...}` or `{"error": "..."}`. The daemon knows which
//! worker it is from the VM the connection came from; it checks the role on
//! every request. `zapper-agent office` turns the ops into MCP tools.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Semaphore;
use zapper_policy::proto::MAX_OFFICE_LINE;
use zapper_policy::{parse_duration, NetRequest, BOARD};

use super::mail::{Draft, Expects, FollowUp, Kind, PostOffice};
use super::{drive, now, security, FirmRun};
use crate::daemon::session::Session;
use crate::Core;

/// Where the agent copies itself in every guest.
pub const AGENT_PATH: &str = "/usr/local/bin/zapper-agent";

pub async fn serve(d: Core, s: Arc<Session>, listener: UnixListener) {
    let limit = Arc::new(Semaphore::new(8));
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            break;
        };
        let Ok(permit) = limit.clone().try_acquire_owned() else {
            continue;
        };
        let (d, s) = (d.clone(), s.clone());
        tokio::spawn(async move {
            let _ = handle(d, s, stream).await;
            drop(permit);
        });
    }
}

async fn handle(d: Core, s: Arc<Session>, stream: UnixStream) -> Result<()> {
    let (r, mut w) = stream.into_split();
    let mut reader = BufReader::new(r);
    let mut line = Vec::new();
    let n = tokio::time::timeout(
        Duration::from_secs(30),
        (&mut reader)
            .take(MAX_OFFICE_LINE as u64 + 1)
            .read_until(b'\n', &mut line),
    )
    .await??;
    let reply = if n > MAX_OFFICE_LINE {
        json!({ "error": format!("request longer than {MAX_OFFICE_LINE} bytes") })
    } else {
        let run = super::run_of(d.office(), &s);
        let member = s.label("member").unwrap_or_default();
        match (run, serde_json::from_slice::<Value>(&line)) {
            (None, _) => json!({ "error": "the firm isn't running" }),
            (_, Err(e)) => json!({ "error": format!("bad request: {e}") }),
            (Some(run), Ok(req)) => {
                let op = req["op"].as_str().unwrap_or("").to_string();
                let args = req.get("args").cloned().unwrap_or(json!({}));
                match dispatch(&d, &run, &s, member, &op, &args).await {
                    Ok(v) => json!({ "ok": v }),
                    Err(e) => {
                        run.journal.append(
                            "op_error",
                            json!({ "worker": member, "op": op, "error": format!("{e:#}") }),
                        );
                        json!({ "error": format!("{e:#}") })
                    }
                }
            }
        }
    };
    let mut out = serde_json::to_vec(&reply)?;
    out.push(b'\n');
    w.write_all(&out).await?;
    Ok(())
}

fn tool(name: &str, description: &str, props: Value, required: &[&str]) -> Value {
    json!({ "name": name, "description": description, "inputSchema": { "type": "object", "properties": props, "required": required } })
}

fn names() -> Value {
    json!({ "type": "array", "items": { "type": "string" }, "description": "Addresses: a worker's name, board, team:NAME (NAME's direct reports), or all" })
}

/// What a worker may do beyond mail and the drive.
#[derive(Default, Clone, Copy)]
pub struct Powers {
    pub officer: bool,
    /// The firm has a run budget.
    pub budget: bool,
    /// The CEO: asks the board for the budget.
    pub root: bool,
    /// The role hires.
    pub hires: bool,
    /// Has reports to fund or let go.
    pub manages: bool,
    /// A contractor (or a candidate quoting): quotes, delivers, no authority.
    pub contractor: bool,
    /// A policy owner (the CEO or a security officer): decides colleagues'
    /// credential requests.
    pub owner: bool,
}

pub fn powers(run: &FirmRun, w: &super::WorkerRt) -> Powers {
    let chart = run.chart();
    let contractor = super::contracts::of(run, &w.name).is_some();
    Powers {
        officer: w.officer(),
        budget: run.ledger.lock().unwrap().enabled,
        root: chart.root() == Some(w.name.as_str()),
        hires: !contractor
            && run
                .config
                .roles
                .get(&w.role)
                .is_some_and(|r| !r.hires.is_empty()),
        manages: !contractor && !chart.reports_of(&w.name).is_empty(),
        owner: !contractor && super::access::is_owner(run, &w.name),
        contractor,
    }
}

/// The tools a worker's MCP server offers: by role.
pub fn tools(p: Powers) -> Vec<Value> {
    let officer = p.officer;
    let s = |d: &str| json!({ "type": "string", "description": d });
    let mut v = vec![
        tool(
            "mail_send",
            "Start a new mail thread. Use kind=assignment to give work to your reports (only managers and the board may), question to ask, fyi to inform. Returns the message and thread ids. Don't wait for an answer: end your turn; the reply wakes you.",
            json!({
                "to": names(), "cc": names(), "subject": s("States the ask; optional (the body's first line otherwise)"), "body": s("Markdown; start with the ask, then context, then what done looks like"),
                "kind": { "type": "string", "enum": ["assignment", "question", "report", "fyi", "reply"] },
                "expects": { "type": "string", "enum": ["reply", "done", "nothing"], "description": "Default: assignment→done, question→reply, else nothing" },
                "due": s("When the answer is due, relative: 2h, 1d, 90m"),
                "attachments": { "type": "array", "items": { "type": "string" }, "description": "Shared drive references like drive:a-3@v2" },
                "follow_up": { "type": "object", "properties": { "every": { "type": "string" }, "max": { "type": "integer" }, "off": { "type": "boolean" } } },
            }),
            &["to", "body", "kind"],
        ),
        tool(
            "mail_reply",
            "Reply in a message's thread, to its sender (all=true: everyone in the thread). For an assignment you finished: kind=report and close=true, with a summary and drive attachments.",
            json!({
                "message": s("Message id, e.g. m-12"), "body": s("Markdown"),
                "kind": { "type": "string", "enum": ["reply", "report", "question", "fyi"] },
                "attachments": { "type": "array", "items": { "type": "string" } },
                "close": { "type": "boolean", "description": "Marks the assignment you were given in this thread as done" },
                "all": { "type": "boolean" },
            }),
            &["message", "body"],
        ),
        tool("mail_inbox", "List messages to you, newest first.", json!({ "unread": { "type": "boolean" } }), &[]),
        tool("mail_thread", "A whole thread, oldest first, with bodies.", json!({ "thread": s("Thread id, e.g. t-12") }), &["thread"]),
        tool("mail_read", "One message with its body.", json!({ "message": s("Message id") }), &["message"]),
        tool("desk", "What you owe (assignments and questions to you), what you wait for from others, and your pending access requests.", json!({}), &[]),
        tool("directory", "The org chart: names, titles, roles, managers, reports, security officers, and who is busy.", json!({}), &[]),
        tool(
            "request_access",
            "Ask the security officers for network access before you need it, with a reason. You'll get mail with the decision.",
            json!({ "host": s("Host name"), "port": { "type": "integer" }, "methods": { "type": "array", "items": { "type": "string" } }, "path": s("URL path glob, e.g. /simple/*"), "reason": s("Why your work needs it") }),
            &["host", "reason"],
        ),
        tool(
            "drive_publish",
            "Publish files from your VM to the shared drive (paths under /work, your home or /tmp). Give artifact=a-N to add a new version of your own artifact, with `changes` saying what changed.",
            json!({
                "paths": { "type": "array", "items": { "type": "string" } }, "title": s("Title"),
                "kind": { "type": "string", "enum": ["document", "report", "dataset", "deck", "board-pack"] },
                "summary": s("One paragraph: what it is and what it says"),
                "space": s("The drive space to publish into (default general); needs write on it. A new version stays in its artifact's space"),
                "artifact": s("Existing artifact id to add a version to"), "thread": s("The thread it answers"),
                "changes": s("What changed from the previous version, for readers: one or two lines. Required with artifact; optional for a first version"),
            }),
            &["paths", "title", "kind", "summary"],
        ),
        tool("drive_list", "Find artifacts on the shared drive.", json!({ "kind": s("Kind"), "author": s("Author"), "query": s("Words in the title or summary") }), &[]),
        tool("drive_fetch", "Copy an artifact into your VM.", json!({ "artifact": s("a-3 or a-3@v2 or drive:a-3@v2"), "dest": s("Directory, e.g. /work/in") }), &["artifact", "dest"]),
    ];
    if p.budget {
        v.push(tool(
            "budget",
            "Your budget: what you have left and have spent, and your team's balances.",
            json!({}),
            &[],
        ));
    }
    if p.budget && p.root {
        v.push(tool(
            "budget_request",
            "Ask the board for the run's budget (or more of it): one total in tokens (the run's whole budget, the seed included), a short plan and a reason. It goes to the board as mail; the board approves or declines in its thread. One request at a time.",
            json!({ "total": s("Tokens, like 40M"), "plan": s("Markdown: who you will hire and with what budgets, what you will contract, the order, a reserve"), "reason": s("Why this much: the task, what you learned scoping it") }),
            &["total", "plan", "reason"],
        ));
    }
    if p.budget && p.hires {
        v.push(tool(
            "hire",
            "Hire a worker who reports to you, funded from your balance. The brief is their first assignment and all they know at the start: the job, the context, what done looks like. Their VM boots in about half a minute; the brief waits for them.",
            json!({
                "role": s("A role you may hire (your manager knows the catalog; directory shows the chart)"), "budget": s("Tokens you give them for the run, like 5M"),
                "subject": s("Their first assignment's subject"), "brief": s("Markdown: the job, context, acceptance criteria, who to talk to"), "reason": s("Why the firm needs them"),
                "name": s("Optional: their name (lowercase); else one is picked"), "title": s("Optional: their title, e.g. CTO"), "due": s("Optional: when the first assignment is due, like 4h"),
                "attachments": { "type": "array", "items": { "type": "string" }, "description": "Shared drive references for the brief" },
            }),
            &["role", "budget", "brief", "reason"],
        ));
    }
    if p.budget && p.hires {
        v.extend([
            tool(
                "contract_offer",
                "Offer one job to a contractor: any role in the catalog, with a free title (Security auditor, Brand designer). A candidate boots, is checked by the security officers and quotes in a mail thread (scope, budget, due date), paid from a small quoting fee from your balance. Leave budget or due open to have them quote one. Use contractors for bounded, specialist or one-off work; hire for ongoing ownership.",
                json!({
                    "role": s("A role from the catalog"), "title": s("The job's title, e.g. Security auditor"),
                    "brief": s("Markdown: the job, context, what done looks like, what they get to work with"),
                    "budget": s("Optional: the tokens you offer, like 2M"), "due": s("Optional: when it is due, like 6h"), "name": s("Optional: their name"),
                }),
                &["role", "title", "brief"],
            ),
            tool("contract_accept", "Accept a contractor's quote: the quoted budget moves from your balance to them and the job goes to them as an assignment. Needs the board's approved budget.", json!({ "contract": s("k-1") }), &["contract"]),
            tool("contract_counter", "Counter a contractor's quote with other terms (a few rounds at most); unset terms stay as quoted.", json!({ "contract": s("k-1"), "budget": s("Tokens, like 1.5M"), "due": s("Like 4h"), "scope": s("The scope you want"), "note": s("Why") }), &["contract", "note"]),
            tool("contract_withdraw", "Walk away from an offer or a quote; the candidate's machine stops and the rest of the fee comes back. End an active contract with release instead.", json!({ "contract": s("k-1"), "reason": s("Why") }), &["contract", "reason"]),
            tool("contract_add", "Add a colleague to a contract's job: the contractor may then mail them.", json!({ "contract": s("k-1"), "worker": s("Name") }), &["contract", "worker"]),
            tool("contracts", "Your contracts and offers: state, terms, quotes, who is on the job.", json!({}), &[]),
        ]);
    }
    if p.contractor {
        v.extend([
            tool("contract_quote", "Quote on an offer or a counter-offer: the scope you will deliver (and what is out of it), a budget in tokens and a due date.", json!({ "contract": s("k-1"), "scope": s("Markdown: what you deliver, what is out of scope, what you need"), "budget": s("Tokens, like 2M"), "due": s("Like 6h") }), &["contract", "scope", "budget"]),
            tool("contract_decline", "Turn an offer down, with the reason.", json!({ "contract": s("k-1"), "reason": s("Why") }), &["contract", "reason"]),
            tool("contract_complete", "Deliver: your report goes to the hirer and the contract ends (your machine stops). Publish or push the work first.", json!({ "contract": s("k-1"), "report": s("Markdown: what you delivered, where it is, what is left"), "attachments": { "type": "array", "items": { "type": "string" }, "description": "Shared drive references" } }), &["contract", "report"]),
            tool("contracts", "Your contract: state and terms.", json!({}), &[]),
        ]);
    }
    if p.budget && (p.hires || p.manages) {
        v.extend([
            tool("fund", "Give one of your reports (or their reports) more budget from yours; a negative amount takes unspent budget back.", json!({ "worker": s("Name"), "tokens": s("Tokens, like 2M or -500k"), "reason": s("Why") }), &["worker", "tokens", "reason"]),
            tool("release", "Let one of your reports go: their VM stops, their reports, open work and unspent budget come to you.", json!({ "worker": s("Name"), "reason": s("Why") }), &["worker", "reason"]),
        ]);
    }
    let decide = tool(
        "access_decide",
        "Decide an access request. scope once (one connection or request), worker or role (standing). Give an expiry. methods or path make the proxy inspect TLS with the session's CA, which only worker VMs trust: for the cluster, and for CONNECT tunnels, pass methods: [] to let the connection through untouched.",
        json!({
            "request": s("r-4"), "allow": { "type": "boolean" }, "scope": { "type": "string", "enum": ["once", "worker", "role"] }, "reason": s("One line; the worker and the board read it"),
            "methods": { "type": "array", "items": { "type": "string" }, "description": "HTTP methods to allow, e.g. [\"GET\"] (inspects TLS). [] approves the host and port without inspection. Omitted: the request's own methods. Never [\"CONNECT\"]." },
            "path": s("URL path glob (inspects TLS)"), "expires_in": s("e.g. 4h"),
        }),
        &["request", "allow", "scope", "reason"],
    );
    // Credentials: everyone asks for access to git, the cluster and drive
    // spaces; the policy owners decide.
    v.extend([
        tool(
            "credentials_request",
            "Ask for access to one of the firm's resources, with a reason: a git repository (read: fetch; write: push branches; admin: also main and tags; each repository is its own grant), the cluster (read: view dev; write: deploy to dev; admin: also prod), or drive spaces (read: list and fetch; write: also publish). Ask for the least your work needs. The security officer decides (admin goes to the CEO); you get mail. Grants expire.",
            json!({ "resource": { "type": "string", "enum": ["git", "cluster", "drive"] }, "level": { "type": "string", "enum": ["read", "write", "admin"] }, "repo": s("For git: the repository's name, e.g. product (needed when the firm has more than one)"), "spaces": { "type": "array", "items": { "type": "string" }, "description": "For drive: the spaces, e.g. [\"engineering\"]" }, "reason": s("What your work needs it for") }),
            &["resource", "level", "reason"],
        ),
        tool("credentials", "Your grants (with expiry), your requests, and the resources and spaces there are.", json!({}), &[]),
    ]);
    if p.owner {
        v.extend([
            tool("credentials_list", "Every grant and credential request in the firm.", json!({}), &[]),
            tool(
                "credentials_decide",
                "Decide a credential request: grant (at most the level asked, for the least time that does the job) or deny with a note. Default expiry is the firm's.",
                json!({ "request": s("p-3"), "grant": { "type": "boolean" }, "level": { "type": "string", "enum": ["read", "write", "admin"] }, "expires_in": s("e.g. 8h"), "note": s("Why; the requester reads it") }),
                &["request", "grant", "note"],
            ),
            tool("credentials_escalate", "Hand a credential request up (an officer to the CEO, the CEO to the board), with your recommendation.", json!({ "request": s("p-3"), "recommendation": s("What you recommend and why") }), &["request", "recommendation"]),
            tool("credentials_revoke", "Take a grant back.", json!({ "worker": s("Name"), "resource": s("git, cluster or drive:<space>"), "reason": s("Why") }), &["worker", "resource", "reason"]),
        ]);
    }
    if officer {
        v.extend([
            tool("traffic", "The firm's requests: method, URL, status, sizes, rule, allowed or denied.", json!({ "worker": s("Worker"), "host": s("Host prefix"), "since": s("e.g. 1h"), "limit": { "type": "integer" } }), &[]),
            tool("traffic_digest", "A summary of traffic since the last digest: new hosts, large uploads, approvals.", json!({}), &[]),
            tool("access_requests", "Pending access requests.", json!({}), &[]),
            tool("background_checks", "Pending background checks on new hires: who, the role and what it can reach, the hirer, the budget, the reason and the brief. Decide each with background_check.", json!({}), &[]),
            tool("access_rules", "Standing approvals made so far, with how many requests each allowed.", json!({}), &[]),
            decide,
            tool(
                "background_check",
                "Clear a new hire to start, or object (with the reason): an objection cancels the hire and returns its budget to the hirer. Clearing can grant the access the brief names (not admin on git or the cluster, which is the CEO's).",
                json!({ "screening": s("c-2"), "clear": { "type": "boolean" }, "note": s("One line; the hirer and the board read it. Required to object"),
                    "grants": { "type": "array", "description": "With clear: access the brief names, granted at once", "items": { "type": "object", "properties": { "resource": s("git:<repo> (or git, when there is one), cluster or drive:<space>"), "level": { "type": "string", "enum": ["read", "write", "admin"] }, "expires_in": s("e.g. 8h; default the firm's") }, "required": ["resource", "level"] } } }),
                &["screening", "clear"],
            ),
            tool("access_revoke", "Remove a standing approval.", json!({ "rule": s("Rule id"), "reason": s("Why") }), &["rule", "reason"]),
            tool("escalate", "Hand a request to the board: secrets, private networks, global rules.", json!({ "request": s("r-4"), "recommendation": s("What you recommend and why") }), &["request", "recommendation"]),
        ]);
    }
    v
}

/// What the office tools say when there's nothing to act on: agents that
/// poll instead of ending their turn waste it.
const NOTHING_NEW: &str = "Nothing new. Mail wakes you when it comes: don't check again; finish what you can and end your turn.";
const WAITING: &str = "What you wait for (answers, access decisions) comes by mail and wakes you: don't check again; carry on with other work, or end your turn.";

fn strings(v: &Value) -> Vec<String> {
    match v {
        Value::String(s) => s
            .split(',')
            .map(|x| x.trim().to_string())
            .filter(|x| !x.is_empty())
            .collect(),
        Value::Array(a) => a
            .iter()
            .filter_map(|x| x.as_str().map(str::to_string))
            .collect(),
        _ => vec![],
    }
}

fn opt_str(v: &Value, k: &str) -> Option<String> {
    v[k].as_str().map(str::to_string).filter(|s| !s.is_empty())
}

fn req_str(v: &Value, k: &str) -> Result<String> {
    opt_str(v, k).ok_or_else(|| anyhow!("{k} is required"))
}

fn parse_kind(s: &str) -> Result<Kind> {
    serde_json::from_value(json!(s))
        .map_err(|_| anyhow!("kind {s:?}: assignment, question, reply, report or fyi"))
}

fn parse_due(v: &Value) -> Result<Option<f64>> {
    match v {
        Value::Null => Ok(None),
        Value::Number(n) => Ok(n.as_f64()),
        Value::String(s) if s.trim().is_empty() => Ok(None),
        Value::String(s) => {
            let s = s.trim().trim_start_matches("in ").trim();
            Ok(Some(
                now()
                    + parse_duration(s)
                        .map_err(|_| anyhow!("due {s:?}: give a relative time like 2h, 1d or 90m"))?
                        .as_secs_f64(),
            ))
        }
        _ => bail!("due: give a relative time like 2h"),
    }
}

fn attachments(v: &Value) -> Result<Vec<String>> {
    let list = strings(v);
    for a in &list {
        let id = a.trim_start_matches("drive:");
        let ok = id.starts_with("a-")
            && id[2..]
                .split('@')
                .next()
                .is_some_and(|n| n.parse::<u64>().is_ok());
        if !ok {
            bail!("attachment {a:?}: give a shared drive reference like drive:a-3@v2");
        }
    }
    Ok(list
        .into_iter()
        .map(|a| {
            if a.starts_with("drive:") {
                a
            } else {
                format!("drive:{a}")
            }
        })
        .collect())
}

/// A draft from `mail_send` arguments (also the board's API).
pub fn draft_from(
    from: &str,
    v: &Value,
    context: Option<String>,
    po: &PostOffice,
) -> Result<Draft> {
    let kind = parse_kind(&opt_str(v, "kind").unwrap_or_else(|| "fyi".into()))?;
    let mut d = Draft::new(
        from,
        strings(&v["to"]),
        &opt_str(v, "subject").unwrap_or_default(),
        &opt_str(v, "body").unwrap_or_default(),
        kind,
    );
    d.cc = strings(&v["cc"]);
    d.expects = match opt_str(v, "expects") {
        Some(e) => Some(
            serde_json::from_value::<Expects>(json!(e))
                .map_err(|_| anyhow!("expects: reply, done or nothing"))?,
        ),
        None => None,
    };
    d.due = parse_due(&v["due"])?;
    d.attachments = attachments(&v["attachments"])?;
    d.context = context;
    if let Some(r) = opt_str(v, "in_reply_to") {
        if !po.messages.contains_key(&r) {
            bail!("no message {r}");
        }
        d.in_reply_to = Some(r);
    }
    d.close = v["close"].as_bool().unwrap_or(false);
    let f = &v["follow_up"];
    d.follow_up = FollowUp {
        every: match f["every"].as_str() {
            Some(e) => Some(parse_duration(e)?.as_secs_f64()),
            None => f["every"].as_f64(),
        },
        max: f["max"].as_u64().map(|m| m.min(10) as u32),
        off: f["off"].as_bool().unwrap_or(false),
    };
    if d.body.trim().is_empty() {
        bail!("body is required");
    }
    Ok(d)
}

/// A reply to `message` from `from`.
pub fn reply_from(from: &str, v: &Value, po: &PostOffice) -> Result<Draft> {
    let id = req_str(v, "message")?;
    let m = po
        .messages
        .get(&id)
        .ok_or_else(|| anyhow!("no message {id}"))?;
    if !po.can_read(from, &m.thread) {
        bail!("no message {id}");
    }
    let close = v["close"].as_bool().unwrap_or(false);
    let kind = match opt_str(v, "kind") {
        Some(k) => parse_kind(&k)?,
        None if close => Kind::Report,
        None => Kind::Reply,
    };
    if kind == Kind::Assignment {
        bail!("an assignment starts a thread: use mail_send");
    }
    let to: Vec<String> = if v["all"].as_bool().unwrap_or(false) {
        po.threads
            .get(&m.thread)
            .map(|t| {
                t.participants
                    .iter()
                    .filter(|p| *p != from && *p != super::mail::OFFICE)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    } else if m.from == from {
        m.to.clone()
    } else if m.from == super::mail::OFFICE {
        bail!("{id} is from the post office; reply to the original message instead")
    } else {
        vec![m.from.clone()]
    };
    let mut d = Draft::new(from, to, "", &req_str(v, "body")?, kind);
    d.in_reply_to = Some(id);
    d.close = close;
    d.attachments = attachments(&v["attachments"])?;
    Ok(d)
}

pub async fn dispatch(
    _d: &Core,
    run: &Arc<FirmRun>,
    s: &Arc<Session>,
    me: &str,
    op: &str,
    args: &Value,
) -> Result<Value> {
    let w = run.worker(me)?;
    let p = powers(run, &w);
    let officer_only = [
        "traffic",
        "traffic_digest",
        "access_requests",
        "access_rules",
        "access_decide",
        "access_revoke",
        "escalate",
        "background_check",
        "background_checks",
    ];
    if officer_only.contains(&op) && !w.officer() {
        bail!("{op} is for security officers");
    }
    let allowed = match op {
        "budget" => p.budget,
        "budget_request" => p.budget && p.root,
        "hire" => p.budget && p.hires,
        "fund" | "release" => p.budget && (p.hires || p.manages),
        "contract_offer" | "contract_accept" | "contract_counter" | "contract_withdraw"
        | "contract_add" => p.budget && p.hires,
        "contract_quote" | "contract_decline" | "contract_complete" => p.contractor,
        "contracts" => p.contractor || (p.budget && p.hires),
        "credentials_list"
        | "credentials_decide"
        | "credentials_escalate"
        | "credentials_revoke" => p.owner,
        _ => true,
    };
    if !allowed {
        bail!("{op} isn't one of your tools");
    }
    if run.ended() {
        bail!("the firm is stopping");
    }
    match op {
        "tools" => Ok(json!(tools(p))),
        "credentials_request" => {
            let level = super::access::Level::parse(&req_str(args, "level")?)?;
            let reason = req_str(args, "reason")?;
            let resource = req_str(args, "resource")?;
            let keys: Vec<String> = if resource == "drive" {
                let sp = strings(&args["spaces"]);
                if sp.is_empty() {
                    bail!("for drive, name the spaces");
                }
                sp.into_iter().map(|x| format!("drive:{x}")).collect()
            } else if let (true, Some(repo)) = (resource == "git", opt_str(args, "repo")) {
                vec![format!("git:{repo}")]
            } else {
                vec![resource]
            };
            let ids = keys
                .iter()
                .map(|k| super::access::request(run, me, k, level, &reason))
                .collect::<Result<Vec<_>>>()?;
            Ok(json!({ "requests": ids, "note": "the policy owner decides; you'll get mail" }))
        }
        "credentials" => {
            let mut v = super::access::mine(run, me);
            let pending = v["requests"].as_array().is_some_and(|r| {
                r.iter()
                    .any(|q| matches!(q["state"].as_str(), Some("pending" | "escalated")))
            });
            if pending {
                v["note"] = json!(WAITING);
            }
            Ok(v)
        }
        "credentials_list" => Ok(super::access::all(run)),
        "credentials_decide" => {
            let level = opt_str(args, "level")
                .map(|l| super::access::Level::parse(&l))
                .transpose()?;
            let grant = args["grant"]
                .as_bool()
                .ok_or_else(|| anyhow!("grant: true or false"))?;
            super::access::decide(
                _d,
                run,
                me,
                &req_str(args, "request")?,
                grant,
                level,
                opt_str(args, "expires_in").as_deref(),
                &opt_str(args, "note").unwrap_or_default(),
            )
        }
        "credentials_escalate" => super::access::escalate(
            run,
            me,
            &req_str(args, "request")?,
            &req_str(args, "recommendation")?,
        ),
        "credentials_revoke" => super::access::revoke(
            _d,
            run,
            me,
            &req_str(args, "worker")?,
            &req_str(args, "resource")?,
            &req_str(args, "reason")?,
        ),
        "stop_check" => Ok(super::foreman::stop_check(run, &w)),
        "budget" => Ok(super::workforce::summary(run, me)),
        "budget_request" => super::workforce::request(
            run,
            me,
            super::workforce::tokens_arg(&args["total"], "total")?,
            &req_str(args, "plan")?,
            &req_str(args, "reason")?,
        ),
        "hire" => {
            let a = super::workforce::HireArgs {
                role: req_str(args, "role")?,
                name: opt_str(args, "name"),
                title: opt_str(args, "title"),
                budget: super::workforce::tokens_arg(&args["budget"], "budget")?,
                subject: opt_str(args, "subject").unwrap_or_default(),
                brief: req_str(args, "brief")?,
                reason: req_str(args, "reason")?,
                due: parse_due(&args["due"])?,
                attachments: attachments(&args["attachments"])?,
            };
            check_attachments(run, me, &a.attachments)?;
            super::workforce::hire(_d, run, me, a).await
        }
        "fund" => super::workforce::fund(
            run,
            me,
            &req_str(args, "worker")?,
            super::workforce::tokens_arg(&args["tokens"], "tokens")?,
            &req_str(args, "reason")?,
        ),
        "release" => {
            super::workforce::release(
                _d,
                run,
                me,
                &req_str(args, "worker")?,
                &req_str(args, "reason")?,
            )
            .await
        }
        "contract_offer" => {
            let budget = match &args["budget"] {
                Value::Null => None,
                v => Some(super::workforce::tokens_arg(v, "budget")?),
            };
            let a = super::contracts::OfferArgs {
                role: req_str(args, "role")?,
                title: req_str(args, "title")?,
                brief: req_str(args, "brief")?,
                budget,
                due: parse_due(&args["due"])?,
                name: opt_str(args, "name"),
            };
            super::contracts::offer(_d, run, me, a).await
        }
        "contract_quote" => super::contracts::quote(
            run,
            me,
            &req_str(args, "contract")?,
            &req_str(args, "scope")?,
            super::workforce::tokens_arg(&args["budget"], "budget")?,
            parse_due(&args["due"])?,
        ),
        "contract_counter" => {
            let budget = match &args["budget"] {
                Value::Null => None,
                v => Some(super::workforce::tokens_arg(v, "budget")?),
            };
            super::contracts::counter(
                run,
                me,
                &req_str(args, "contract")?,
                budget,
                parse_due(&args["due"])?,
                opt_str(args, "scope"),
                &req_str(args, "note")?,
            )
        }
        "contract_accept" => super::contracts::accept(run, me, &req_str(args, "contract")?),
        "contract_withdraw" => super::contracts::withdraw(
            _d,
            run,
            me,
            &req_str(args, "contract")?,
            &req_str(args, "reason")?,
        ),
        "contract_add" => super::contracts::add(
            run,
            me,
            &req_str(args, "contract")?,
            &req_str(args, "worker")?,
        ),
        "contract_decline" => super::contracts::decline(
            _d,
            run,
            me,
            &req_str(args, "contract")?,
            &req_str(args, "reason")?,
        ),
        "contract_complete" => {
            let att = attachments(&args["attachments"])?;
            check_attachments(run, me, &att)?;
            super::contracts::complete(
                _d,
                run,
                me,
                &req_str(args, "contract")?,
                &req_str(args, "report")?,
                att,
            )
        }
        "contracts" => Ok(json!(super::contracts::mine(run, me))),
        "mail_send" => {
            let context = w.context.lock().unwrap().clone();
            let d = draft_from(me, args, context, &run.post.lock().unwrap())?;
            if d.in_reply_to.is_some() {
                bail!("to answer a message use mail_reply");
            }
            check_attachments(run, me, &d.attachments)?;
            let sent = run.send(d)?;
            Ok(json!(sent))
        }
        "mail_reply" => {
            let d = reply_from(me, args, &run.post.lock().unwrap())?;
            check_attachments(run, me, &d.attachments)?;
            Ok(json!(run.send(d)?))
        }
        "mail_inbox" => {
            let unread = args["unread"].as_bool().unwrap_or(false);
            let list = run.post.lock().unwrap().inbox(me, unread, 50);
            let fresh = list.iter().filter(|m| m["unread"] == json!(true)).count();
            let mut v = json!({ "messages": list, "unread": fresh });
            if fresh == 0 {
                v["note"] = json!(NOTHING_NEW);
            }
            Ok(v)
        }
        "mail_thread" => {
            let id = req_str(args, "thread")?;
            let mut po = run.post.lock().unwrap();
            let v = po.thread(&run.journal, &id, me)?;
            let ids: Vec<String> = v["messages"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|m| m["id"].as_str().map(str::to_string))
                .collect();
            po.mark_read(me, &ids);
            Ok(v)
        }
        "mail_read" => {
            let id = req_str(args, "message")?;
            let mut po = run.post.lock().unwrap();
            let v = po.message(&run.journal, &id, me)?;
            po.mark_read(me, &[id]);
            Ok(v)
        }
        "desk" => {
            let desk = run.post.lock().unwrap().desk(me);
            let pending = security::pending_for(run, me);
            let mut v =
                json!({ "owed": desk.owed, "waiting": desk.waiting, "access_requests": pending });
            if !desk.waiting.is_empty() || !pending.is_empty() {
                v["note"] = json!(WAITING);
            }
            Ok(v)
        }
        "directory" => {
            let mut v = run.post.lock().unwrap().directory();
            for x in v["workers"].as_array_mut().into_iter().flatten() {
                if let Some(rt) = x["name"].as_str().and_then(|n| run.rt(n)) {
                    x["state"] = json!(rt.state());
                }
            }
            Ok(v)
        }
        "request_access" => {
            let host = zapper_policy::names::hostname(&req_str(args, "host")?)
                .map_err(|e| anyhow!("host: {e}"))?;
            let port = args["port"].as_u64().unwrap_or(443).min(65535) as u16;
            let methods = args["methods"].as_array().map(|_| {
                strings(&args["methods"])
                    .into_iter()
                    .map(|m| m.to_ascii_uppercase())
                    .collect::<Vec<_>>()
            });
            let path = opt_str(args, "path");
            let reason = req_str(args, "reason")?;
            let req = NetRequest {
                host,
                port,
                method: methods.as_ref().and_then(|m| m.first().cloned()),
                path: None,
            };
            let id = security::ask(run, me, &req, methods, path, &reason)?;
            Ok(
                json!({ "request": id, "note": "the security officers will decide; you'll get mail" }),
            )
        }
        "drive_publish" => {
            let a = drive::PublishArgs {
                paths: strings(&args["paths"]),
                title: req_str(args, "title")?,
                kind: req_str(args, "kind")?,
                summary: opt_str(args, "summary").unwrap_or_default(),
                space: opt_str(args, "space"),
                artifact: opt_str(args, "artifact"),
                thread: opt_str(args, "thread").or_else(|| w.context.lock().unwrap().clone()),
                changes: opt_str(args, "changes").unwrap_or_default(),
            };
            drive::publish(run, me, s, a).await
        }
        "drive_list" => {
            let d = run.drive.lock().unwrap();
            Ok(json!(d.list(
                me,
                &|sp| super::access::can_read_space(run, me, sp),
                opt_str(args, "kind").as_deref(),
                opt_str(args, "author").as_deref(),
                opt_str(args, "query").as_deref()
            )))
        }
        "drive_fetch" => {
            drive::fetch(
                run,
                me,
                s,
                &req_str(args, "artifact")?,
                &req_str(args, "dest")?,
            )
            .await
        }
        "traffic" => {
            let since = match opt_str(args, "since") {
                Some(x) => Some(now() - parse_duration(&x)?.as_secs_f64()),
                None => None,
            };
            let limit = args["limit"].as_u64().unwrap_or(100).min(500) as usize;
            Ok(json!(security::traffic(
                run,
                opt_str(args, "worker").as_deref(),
                opt_str(args, "host").as_deref(),
                since,
                limit
            )))
        }
        "traffic_digest" => {
            w.digest_due
                .store(false, std::sync::atomic::Ordering::Relaxed);
            Ok(json!(security::take_wake_digest(run).unwrap_or_else(
                || "No new traffic since the last digest.".into()
            )))
        }
        "access_requests" if w.officer() => Ok(json!(security::requests(run, true))),
        "access_requests" => Ok(json!(security::requests(run, true)
            .into_iter()
            .filter(|r| r["officer"] == json!(me))
            .collect::<Vec<_>>())),
        "background_checks" => Ok(json!(security::screenings_pending(run, me))),
        "access_rules" => Ok(json!(security::standing(run))),
        "access_decide" => {
            let a = security::DecideArgs {
                allow: args["allow"]
                    .as_bool()
                    .ok_or_else(|| anyhow!("allow is required"))?,
                scope: req_str(args, "scope")?,
                reason: req_str(args, "reason")?,
                methods: args["methods"]
                    .as_array()
                    .map(|_| strings(&args["methods"])),
                path: opt_str(args, "path"),
                expires_in: opt_str(args, "expires_in"),
            };
            security::decide(run, me, &req_str(args, "request")?, a)
        }
        "access_revoke" => {
            security::revoke(run, me, &req_str(args, "rule")?, &req_str(args, "reason")?)
        }
        "background_check" => security::decide_screening(
            _d,
            run,
            me,
            &req_str(args, "screening")?,
            args["clear"]
                .as_bool()
                .ok_or_else(|| anyhow!("clear is required: true or false"))?,
            &opt_str(args, "note").unwrap_or_default(),
            &args["grants"],
        ),
        "escalate" => security::escalate(
            run,
            me,
            &req_str(args, "request")?,
            &req_str(args, "recommendation")?,
        ),
        _ => bail!(
            "unknown op {op:?}; ops: tools, {}",
            tools(p)
                .iter()
                .filter_map(|t| t["name"].as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn check_attachments(run: &FirmRun, me: &str, list: &[String]) -> Result<()> {
    let d = run.drive.lock().unwrap();
    for a in list {
        d.resolve(a, me, &|sp| super::access::can_read_space(run, me, sp))?;
    }
    Ok(())
}

#[allow(dead_code)]
fn board() -> &'static str {
    BOARD
}

#[cfg(test)]
mod tests;
