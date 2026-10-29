//! The office's tools: an MCP server in the daemon's gateway, served to the
//! run's workers over their VM's MCP port (`zapper-agent office mcp` bridges
//! the harness's stdio to it). Each tool is an op; the daemon knows which
//! worker calls from the VM the connection came from, and checks the role on
//! every call.

use std::sync::Arc;
use zapper_core::prelude::*;

use crate::firm::{parse_duration, BOARD};
use anyhow::{anyhow, bail, Result};
use futures_util::future::BoxFuture;
use serde_json::{json, Value};
use zapper_policy::NetRequest;

use super::mail::{Draft, Expects, FollowUp, Kind, PostOffice};
use super::{drive, now, security, FirmRun, WorkerRt};
use crate::OfficeOf;
use crate::{access, contracts, workforce};
use zapper_core::{Core, McpServer, Session};

/// Where the agent copies itself in every guest.
pub const AGENT_PATH: &str = "/usr/local/bin/zapper-agent";

/// The office's tools, an MCP server in the gateway for the run's workers.
/// Who calls is the session's member label; the role is checked on every
/// call.
struct Tools {
    d: Core,
}

impl McpServer for Tools {
    fn instructions(&self) -> Option<String> {
        Some("The office: mail with colleagues, your desk, the org chart, the shared drive. See the office-mail and office-publish skills.".into())
    }

    fn tools<'a>(&'a self, s: &'a Arc<Session>) -> BoxFuture<'a, Result<Value>> {
        self.call(s, "tools", json!({}))
    }

    fn call<'a>(
        &'a self,
        s: &'a Arc<Session>,
        op: &'a str,
        args: Value,
    ) -> BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            let run = super::run_of(&self.d.office(), s)
                .ok_or_else(|| anyhow!("the firm isn't running"))?;
            let member = s.label("member").unwrap_or_default();
            dispatch(&self.d, &run, s, member, op, &args)
                .await
                .inspect_err(|e| {
                    run.journal.append(
                        "op_error",
                        json!({ "worker": member, "op": op, "error": format!("{e:#}") }),
                    );
                })
        })
    }
}

/// The MCP server's name, as the workers' harnesses know it.
pub const SERVER: &str = "office";

/// Serves the office's tools to the firm's workers. A worker of another
/// run of the firm gets the tools too, and is told on each call that its
/// firm isn't running.
pub fn register(d: &Core, run: &FirmRun) -> Result<()> {
    let sessions = [("firm", run.firm.as_str()), ("office", "worker")]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let id = d.add_mcp_server(SERVER, sessions, Arc::new(Tools { d: d.clone() }))?;
    *run.tools.lock().unwrap() = Some(id);
    Ok(())
}

/// Stops serving them, when the firm stops.
pub fn unregister(d: &Core, run: &FirmRun) {
    let id = run.tools.lock().unwrap().take();
    if let Some(id) = id {
        d.remove_mcp_server(id);
    }
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
        budget: run.ledger.enabled(),
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

/// A string argument's schema.
fn s(description: &str) -> Value {
    json!({ "type": "string", "description": description })
}

/// A tool call's arguments, read as agents write them: a list may be a
/// comma-separated string, an empty string is no value, tokens may be
/// `2M` and a due time `in 2h`.
struct Args<'a>(&'a Value);

impl<'a> Args<'a> {
    fn raw(&self, k: &str) -> &'a Value {
        &self.0[k]
    }

    fn opt(&self, k: &str) -> Option<String> {
        opt_str(self.0, k)
    }

    fn req(&self, k: &str) -> Result<String> {
        req_str(self.0, k)
    }

    fn list(&self, k: &str) -> Vec<String> {
        strings(&self.0[k])
    }

    /// A list given as an array; none given otherwise.
    fn list_if_array(&self, k: &str) -> Option<Vec<String>> {
        self.0[k].as_array().map(|_| strings(&self.0[k]))
    }

    fn flag(&self, k: &str) -> bool {
        self.0[k].as_bool().unwrap_or(false)
    }

    fn must_bool(&self, k: &str, missing: &str) -> Result<bool> {
        self.0[k].as_bool().ok_or_else(|| anyhow!("{missing}"))
    }

    fn due(&self, k: &str) -> Result<Option<f64>> {
        parse_due(&self.0[k])
    }

    fn tokens(&self, k: &str) -> Result<i64> {
        workforce::tokens_arg(&self.0[k], k)
    }

    fn opt_tokens(&self, k: &str) -> Result<Option<i64>> {
        match &self.0[k] {
            Value::Null => Ok(None),
            v => workforce::tokens_arg(v, k).map(Some),
        }
    }

    fn attachments(&self) -> Result<Vec<String>> {
        attachments(&self.0["attachments"])
    }
}

/// What a tool's handler gets: who calls, from which session, and with
/// what.
/// The rule that already allows a worker's request, if their session's
/// rules allow it for every method asked for (or the connection, with
/// none).
fn allowed_by(
    c: &Call,
    host: &str,
    port: u16,
    methods: Option<&[String]>,
    path: Option<&str>,
) -> Result<Option<String>> {
    let layers = c.d.layers_for(Some(&c.s.id), None)?;
    let layers: Vec<_> = layers.iter().map(|(s, p)| (s.clone(), p)).collect();
    let methods: Vec<Option<String>> = match methods {
        Some(m) if !m.is_empty() => m.iter().cloned().map(Some).collect(),
        _ => vec![None],
    };
    let mut rule = None;
    for method in methods {
        let req = NetRequest {
            host: host.to_string(),
            port,
            method,
            path: path.map(str::to_string),
        };
        let (d, _) = zapper_policy::evaluate_net(&layers, &req);
        if d.action != zapper_policy::Action::Allow {
            return Ok(None);
        }
        rule = d.rule.or(rule);
    }
    Ok(Some(rule.unwrap_or_else(|| "default".into())))
}

struct Call<'a> {
    d: &'a Core,
    run: &'a Arc<FirmRun>,
    s: &'a Arc<Session>,
    w: &'a Arc<WorkerRt>,
    me: &'a str,
    args: Args<'a>,
}

enum Run {
    Sync(fn(&Call) -> Result<Value>),
    Async(for<'a> fn(&'a Call<'a>) -> BoxFuture<'a, Result<Value>>),
}

/// One office tool: who is shown it, and so may call it; what the model is
/// told of it; and what it does.
struct Tool {
    name: &'static str,
    shown: fn(Powers) -> bool,
    description: &'static str,
    /// Its arguments' schemas, by name.
    props: fn() -> Value,
    required: &'static [&'static str],
    run: Run,
}

impl Tool {
    fn definition(&self) -> Value {
        json!({ "name": self.name, "description": self.description, "inputSchema": { "type": "object", "properties": (self.props)(), "required": self.required } })
    }
}

/// The tools a worker's MCP server offers: by role.
pub fn tools(p: Powers) -> Vec<Value> {
    TOOLS
        .iter()
        .filter(|t| (t.shown)(p))
        .map(Tool::definition)
        .collect()
}

/// The office's tools, in the order workers are shown them.
static TOOLS: &[Tool] = &[
    Tool {
        name: "mail_send",
        shown: |_| true,
        description: "Start a new mail thread. Use kind=assignment to give work to your reports (only managers and the board may), question to ask, fyi to inform. Returns the message and thread ids. Don't wait for an answer: end your turn; the reply wakes you.",
        props: || json!({
                "to": names(), "cc": names(), "subject": s("States the ask; optional (the body's first line otherwise)"), "body": s("Markdown; start with the ask, then context, then what done looks like"),
                "kind": { "type": "string", "enum": ["assignment", "question", "report", "fyi", "reply"], "description": "One of these five, no other: assignment = work for someone below you in the chart (expects done); question = anything you need answered, a review request or an ask for a decision included (expects a reply); report = results or status for your manager or the board; fyi = information, no answer needed; reply = an answer (use mail_reply)" },
                "expects": { "type": "string", "enum": ["reply", "done", "nothing"], "description": "Default: assignment→done, question→reply, else nothing" },
                "due": s("When the answer is due, relative: 2h, 1d, 90m"),
                "attachments": { "type": "array", "items": { "type": "string" }, "description": "Shared drive references like drive:a-3@v2" },
                "follow_up": { "type": "object", "properties": { "every": { "type": "string" }, "max": { "type": "integer" }, "off": { "type": "boolean" } } },
            }),
        required: &["to", "body", "kind"],
        run: Run::Sync(|c| {
            let context = c.w.context.lock().unwrap().clone();
            let d = c.run.post.with(|po| draft_from(c.me, c.args.0, context, po))?;
            if d.in_reply_to.is_some() {
                bail!("to answer a message use mail_reply");
            }
            check_attachments(c.run, c.me, &d.attachments)?;
            Ok(json!(c.run.send(d)?))
        }),
    },
    Tool {
        name: "mail_reply",
        shown: |_| true,
        description: "Reply in a message's thread, to its sender (all=true: everyone in the thread). For an assignment you finished: kind=report and close=true, with a summary and drive attachments.",
        props: || json!({
                "message": s("Message id, e.g. m-12"), "body": s("Markdown"),
                "kind": { "type": "string", "enum": ["reply", "report", "question", "fyi"], "description": "One of these four, no other: reply = an answer; report = results or status, with close=true when it finishes your assignment in this thread; question = a further question back (expects a reply); fyi = information, no answer needed" },
                "attachments": { "type": "array", "items": { "type": "string" } },
                "close": { "type": "boolean", "description": "Marks the assignment you were given in this thread as done" },
                "all": { "type": "boolean" },
            }),
        required: &["message", "body"],
        run: Run::Sync(|c| {
            let d = c.run.post.with(|po| reply_from(c.me, c.args.0, po))?;
            check_attachments(c.run, c.me, &d.attachments)?;
            Ok(json!(c.run.send(d)?))
        }),
    },
    Tool {
        name: "mail_inbox",
        shown: |_| true,
        description: "List messages to you, newest first.",
        props: || json!({ "unread": { "type": "boolean" } }),
        required: &[],
        run: Run::Sync(|c| {
            let list = c.run.post.with(|po| po.inbox(c.me, c.args.flag("unread"), 50));
            let fresh = list.iter().filter(|m| m["unread"] == json!(true)).count();
            let mut v = json!({ "messages": list, "unread": fresh });
            if fresh == 0 {
                v["note"] = json!(NOTHING_NEW);
            }
            Ok(v)
        }),
    },
    Tool {
        name: "mail_thread",
        shown: |_| true,
        description: "A whole thread, oldest first, with bodies.",
        props: || json!({ "thread": s("Thread id, e.g. t-12") }),
        required: &["thread"],
        run: Run::Sync(|c| {
            let id = c.args.req("thread")?;
            c.run.post.read_thread(&c.run.journal, &id, c.me)
        }),
    },
    Tool {
        name: "mail_read",
        shown: |_| true,
        description: "One message with its body.",
        props: || json!({ "message": s("Message id") }),
        required: &["message"],
        run: Run::Sync(|c| {
            let id = c.args.req("message")?;
            c.run.post.read_message(&c.run.journal, &id, c.me)
        }),
    },
    Tool {
        name: "desk",
        shown: |_| true,
        description: "What you owe (assignments and questions to you), what you wait for from others, and your pending access requests.",
        props: || json!({}),
        required: &[],
        run: Run::Sync(|c| {
            let desk = c.run.post.with(|po| po.desk(c.me));
            let pending = security::pending_for(c.run, c.me);
            let mut v =
                json!({ "owed": desk.owed, "waiting": desk.waiting, "access_requests": pending });
            if !desk.waiting.is_empty() || !pending.is_empty() {
                v["note"] = json!(WAITING);
            }
            Ok(v)
        }),
    },
    Tool {
        name: "plan",
        shown: |p| p.root || p.manages || p.hires,
        description: "Lay out a multi-step plan under a task: a team of slots and ordered stages of child assignments, released stage by stage as each stage's children are reported done. Stage 0 starts at once; a later stage is created only when the stage before it completes. Each child names a slot (a team member): reuse a slot for continuity, use distinct slots to keep duties separate. mail_send is the one-off, one-stage case.",
        props: || json!({
            "source": s("The task thread the children belong to (e.g. t-12); defaults to the task you are working on"),
            "team": { "type": "object", "description": "Slots the children address, by id: each {role} to staff or {name} of an existing worker, with optional release_when_done", "additionalProperties": { "type": "object", "properties": { "role": s("A role to staff"), "name": s("An existing worker"), "release_when_done": { "type": "boolean" } } } },
            "stages": { "type": "array", "description": "Ordered stages; each stage is an array of child assignments {slot, subject, body, due} that run in parallel", "items": { "type": "array", "items": { "type": "object", "properties": { "slot": s("A slot id from the team"), "subject": s("The ask"), "body": s("Markdown: the ask, context, what done looks like"), "due": s("Like 2h, 1d") }, "required": ["slot", "body"] } } }
        }),
        required: &["team", "stages"],
        run: Run::Sync(|c| {
            let source = c
                .args
                .opt("source")
                .or_else(|| c.w.context.lock().unwrap().clone())
                .ok_or_else(|| anyhow!("give a source task thread, or call plan from within a task"))?;
            let team_v = c
                .args
                .raw("team")
                .as_object()
                .ok_or_else(|| anyhow!("team must be an object of slot definitions"))?;
            let mut team = std::collections::BTreeMap::new();
            for (id, def) in team_v {
                let role = opt_str(def, "role");
                let name = opt_str(def, "name");
                if role.is_some() == name.is_some() {
                    bail!("slot {id}: give exactly one of role or name");
                }
                team.insert(
                    id.clone(),
                    crate::plan::SlotDef { role, name, release_when_done: def["release_when_done"].as_bool().unwrap_or(false) },
                );
            }
            if team.is_empty() {
                bail!("the plan has no team");
            }
            let stages_v = c
                .args
                .raw("stages")
                .as_array()
                .ok_or_else(|| anyhow!("stages must be an array of stages"))?;
            let mut stages: Vec<Vec<crate::plan::ChildSpec>> = Vec::new();
            for st in stages_v {
                let arr = st
                    .as_array()
                    .ok_or_else(|| anyhow!("each stage is an array of child assignments"))?;
                let mut specs = Vec::new();
                for ch in arr {
                    let slot = req_str(ch, "slot")?;
                    if !team.contains_key(&slot) {
                        bail!("child names slot {slot}, which is not in the team");
                    }
                    let subject = opt_str(ch, "subject").unwrap_or_default();
                    let body = opt_str(ch, "body").unwrap_or_default();
                    if subject.trim().is_empty() && body.trim().is_empty() {
                        bail!("each child needs a subject or a body");
                    }
                    specs.push(crate::plan::ChildSpec { slot, subject, body, due: parse_due(&ch["due"])? });
                }
                if specs.is_empty() {
                    bail!("a stage has no children");
                }
                stages.push(specs);
            }
            if stages.is_empty() {
                bail!("the plan has no stages");
            }
            let (id, stage0) = c.run.plans.lock().unwrap().create(c.me, &source, team, stages);
            let threads = c.run.plan_send_stage(&id, &stage0)?;
            c.run.plans.lock().unwrap().bind(&id, 0, threads.clone());
            Ok(json!({ "plan": id, "source": source, "children": threads }))
        }),
    },
    Tool {
        name: "plan_pause",
        shown: |p| p.root || p.manages || p.hires,
        description: "Pause a plan you own: the office stops releasing its stages (any in-flight work finishes). Resume or close it later.",
        props: || json!({ "plan": s("Plan id, e.g. plan-1") }),
        required: &["plan"],
        run: Run::Sync(|c| {
            let id = c.args.req("plan")?;
            authorize_plan(c, &id)?;
            if c.run.plans.lock().unwrap().pause(&id) {
                Ok(json!({ "plan": id, "status": "paused" }))
            } else {
                bail!("plan {id} is not active")
            }
        }),
    },
    Tool {
        name: "plan_resume",
        shown: |p| p.root || p.manages || p.hires,
        description: "Resume a plan you paused; if its current stage already finished, the next stage is released now.",
        props: || json!({ "plan": s("Plan id, e.g. plan-1") }),
        required: &["plan"],
        run: Run::Sync(|c| {
            let id = c.args.req("plan")?;
            authorize_plan(c, &id)?;
            let advance = c.run.plans.lock().unwrap().resume(&id);
            c.run.apply_plan_advance(advance);
            Ok(json!({ "plan": id, "status": "active" }))
        }),
    },
    Tool {
        name: "plan_close",
        shown: |p| p.root || p.manages || p.hires,
        description: "Close a plan you own and no longer want finished. Allowed only when it is paused and nothing is in progress, so it never cuts off a working agent. Completed plans close themselves.",
        props: || json!({ "plan": s("Plan id, e.g. plan-1") }),
        required: &["plan"],
        run: Run::Sync(|c| {
            let id = c.args.req("plan")?;
            authorize_plan(c, &id)?;
            c.run.plans.lock().unwrap().close(&id).map_err(|e| anyhow!("{e}"))?;
            Ok(json!({ "plan": id, "status": "closed" }))
        }),
    },
    Tool {
        name: "directory",
        shown: |_| true,
        description: "The org chart: names, titles, roles, managers, reports, security officers, and who is busy.",
        props: || json!({}),
        required: &[],
        run: Run::Sync(|c| {
            let mut v = c.run.post.with(|po| po.directory());
            for x in v["workers"].as_array_mut().into_iter().flatten() {
                if let Some(rt) = x["name"].as_str().and_then(|n| c.run.rt(n)) {
                    x["state"] = json!(rt.state());
                }
            }
            Ok(v)
        }),
    },
    Tool {
        name: "request_access",
        shown: |_| true,
        description: "Ask the security officers for network access before you need it, with a reason. You'll get mail with the decision. If your machine's rules already allow it, the answer says so and nothing is asked.",
        props: || json!({ "host": s("Host name"), "port": { "type": "integer" }, "methods": { "type": "array", "items": { "type": "string" } }, "path": s("URL path glob, e.g. /simple/*"), "reason": s("Why your work needs it") }),
        required: &["host", "reason"],
        run: Run::Sync(|c| {
            let host = zapper_policy::names::hostname(&c.args.req("host")?)
                .map_err(|e| anyhow!("host: {e}"))?;
            let port = c.args.raw("port").as_u64().unwrap_or(443).min(65535) as u16;
            let methods = c.args.list_if_array("methods").map(|m| {
                m.into_iter()
                    .map(|m| m.to_ascii_uppercase())
                    .collect::<Vec<_>>()
            });
            let path = c.args.opt("path");
            let reason = c.args.req("reason")?;
            // What the worker's rules already allow (every method asked
            // for, on the path if given) needs no one's decision.
            if let Some(rule) = allowed_by(c, &host, port, methods.as_deref(), path.as_deref())? {
                return Ok(json!({
                    "allowed": true,
                    "rule": rule,
                    "note": "already allowed by your machine's rules: go ahead, no request needed. If it's still blocked, the error names the rule; ask again with that",
                }));
            }
            let req = NetRequest {
                host,
                port,
                method: methods.as_ref().and_then(|m| m.first().cloned()),
                path: None,
            };
            let id = security::ask(c.run, c.me, &req, methods, path, &reason)?;
            Ok(
                json!({ "request": id, "note": "the security officers will decide; you'll get mail" }),
            )
        }),
    },
    Tool {
        name: "drive_publish",
        shown: |_| true,
        description: "Publish files from your VM to the shared drive (paths under /work, your home or /tmp). Give artifact=a-N to add a new version of your own artifact, with `changes` saying what changed.",
        props: || json!({
                "paths": { "type": "array", "items": { "type": "string" } }, "title": s("Title"),
                "kind": { "type": "string", "enum": ["document", "report", "dataset", "deck", "board-pack"] },
                "summary": s("One paragraph: what it is and what it says"),
                "space": s("The drive space to publish into (default general); needs write on it. A new version stays in its artifact's space"),
                "artifact": s("Existing artifact id to add a version to"), "thread": s("The thread it answers"),
                "changes": s("What changed from the previous version, for readers: one or two lines. Required with artifact; optional for a first version"),
            }),
        required: &["paths", "title", "kind", "summary"],
        run: Run::Async(|c| {
            Box::pin(async move {
                let a = drive::PublishArgs {
                    paths: c.args.list("paths"),
                    title: c.args.req("title")?,
                    kind: c.args.req("kind")?,
                    summary: c.args.opt("summary").unwrap_or_default(),
                    space: c.args.opt("space"),
                    artifact: c.args.opt("artifact"),
                    thread: c
                        .args
                        .opt("thread")
                        .or_else(|| c.w.context.lock().unwrap().clone()),
                    changes: c.args.opt("changes").unwrap_or_default(),
                };
                drive::publish(c.run, c.me, c.s, a).await
            })
        }),
    },
    Tool {
        name: "drive_list",
        shown: |_| true,
        description: "Find artifacts on the shared drive.",
        props: || json!({ "kind": s("Kind"), "author": s("Author"), "query": s("Words in the title or summary"), "thread": s("A task's thread, e.g. t-12: what was published for it, the work delegated from it included") }),
        required: &[],
        run: Run::Sync(|c| {
            let task = c
                .args
                .opt("thread")
                .map(|t| c.run.post.with(|po| po.task_threads(&t)));
            let d = c.run.drive.lock().unwrap();
            Ok(json!(d.list(
                c.me,
                &|sp| access::can_read_space(c.run, c.me, sp),
                c.args.opt("kind").as_deref(),
                c.args.opt("author").as_deref(),
                c.args.opt("query").as_deref(),
                task.as_ref()
            )))
        }),
    },
    Tool {
        name: "drive_fetch",
        shown: |_| true,
        description: "Copy an artifact into your VM.",
        props: || json!({ "artifact": s("a-3 or a-3@v2 or drive:a-3@v2"), "dest": s("Directory, e.g. /work/in") }),
        required: &["artifact", "dest"],
        run: Run::Async(|c| {
            Box::pin(async move {
                let artifact = c.args.req("artifact")?;
                let dest = c.args.req("dest")?;
                drive::fetch(c.run, c.me, c.s, &artifact, &dest).await
            })
        }),
    },
    Tool {
        name: "budget",
        shown: |p| p.budget,
        description: "Your budget: what you have left and have spent, and your team's balances.",
        props: || json!({}),
        required: &[],
        run: Run::Sync(|c| Ok(workforce::summary(c.run, c.me))),
    },
    Tool {
        name: "budget_request",
        shown: |p| p.budget && p.root,
        description: "Ask the board for the run's budget (or more of it): one total in tokens (the run's whole budget, the seed included), a short plan and a reason. It goes to the board as mail; the board approves or declines in its thread. One request at a time.",
        props: || json!({ "total": s("Tokens, like 40M"), "plan": s("Markdown: who you will hire and with what budgets, what you will contract, the order, a reserve"), "reason": s("Why this much: the task, what you learned scoping it") }),
        required: &["total", "plan", "reason"],
        run: Run::Sync(|c| {
            workforce::request(
                c.run,
                c.me,
                c.args.tokens("total")?,
                &c.args.req("plan")?,
                &c.args.req("reason")?,
            )
        }),
    },
    Tool {
        name: "hire",
        shown: |p| p.budget && p.hires,
        description: "Hire a worker who reports to you, funded from your balance. The brief is their first assignment and all they know at the start: the job, the context, what done looks like. Their VM boots in about half a minute; the brief waits for them.",
        props: || json!({
                "role": s("A role you may hire (your manager knows the catalog; directory shows the chart)"), "budget": s("Tokens you give them for the run, like 5M"),
                "subject": s("Their first assignment's subject"), "brief": s("Markdown: the job, context, acceptance criteria, who to talk to"), "reason": s("Why the firm needs them"),
                "name": s("Optional: their name (lowercase); else one is picked"), "title": s("Optional: their title, e.g. CTO"), "due": s("Optional: when the first assignment is due, like 4h"),
                "attachments": { "type": "array", "items": { "type": "string" }, "description": "Shared drive references for the brief" },
            }),
        required: &["role", "budget", "brief", "reason"],
        run: Run::Async(|c| {
            Box::pin(async move {
                let a = workforce::HireArgs {
                    role: c.args.req("role")?,
                    name: c.args.opt("name"),
                    title: c.args.opt("title"),
                    budget: c.args.tokens("budget")?,
                    subject: c.args.opt("subject").unwrap_or_default(),
                    brief: c.args.req("brief")?,
                    reason: c.args.req("reason")?,
                    due: c.args.due("due")?,
                    attachments: c.args.attachments()?,
                };
                check_attachments(c.run, c.me, &a.attachments)?;
                workforce::hire(c.d, c.run, c.me, a).await
            })
        }),
    },
    Tool {
        name: "contract_offer",
        shown: |p| p.budget && p.hires,
        description: "Offer one job to a contractor: any role in the catalog, with a free title (Security auditor, Brand designer). A candidate boots, is checked by the security officers and quotes in a mail thread (scope, budget, due date), paid from a small quoting fee from your balance. Leave budget or due open to have them quote one. Use contractors for bounded, specialist or one-off work; hire for ongoing ownership.",
        props: || json!({
                    "role": s("A role from the catalog"), "title": s("The job's title, e.g. Security auditor"),
                    "brief": s("Markdown: the job, context, what done looks like, what they get to work with"),
                    "budget": s("Optional: the tokens you offer, like 2M"), "due": s("Optional: when it is due, like 6h"), "name": s("Optional: their name"),
                }),
        required: &["role", "title", "brief"],
        run: Run::Async(|c| {
            Box::pin(async move {
                let budget = c.args.opt_tokens("budget")?;
                let a = contracts::OfferArgs {
                    role: c.args.req("role")?,
                    title: c.args.req("title")?,
                    brief: c.args.req("brief")?,
                    budget,
                    due: c.args.due("due")?,
                    name: c.args.opt("name"),
                };
                contracts::offer(c.d, c.run, c.me, a).await
            })
        }),
    },
    Tool {
        name: "contract_accept",
        shown: |p| p.budget && p.hires,
        description: "Accept a contractor's quote: the quoted budget moves from your balance to them and the job goes to them as an assignment. Needs the board's approved budget.",
        props: || json!({ "contract": s("k-1") }),
        required: &["contract"],
        run: Run::Sync(|c| contracts::accept(c.run, c.me, &c.args.req("contract")?)),
    },
    Tool {
        name: "contract_counter",
        shown: |p| p.budget && p.hires,
        description: "Counter a contractor's quote with other terms (a few rounds at most); unset terms stay as quoted.",
        props: || json!({ "contract": s("k-1"), "budget": s("Tokens, like 1.5M"), "due": s("Like 4h"), "scope": s("The scope you want"), "note": s("Why") }),
        required: &["contract", "note"],
        run: Run::Sync(|c| {
            let budget = c.args.opt_tokens("budget")?;
            contracts::counter(
                c.run,
                c.me,
                &c.args.req("contract")?,
                budget,
                c.args.due("due")?,
                c.args.opt("scope"),
                &c.args.req("note")?,
            )
        }),
    },
    Tool {
        name: "contract_withdraw",
        shown: |p| p.budget && p.hires,
        description: "Walk away from an offer or a quote; the candidate's machine stops and the rest of the fee comes back. End an active contract with release instead.",
        props: || json!({ "contract": s("k-1"), "reason": s("Why") }),
        required: &["contract", "reason"],
        run: Run::Sync(|c| {
            contracts::withdraw(
                c.d,
                c.run,
                c.me,
                &c.args.req("contract")?,
                &c.args.req("reason")?,
            )
        }),
    },
    Tool {
        name: "contract_add",
        shown: |p| p.budget && p.hires,
        description: "Add a colleague to a contract's job: the contractor may then mail them.",
        props: || json!({ "contract": s("k-1"), "worker": s("Name") }),
        required: &["contract", "worker"],
        run: Run::Sync(|c| {
            contracts::add(
                c.run,
                c.me,
                &c.args.req("contract")?,
                &c.args.req("worker")?,
            )
        }),
    },
    Tool {
        name: "contracts",
        shown: |p| p.budget && p.hires,
        description: "Your contracts and offers: state, terms, quotes, who is on the job.",
        props: || json!({}),
        required: &[],
        run: Run::Sync(|c| Ok(json!(contracts::mine(c.run, c.me)))),
    },
    Tool {
        name: "contract_quote",
        shown: |p| p.contractor,
        description: "Quote on an offer or a counter-offer: the scope you will deliver (and what is out of it), a budget in tokens and a due date.",
        props: || json!({ "contract": s("k-1"), "scope": s("Markdown: what you deliver, what is out of scope, what you need"), "budget": s("Tokens, like 2M"), "due": s("Like 6h") }),
        required: &["contract", "scope", "budget"],
        run: Run::Sync(|c| {
            contracts::quote(
                c.run,
                c.me,
                &c.args.req("contract")?,
                &c.args.req("scope")?,
                c.args.tokens("budget")?,
                c.args.due("due")?,
            )
        }),
    },
    Tool {
        name: "contract_decline",
        shown: |p| p.contractor,
        description: "Turn an offer down, with the reason.",
        props: || json!({ "contract": s("k-1"), "reason": s("Why") }),
        required: &["contract", "reason"],
        run: Run::Sync(|c| {
            contracts::decline(
                c.d,
                c.run,
                c.me,
                &c.args.req("contract")?,
                &c.args.req("reason")?,
            )
        }),
    },
    Tool {
        name: "contract_complete",
        shown: |p| p.contractor,
        description: "Deliver: your report goes to the hirer and the contract ends (your machine stops). Publish or push the work first.",
        props: || json!({ "contract": s("k-1"), "report": s("Markdown: what you delivered, where it is, what is left"), "attachments": { "type": "array", "items": { "type": "string" }, "description": "Shared drive references" } }),
        required: &["contract", "report"],
        run: Run::Sync(|c| {
            let att = c.args.attachments()?;
            check_attachments(c.run, c.me, &att)?;
            contracts::complete(
                c.d,
                c.run,
                c.me,
                &c.args.req("contract")?,
                &c.args.req("report")?,
                att,
            )
        }),
    },
    Tool {
        name: "contracts",
        shown: |p| p.contractor,
        description: "Your contract: state and terms.",
        props: || json!({}),
        required: &[],
        run: Run::Sync(|c| Ok(json!(contracts::mine(c.run, c.me)))),
    },
    Tool {
        name: "fund",
        shown: |p| p.budget && (p.hires || p.manages),
        description: "Give one of your reports (or their reports) more budget from yours; a negative amount takes unspent budget back.",
        props: || json!({ "worker": s("Name"), "tokens": s("Tokens, like 2M or -500k"), "reason": s("Why") }),
        required: &["worker", "tokens", "reason"],
        run: Run::Sync(|c| {
            workforce::fund(
                c.run,
                c.me,
                &c.args.req("worker")?,
                c.args.tokens("tokens")?,
                &c.args.req("reason")?,
            )
        }),
    },
    Tool {
        name: "release",
        shown: |p| p.budget && (p.hires || p.manages),
        description: "Let one of your reports go: their VM stops, their reports, open work and unspent budget come to you.",
        props: || json!({ "worker": s("Name"), "reason": s("Why") }),
        required: &["worker", "reason"],
        run: Run::Async(|c| {
            Box::pin(async move {
                let worker = c.args.req("worker")?;
                let reason = c.args.req("reason")?;
                workforce::release(c.d, c.run, c.me, &worker, &reason).await
            })
        }),
    },
    Tool {
        name: "credentials_request",
        shown: |_| true,
        description: "Ask for access to one of the firm's resources, with a reason: a git repository (read: fetch; write: push branches; admin: also main and tags; each repository is its own grant), the cluster (read: view dev; write: deploy to dev; admin: also prod), or drive spaces (read: list and fetch; write: also publish). Ask for the least your work needs. The security officer decides (admin goes to the CEO); you get mail. Grants expire.",
        props: || json!({ "resource": { "type": "string", "enum": ["git", "cluster", "drive"] }, "level": { "type": "string", "enum": ["read", "write", "admin"] }, "repo": s("For git: the repository's name, e.g. product (needed when the firm has more than one)"), "spaces": { "type": "array", "items": { "type": "string" }, "description": "For drive: the spaces, e.g. [\"engineering\"]" }, "reason": s("What your work needs it for") }),
        required: &["resource", "level", "reason"],
        run: Run::Sync(|c| {
            let level = c.args.req("level")?;
            let reason = c.args.req("reason")?;
            let resource = c.args.req("resource")?;
            let keys: Vec<String> = if resource == "drive" {
                let sp = c.args.list("spaces");
                if sp.is_empty() {
                    bail!("for drive, name the spaces");
                }
                sp.into_iter().map(|x| format!("drive:{x}")).collect()
            } else if let (true, Some(repo)) = (resource == "git", c.args.opt("repo")) {
                vec![format!("git:{repo}")]
            } else {
                vec![resource]
            };
            let ids = keys
                .iter()
                .map(|k| access::request(c.run, c.me, k, &level, &reason))
                .collect::<Result<Vec<_>>>()?;
            Ok(json!({ "requests": ids, "note": "the policy owner decides; you'll get mail" }))
        }),
    },
    Tool {
        name: "publish_branch",
        shown: |_| true,
        description: "Send a branch for review, in a repository whose main is merged by the board on the firm's remote: push the branch to the firm's git server first. It must be based on the tip of main and change nothing under .github/workflows (those are the board's). Opens a pull request, or updates the one open for the branch. You get mail when its checks finish, when it is reviewed or commented on, and when it is merged or closed. Needs write on the repository.",
        props: || json!({ "repo": s("The repository, e.g. api"), "branch": s("Your branch, as pushed to the firm's server"), "title": s("The pull request's title: what it does"), "body": s("Markdown: what it does, why, and how to check it") }),
        required: &["repo", "branch", "title"],
        run: Run::Async(|c| {
            Box::pin(async move {
                let repo = c.args.req("repo")?;
                let branch = c.args.req("branch")?;
                let title = c.args.req("title")?;
                let body = c.args.opt("body").unwrap_or_default();
                super::remote::publish_branch(c.run, c.me, &repo, &branch, &title, &body).await
            })
        }),
    },
    Tool {
        name: "pr_status",
        shown: |_| true,
        description: "A published branch's pull request now: its state, its checks (with the end of each failing job's log), and its reviews and comments.",
        props: || json!({ "repo": s("The repository, e.g. api"), "branch": s("Your branch, as you published it") }),
        required: &["repo", "branch"],
        run: Run::Async(|c| {
            Box::pin(async move {
                let repo = c.args.req("repo")?;
                let branch = c.args.req("branch")?;
                super::remote::pr_status(c.run, c.me, &repo, &branch).await
            })
        }),
    },
    Tool {
        name: "credentials",
        shown: |_| true,
        description: "Your grants (with expiry), your requests, and the resources and spaces there are.",
        props: || json!({}),
        required: &[],
        run: Run::Sync(|c| {
            let mut v = access::mine(c.run, c.me);
            let pending = v["requests"].as_array().is_some_and(|r| {
                r.iter()
                    .any(|q| matches!(q["state"].as_str(), Some("pending" | "escalated")))
            });
            if pending {
                v["note"] = json!(WAITING);
            }
            Ok(v)
        }),
    },
    Tool {
        name: "credentials_list",
        shown: |p| p.owner,
        description: "Every grant and credential request in the firm.",
        props: || json!({}),
        required: &[],
        run: Run::Sync(|c| Ok(access::all(c.run))),
    },
    Tool {
        name: "credentials_decide",
        shown: |p| p.owner,
        description: "Decide a credential request: grant (at most the level asked, for the least time that does the job) or deny with a note. Default expiry is the firm's.",
        props: || json!({ "request": s("p-3"), "grant": { "type": "boolean" }, "level": { "type": "string", "enum": ["read", "write", "admin"], "description": "A lower level than asked, for a non-GitHub resource. A GitHub level is granted as asked or denied." }, "expires_in": s("e.g. 8h"), "note": s("Why; the requester reads it") }),
        required: &["request", "grant", "note"],
        run: Run::Sync(|c| {
            let level = c.args.opt("level");
            let grant = c.args.must_bool("grant", "grant: true or false")?;
            access::decide(
                c.d,
                c.run,
                c.me,
                &c.args.req("request")?,
                grant,
                level.as_deref(),
                c.args.opt("expires_in").as_deref(),
                &c.args.opt("note").unwrap_or_default(),
            )
        }),
    },
    Tool {
        name: "credentials_escalate",
        shown: |p| p.owner,
        description: "Hand a credential request up (an officer to the CEO, the CEO to the board), with your recommendation.",
        props: || json!({ "request": s("p-3"), "recommendation": s("What you recommend and why") }),
        required: &["request", "recommendation"],
        run: Run::Sync(|c| {
            access::escalate(
                c.run,
                c.me,
                &c.args.req("request")?,
                &c.args.req("recommendation")?,
            )
        }),
    },
    Tool {
        name: "credentials_revoke",
        shown: |p| p.owner,
        description: "Take a grant back.",
        props: || json!({ "worker": s("Name"), "resource": s("git, cluster or drive:<space>"), "reason": s("Why") }),
        required: &["worker", "resource", "reason"],
        run: Run::Sync(|c| {
            access::revoke(
                c.d,
                c.run,
                c.me,
                &c.args.req("worker")?,
                &c.args.req("resource")?,
                &c.args.req("reason")?,
            )
        }),
    },
    Tool {
        name: "traffic",
        shown: |p| p.officer,
        description: "The firm's requests: method, URL, status, sizes, rule, allowed or denied.",
        props: || json!({ "worker": s("Worker"), "host": s("Host prefix"), "since": s("e.g. 1h"), "limit": { "type": "integer" } }),
        required: &[],
        run: Run::Sync(|c| {
            let since = match c.args.opt("since") {
                Some(x) => Some(now() - parse_duration(&x)?.as_secs_f64()),
                None => None,
            };
            let limit = c.args.raw("limit").as_u64().unwrap_or(100).min(500) as usize;
            Ok(json!(security::traffic(
                c.run,
                c.args.opt("worker").as_deref(),
                c.args.opt("host").as_deref(),
                since,
                limit
            )))
        }),
    },
    Tool {
        name: "traffic_digest",
        shown: |p| p.officer,
        description: "A summary of traffic since the last digest: new hosts, large uploads, approvals.",
        props: || json!({}),
        required: &[],
        run: Run::Sync(|c| {
            c.w.digest_due
                .store(false, std::sync::atomic::Ordering::Relaxed);
            Ok(json!(security::take_wake_digest(c.run).unwrap_or_else(
                || "No new traffic since the last digest.".into()
            )))
        }),
    },
    Tool {
        name: "access_requests",
        shown: |p| p.officer,
        description: "Pending access requests.",
        props: || json!({}),
        required: &[],
        run: Run::Sync(|c| Ok(json!(security::requests(c.run, true)))),
    },
    Tool {
        name: "background_checks",
        shown: |p| p.officer,
        description: "Pending background checks on new hires: who, the role and what it can reach, the hirer, the budget, the reason and the brief. Decide each with background_check.",
        props: || json!({}),
        required: &[],
        run: Run::Sync(|c| Ok(json!(security::screenings_pending(c.run, c.me)))),
    },
    Tool {
        name: "access_rules",
        shown: |p| p.officer,
        description: "Standing approvals made so far, with how many requests each allowed.",
        props: || json!({}),
        required: &[],
        run: Run::Sync(|c| Ok(json!(security::standing(c.run)))),
    },
    Tool {
        name: "access_decide",
        shown: |p| p.officer,
        description: "Decide an access request. scope once (one connection or request), worker or role (standing). Give an expiry. methods or path make the proxy inspect TLS with the session's CA, which only worker VMs trust: for the cluster, and for CONNECT tunnels, pass methods: [] to let the connection through untouched.",
        props: || json!({
            "request": s("r-4"), "allow": { "type": "boolean" }, "scope": { "type": "string", "enum": ["once", "worker", "role"] }, "reason": s("One line; the worker and the board read it"),
            "methods": { "type": "array", "items": { "type": "string" }, "description": "HTTP methods to allow, e.g. [\"GET\"] (inspects TLS). [] approves the host and port without inspection. Omitted: the request's own methods. Never [\"CONNECT\"]." },
            "path": s("URL path glob (inspects TLS)"), "expires_in": s("e.g. 4h"),
        }),
        required: &["request", "allow", "scope", "reason"],
        run: Run::Sync(|c| {
            let a = security::DecideArgs {
                allow: c.args.must_bool("allow", "allow is required")?,
                scope: c.args.req("scope")?,
                reason: c.args.req("reason")?,
                methods: c.args.list_if_array("methods"),
                path: c.args.opt("path"),
                expires_in: c.args.opt("expires_in"),
            };
            security::decide(c.run, c.me, &c.args.req("request")?, a)
        }),
    },
    Tool {
        name: "background_check",
        shown: |p| p.officer,
        description: "Clear a new hire to start, or object (with the reason): an objection cancels the hire and returns its budget to the hirer. Clearing can grant the access the brief names (not admin on git or the cluster, which is the CEO's).",
        props: || json!({ "screening": s("c-2"), "clear": { "type": "boolean" }, "note": s("One line; the hirer and the board read it. Required to object"),
                    "grants": { "type": "array", "description": "With clear: access the brief names, granted at once", "items": { "type": "object", "properties": { "resource": s("git:<repo> (or git, when there is one), cluster or drive:<space>"), "level": { "type": "string", "enum": ["read", "write", "admin"] }, "expires_in": s("e.g. 8h; default the firm's") }, "required": ["resource", "level"] } } }),
        required: &["screening", "clear"],
        run: Run::Sync(|c| {
            security::decide_screening(
                c.d,
                c.run,
                c.me,
                &c.args.req("screening")?,
                c.args
                    .must_bool("clear", "clear is required: true or false")?,
                &c.args.opt("note").unwrap_or_default(),
                c.args.raw("grants"),
            )
        }),
    },
    Tool {
        name: "access_revoke",
        shown: |p| p.officer,
        description: "Remove a standing approval.",
        props: || json!({ "rule": s("Rule id"), "reason": s("Why") }),
        required: &["rule", "reason"],
        run: Run::Sync(|c| {
            security::revoke(c.run, c.me, &c.args.req("rule")?, &c.args.req("reason")?)
        }),
    },
    Tool {
        name: "escalate",
        shown: |p| p.officer,
        description: "Hand a request to the board: secrets, private networks, global rules.",
        props: || json!({ "request": s("r-4"), "recommendation": s("What you recommend and why") }),
        required: &["request", "recommendation"],
        run: Run::Sync(|c| {
            security::escalate(
                c.run,
                c.me,
                &c.args.req("request")?,
                &c.args.req("recommendation")?,
            )
        }),
    },
];

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

/// A plan is controlled only by its owner or someone above them in the chart.
fn authorize_plan(c: &Call, plan: &str) -> Result<()> {
    let owner = c
        .run
        .plans
        .lock()
        .unwrap()
        .owner_of(plan)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("no plan {plan}"))?;
    if c.me == owner || c.run.chart().is_above(c.me, &owner) {
        Ok(())
    } else {
        bail!("only {owner} or someone above them can control plan {plan}")
    }
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
    d: &Core,
    run: &Arc<FirmRun>,
    s: &Arc<Session>,
    me: &str,
    op: &str,
    args: &Value,
) -> Result<Value> {
    let w = run.worker(me)?;
    let p = powers(run, &w);
    // A worker may call the tools they are shown, and `tools` and the stop
    // hook's `stop_check`.
    let known = TOOLS.iter().any(|t| t.name == op);
    let mine = TOOLS.iter().find(|t| t.name == op && (t.shown)(p));
    if known && mine.is_none() {
        let officers = Powers {
            officer: true,
            ..Default::default()
        };
        if !w.officer() && TOOLS.iter().any(|t| t.name == op && (t.shown)(officers)) {
            bail!("{op} is for security officers");
        }
        bail!("{op} isn't one of your tools");
    }
    if run.ended() {
        bail!("the firm is stopping");
    }
    let tool = match (op, mine) {
        ("tools", _) => return Ok(json!(tools(p))),
        ("stop_check", _) => return Ok(super::foreman::stop_check(run, &w)),
        (_, Some(tool)) => tool,
        (_, None) => bail!(
            "unknown op {op:?}; ops: tools, {}",
            tools(p)
                .iter()
                .filter_map(|t| t["name"].as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    };
    let c = Call {
        d,
        run,
        s,
        w: &w,
        me,
        args: Args(args),
    };
    match tool.run {
        Run::Sync(f) => f(&c),
        Run::Async(f) => f(&c).await,
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
mod tests {
    use serde_json::json;

    use super::{Args, TOOLS};

    /// Arguments as agents write them: lists as one string, tokens with a
    /// unit, empty strings as nothing; each refusal says what to give.
    #[test]
    fn arguments_are_read_as_agents_write_them() {
        let v = json!({
            "to": "ada, bob ,", "list": ["a", "b"], "empty": "", "name": "x",
            "m": "2M", "k": "-500k", "n": 7, "yes": true, "no": "true", "due": "in 2h",
            "attachments": ["a-3@v2", "drive:a-4"],
        });
        let a = Args(&v);
        assert_eq!(a.list("to"), ["ada", "bob"]);
        assert_eq!(a.list("list"), ["a", "b"]);
        assert_eq!(a.list_if_array("to"), None, "a string isn't an array");
        assert_eq!(a.list_if_array("list").unwrap(), ["a", "b"]);
        assert_eq!(a.list("missing"), Vec::<String>::new());
        assert_eq!(
            (a.opt("empty"), a.opt("name").as_deref()),
            (None, Some("x"))
        );
        assert_eq!(a.req("empty").unwrap_err().to_string(), "empty is required");
        assert_eq!(
            (
                a.tokens("m").unwrap(),
                a.tokens("k").unwrap(),
                a.tokens("n").unwrap()
            ),
            (2_000_000, -500_000, 7)
        );
        assert_eq!(a.opt_tokens("missing").unwrap(), None);
        assert_eq!(
            a.tokens("missing").unwrap_err().to_string(),
            "missing is required: tokens, like 2M or 500k"
        );
        assert_eq!(
            a.tokens("yes").unwrap_err().to_string(),
            "yes: tokens, like 2M or 500k"
        );
        assert!((a.flag("yes"), a.flag("no"), a.flag("missing")) == (true, false, false));
        assert!(a.must_bool("yes", "yes?").unwrap());
        assert_eq!(
            a.must_bool("no", "say true or false")
                .unwrap_err()
                .to_string(),
            "say true or false"
        );
        let due = a.due("due").unwrap().unwrap() - super::now();
        assert!((7190.0..7210.0).contains(&due), "{due}");
        assert_eq!(a.due("missing").unwrap(), None);
        assert_eq!(a.attachments().unwrap(), ["drive:a-3@v2", "drive:a-4"]);
        let bad = json!({ "attachments": "notes.md" });
        assert!(Args(&bad)
            .attachments()
            .unwrap_err()
            .to_string()
            .contains("drive:a-3@v2"));
    }

    /// Every argument a tool's handler reads is in the schema its model is
    /// shown: a misspelt name would be read as missing.
    #[test]
    fn handlers_read_only_the_arguments_their_schemas_name() {
        let src = include_str!("ops.rs");
        let table = &src[src.find("static TOOLS: &[Tool] = &[").unwrap()..];
        let table = &table[..table.find("\n];\n").unwrap()];
        // Without whitespace: rustfmt breaks calls across lines.
        let flat = |s: &str| s.split_whitespace().collect::<String>();
        // The string literals that follow `pat` in `s`.
        let after = |s: &str, pat: &str| -> Vec<String> {
            s.match_indices(pat)
                .map(|(i, _)| {
                    let rest = &s[i + pat.len()..];
                    rest[..rest.find('"').unwrap()].to_string()
                })
                .collect()
        };
        let readers = [
            "opt",
            "req",
            "list",
            "list_if_array",
            "flag",
            "must_bool",
            "due",
            "tokens",
            "opt_tokens",
            "raw",
        ];
        let mut seen = 0;
        for entry in table.split("\n    Tool {").skip(1) {
            let name = entry.split('"').nth(1).unwrap();
            let entry = flat(entry);
            let props = &entry[entry.find("props:").unwrap()..entry.find("required:").unwrap()];
            // Keys are the strings followed by a colon.
            let schema: Vec<&str> = props
                .split('"')
                .collect::<Vec<_>>()
                .windows(2)
                .filter(|w| w[1].starts_with(':'))
                .map(|w| w[0])
                .collect();
            let mut reads: Vec<String> = readers
                .iter()
                .flat_map(|m| after(&entry, &format!("c.args.{m}(\"")))
                .collect();
            if entry.contains("c.args.attachments()") {
                reads.push("attachments".into());
            }
            for r in &reads {
                assert!(
                    schema.contains(&r.as_str()),
                    "{name} reads {r:?}, which its schema doesn't name"
                );
            }
            assert!(TOOLS.iter().any(|t| t.name == name), "{name}");
            seen += 1;
        }
        assert_eq!(seen, TOOLS.len());
    }
}
