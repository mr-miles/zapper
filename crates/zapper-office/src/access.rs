//! The credentials subsystem: who may use the firm's resources (each git
//! repository, the cluster, the shared drive's spaces) and how far, as
//! grants the policy owners decide. A worker asks with a reason; the
//! security officer decides, escalating to the CEO, and the CEO to the
//! board; nobody decides their own request. The platform enforces every
//! grant (git on the host, the cluster with RBAC, the drive on the host),
//! grants expire, and leaving the firm revokes them all. Every step is a
//! `credentials` event in the journal.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::firm::{parse_duration, BOARD};
use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{now, FirmRun};

/// How far a grant reaches. For git: read is fetch, write is push to
/// branches, merge is read and merging someone else's branch into `main`
/// (no branches of its own), admin is everything, `main` and tags too. For
/// the cluster: read and write are the `dev` namespace, admin is also
/// `prod`. For a drive space: read is list and fetch, write is also
/// publish.
///
/// Levels aren't a line: merge and write each do what the other can't.
/// What one allows that another does is [`Level::covers`]; the order is for
/// listing only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    None,
    Read,
    Write,
    Merge,
    Admin,
}

impl Level {
    pub fn parse(s: &str) -> Result<Level> {
        match s.trim() {
            "none" => Ok(Level::None),
            "read" => Ok(Level::Read),
            "write" => Ok(Level::Write),
            "merge" => Ok(Level::Merge),
            "admin" => Ok(Level::Admin),
            other => bail!("level {other:?}: none, read, write, merge or admin"),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Level::None => "none",
            Level::Read => "read",
            Level::Write => "write",
            Level::Merge => "merge",
            Level::Admin => "admin",
        }
    }

    /// Whether holding `self` allows all `other` does: admin covers
    /// everything, write and merge each cover read (and themselves), and
    /// nothing covers more than itself otherwise.
    pub fn covers(self, other: Level) -> bool {
        match (self, other) {
            (_, Level::None) => true,
            (Level::Admin, _) => true,
            (Level::Write | Level::Merge, Level::Read) => true,
            (a, b) => a == b,
        }
    }

    /// Whether it may push at all: to branches (write), into `main` (merge),
    /// or anywhere (admin); the repository's hook says where.
    pub fn may_push(self) -> bool {
        matches!(self, Level::Write | Level::Merge | Level::Admin)
    }

    /// Whether it reaches `main` (or `prod`): the CEO's to give.
    pub fn is_main(self) -> bool {
        matches!(self, Level::Merge | Level::Admin)
    }
}

/// A resource's key: `git`, `cluster` or `drive:<space>`.
pub fn resource(run: &FirmRun, r: &str) -> Result<String> {
    resource_in(&run.config, r)
}

/// A resource's canonical name in `firm`: `git:<repo>`, `cluster` or
/// `drive:<space>`. Plain `git` names the repository of a firm that has one.
pub fn resource_in(firm: &crate::firm::Firm, r: &str) -> Result<String> {
    let r = r.trim();
    let repos = firm.git_repos();
    match r {
        "git" => match repos.as_slice() {
            [] => bail!("this firm has no git"),
            [one] => Ok(format!("git:{one}")),
            _ => bail!("name the repository: git:<name> ({})", repos.join(", ")),
        },
        "cluster" if !firm.facilities.is_empty() => Ok(r.into()),
        "cluster" => bail!("this firm has no cluster"),
        _ => {
            if let Some(repo) = r.strip_prefix("git:") {
                // As the firm's server (and a remote) spells the name.
                if let Some(x) = repos.iter().find(|x| x.eq_ignore_ascii_case(repo)) {
                    return Ok(format!("git:{x}"));
                }
                if repos.is_empty() {
                    bail!("this firm has no git");
                }
                bail!(
                    "no repository {repo:?} (repositories: {})",
                    repos.join(", ")
                );
            }
            match r.strip_prefix("drive:") {
                Some(space) if firm.credentials.spaces.iter().any(|s| s == space) => Ok(r.into()),
                Some(space) => bail!(
                    "no drive space {space:?} (spaces: {})",
                    firm.credentials.spaces.join(", ")
                ),
                None => bail!("resource {r:?}: git:<repo>, cluster, or drive:<space>"),
            }
        }
    }
}

/// What a grant or a request is for: its level, by name, and as a `Level`.
#[derive(Debug, Clone, PartialEq)]
pub struct Want {
    pub level: String,
    pub class: Level,
}

impl Want {
    fn of(class: Level) -> Want {
        Want {
            level: class.name().into(),
            class,
        }
    }

    /// `write`.
    pub fn name(&self) -> String {
        self.level.clone()
    }
}

/// What is meant by `s` on a (canonical) resource: read, write or admin;
/// merge too on a git repository.
pub fn want(_run: &FirmRun, resource: &str, s: &str) -> Result<Want> {
    let s = s.trim();
    let level = Level::parse(s)?;
    let git = resource.starts_with("git:");
    if level == Level::None || !max_level(resource).covers(level) || (level == Level::Merge && !git)
    {
        bail!(
            "{resource} levels: read, write{}",
            if git {
                ", merge, admin"
            } else if max_level(resource) == Level::Admin {
                ", admin"
            } else {
                ""
            }
        );
    }
    Ok(Want::of(level))
}

/// The highest level a resource has: drive spaces stop at write.
fn max_level(resource: &str) -> Level {
    if resource.starts_with("drive:") {
        Level::Write
    } else {
        Level::Admin
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Grant {
    pub worker: String,
    pub resource: String,
    /// read, write or admin, or for a GitHub repository its GitHub level.
    pub level: String,
    /// What the level counts as: itself, or a GitHub level's class.
    #[serde(skip)]
    pub class: Level,
    pub by: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub note: String,
    pub granted: f64,
    /// None: while they hold the role (the policy owners' own access).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request: Option<String>,
    #[serde(skip)]
    warned: bool,
}

/// How long before a grant expires its holder is warned, and may renew it:
/// a tenth of its life, from a minute to an hour.
fn renew_window(granted: f64, expires: f64) -> f64 {
    ((expires - granted) / 10.0).clamp(60.0, 3600.0)
}

#[derive(Debug, Clone, Serialize)]
pub struct Request {
    pub id: String,
    pub worker: String,
    pub resource: String,
    /// read, write or admin, or for a GitHub repository its GitHub level.
    pub level: String,
    /// What the level counts as: itself, or a GitHub level's class.
    #[serde(skip)]
    pub class: Level,
    /// What a GitHub level allows, for whoever decides.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub detail: String,
    pub reason: String,
    pub created: f64,
    /// A renewal: when the grant it renews expires.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub renews: Option<f64>,
    /// What the requester is working on: their open assignments, with who
    /// gave them.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub assignments: Vec<String>,
    /// `pending`, `escalated` (to the CEO or the board), `granted`, `denied`.
    pub state: String,
    /// Who decides it now; None is the board.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision: Option<Value>,
}

#[derive(Default)]
pub struct Access {
    /// By worker, then resource.
    grants: BTreeMap<String, BTreeMap<String, Grant>>,
    pub requests: BTreeMap<String, Request>,
    counter: u64,
}

/// The policy owners: the CEO and the security officers.
pub fn owners(run: &FirmRun) -> Vec<String> {
    let chart = run.chart();
    let mut v: Vec<String> = chart.root().map(str::to_string).into_iter().collect();
    for o in &chart.security.officers {
        if !v.contains(o) {
            v.push(o.clone());
        }
    }
    v
}

pub fn is_owner(run: &FirmRun, w: &str) -> bool {
    owners(run).iter().any(|o| o == w)
}

/// A worker's level on a resource now (expired grants count as none).
pub fn level(run: &FirmRun, worker: &str, resource: &str) -> Level {
    let a = run.access.lock().unwrap();
    a.grants
        .get(worker)
        .and_then(|g| g.get(resource))
        .filter(|g| g.expires.is_none_or(|e| e > now()))
        .map(|g| g.class)
        .unwrap_or(Level::None)
}

/// Whether a worker may read a drive space (the board reads everything).
pub fn can_read_space(run: &FirmRun, who: &str, space: &str) -> bool {
    who == BOARD || level(run, who, &format!("drive:{space}")).covers(Level::Read)
}

pub fn can_write_space(run: &FirmRun, who: &str, space: &str) -> bool {
    who == BOARD || level(run, who, &format!("drive:{space}")).covers(Level::Write)
}

fn every_resource(run: &FirmRun) -> Vec<String> {
    let mut v = Vec::new();
    v.extend(run.config.git_resources());
    if !run.config.facilities.is_empty() {
        v.push("cluster".to_string());
    }
    v.extend(
        run.config
            .credentials
            .spaces
            .iter()
            .map(|s| format!("drive:{s}")),
    );
    v
}

/// Puts a grant in place: recorded, enforced, and the worker told.
#[allow(clippy::too_many_arguments)]
fn put(
    run: &Arc<FirmRun>,
    worker: &str,
    resource: &str,
    want: &Want,
    by: &str,
    note: &str,
    expires: Option<f64>,
    request: Option<&str>,
    tell: bool,
) {
    let g = Grant {
        worker: worker.into(),
        resource: resource.into(),
        level: want.level.clone(),
        class: want.class,
        by: by.into(),
        note: note.trim().chars().take(2000).collect(),
        granted: now(),
        expires,
        request: request.map(str::to_string),
        warned: false,
    };
    run.journal
        .append("credentials", json!({ "event": "granted", "grant": g }));
    run.access
        .lock()
        .unwrap()
        .grants
        .entry(worker.into())
        .or_default()
        .insert(resource.into(), g);
    enforce(run, worker, resource);
    if tell {
        let until = expires
            .map(|e| format!(", until {}", super::prompts::when(e)))
            .unwrap_or_default();
        run.notice(
            worker,
            &format!("Access granted: {resource} {}", want.name()),
            &format!(
                "[Automatic notice] {by} granted you {} on {resource}{until}{}.\n",
                want.name(),
                if note.trim().is_empty() {
                    String::new()
                } else {
                    format!(": {}", note.trim())
                }
            ),
        );
    }
}

/// Takes a grant back.
fn take(
    run: &Arc<FirmRun>,
    worker: &str,
    resource: &str,
    by: &str,
    reason: &str,
    tell: bool,
) -> bool {
    let had = run
        .access
        .lock()
        .unwrap()
        .grants
        .get_mut(worker)
        .and_then(|g| g.remove(resource))
        .is_some();
    if !had {
        return false;
    }
    run.journal.append("credentials", json!({ "event": "revoked", "worker": worker, "resource": resource, "by": by, "reason": reason.trim() }));
    enforce(run, worker, resource);
    if tell && run.rt(worker).is_some() {
        run.notice(worker, &format!("Access revoked: {resource}"), &format!("[Automatic notice] Your access to {resource} was revoked by {by}: {}. Ask again with credentials_request if your work needs it.\n", reason.trim()));
    }
    true
}

/// Makes the platform match the grant: the cluster's RBAC and the worker's
/// kubeconfig, and the network rules for git and the cluster. git and the
/// drive are checked on every request, so they need nothing more here.
fn enforce(run: &Arc<FirmRun>, worker: &str, resource: &str) {
    if resource.starts_with("git:") || resource == "cluster" {
        super::facility::network_follows_grant(run, worker, resource, level(run, worker, resource));
    }
    if resource == "cluster" {
        let (run, worker) = (run.clone(), worker.to_string());
        tokio::spawn(async move {
            let lvl = level(&run, &worker, "cluster");
            let expires = run
                .access
                .lock()
                .unwrap()
                .grants
                .get(&worker)
                .and_then(|g| g.get("cluster"))
                .and_then(|g| g.expires);
            if let Err(e) = super::facility::cluster_access(&run, &worker, lvl, expires).await {
                tracing::warn!(firm = %run.firm, %worker, "cluster access: {e:#}");
                run.journal.append("credentials", json!({ "event": "enforce_failed", "worker": worker, "resource": "cluster", "error": format!("{e:#}") }));
            }
        });
    }
}

/// Puts a worker's grants in place again: their VM came up after them.
pub fn reapply(run: &Arc<FirmRun>, worker: &str) {
    let repos = run.config.git_resources();
    for r in repos.into_iter().chain(["cluster".to_string()]) {
        if level(run, worker, &r) > Level::None {
            enforce(run, worker, &r);
        }
    }
}

/// The policy owners' own access: admin on git and the cluster, write on
/// every drive space, for as long as they hold the role.
pub fn owner_access(run: &Arc<FirmRun>, worker: &str, by: &str) {
    for r in every_resource(run) {
        let want = Want::of(max_level(&r));
        put(
            run,
            worker,
            &r,
            &want,
            by,
            "a policy owner",
            None,
            None,
            false,
        );
    }
}

/// Everything a worker holds, revoked: they are leaving, or stepping down.
pub fn revoke_all(run: &Arc<FirmRun>, worker: &str, by: &str, reason: &str) {
    let held: Vec<String> = run
        .access
        .lock()
        .unwrap()
        .grants
        .get(worker)
        .map(|g| g.keys().cloned().collect())
        .unwrap_or_default();
    for r in held {
        take(run, worker, &r, by, reason, false);
    }
    // Their own open requests lapse.
    let mut a = run.access.lock().unwrap();
    for q in a
        .requests
        .values_mut()
        .filter(|q| q.worker == worker && matches!(q.state.as_str(), "pending" | "escalated"))
    {
        q.state = "denied".into();
        q.decision = Some(json!({ "by": by, "note": reason, "at": now() }));
    }
}

/// Who decides a request first: a security officer other than the
/// requester; the CEO for merge or admin on git, admin on the cluster, or
/// when no officer can; the board for the CEO's own.
fn first_decider(run: &FirmRun, worker: &str, resource: &str, level: Level) -> Option<String> {
    let chart = run.chart();
    let ceo = chart.root().map(str::to_string);
    let to_ceo = || ceo.clone().filter(|c| c != worker);
    if level.is_main() && !resource.starts_with("drive:") {
        return to_ceo();
    }
    chart
        .security
        .officers
        .iter()
        .find(|o| *o != worker && Some(*o) != ceo.as_ref())
        .cloned()
        .or_else(to_ceo)
}

/// One step up: from an officer to the CEO, from the CEO to the board.
fn next_decider(run: &FirmRun, from: Option<&str>, worker: &str) -> Option<String> {
    let ceo = run.chart().root().map(str::to_string);
    match from {
        Some(f) if Some(f) != ceo.as_deref() => ceo.filter(|c| c != worker),
        _ => None,
    }
}

fn request_text(q: &Request) -> String {
    format!(
        "Credential request {} — {} asks for {} on {}{}.\n{}{}Reason (the requester's words):\n{}\nDecide with credentials_decide(request: \"{}\", grant, level?, expires_in?, note), or hand it up with credentials_escalate.\n",
        q.id,
        q.worker,
        q.level,
        q.resource,
        q.renews
            .map(|e| format!(
                " (a renewal: the grant they hold expires at {})",
                super::prompts::when(e)
            ))
            .unwrap_or_default(),
        if q.detail.is_empty() {
            String::new()
        } else {
            format!("The level allows: {}. A GitHub request is granted as asked or denied; they can ask for a narrower level.\n", q.detail)
        },
        if q.assignments.is_empty() {
            "Their open assignments: none.\n".to_string()
        } else {
            format!(
                "Their open assignments (from the post office):\n{}",
                q.assignments
                    .iter()
                    .map(|a| format!("  - {a}\n"))
                    .collect::<String>()
            )
        },
        q.reason.lines().map(|l| format!("> {l}\n")).collect::<String>(),
        q.id
    )
}

fn notify_decider(run: &FirmRun, q: &Request, extra: &str) {
    match &q.decider {
        Some(d) => run.notice(d, &format!("Credential request {}: {} {} for {}", q.id, q.resource, q.level, q.worker), &format!("[Automatic notice] {extra}{}", request_text(q))),
        None => run.notice(BOARD, &format!("Credential request {}: {} {} for {}", q.id, q.resource, q.level, q.worker), &format!("[Automatic notice] {extra}A credential request needs the board's decision.\n\n{}\nDecide it in the UI's Security tab, or with `zapper firm credentials`.\n", request_text(q))),
    }
}

/// A worker asks for access; returns the request's id. `level` is read,
/// write or admin, or for a GitHub repository one of the firm's GitHub
/// levels.
pub fn request(
    run: &FirmRun,
    worker: &str,
    resource: &str,
    level: &str,
    reason: &str,
) -> Result<String> {
    let resource = self::resource(run, resource)?;
    let want = self::want(run, &resource, level)?;
    let level = want.class;
    if reason.trim().is_empty() {
        bail!("give a reason: what your work needs it for");
    }
    // The same again: a renewal, once the grant is about to expire.
    let held = self::level(run, worker, &resource);
    let repeat = held != Level::None && held.covers(level);
    let renews = if repeat {
        let g = run
            .access
            .lock()
            .unwrap()
            .grants
            .get(worker)
            .and_then(|g| g.get(&resource))
            .map(|g| (g.granted, g.expires));
        match g {
            Some((granted, Some(e))) if held == level => {
                let from = e - renew_window(granted, e);
                if now() < from {
                    bail!(
                        "you have {} on {resource} until {}; renew it from {}",
                        held.name(),
                        super::prompts::when(e),
                        super::prompts::when(from)
                    );
                }
                Some(e)
            }
            _ => bail!("you already have {} on {resource}", held.name()),
        }
    } else {
        None
    };
    let decider = first_decider(run, worker, &resource, level);
    let assignments: Vec<String> = run
        .post
        .with(|po| po.desk(worker))
        .owed
        .iter()
        .filter(|o| o.kind == super::mail::Kind::Assignment)
        .take(5)
        .map(|o| format!("{} (from {})", o.subject, o.creditor))
        .collect();
    let q = {
        let mut a = run.access.lock().unwrap();
        if let Some(q) = a.requests.values().find(|q| {
            q.worker == worker
                && q.resource == resource
                && matches!(q.state.as_str(), "pending" | "escalated")
        }) {
            if q.level != want.level {
                bail!(
                    "you already asked for {} on {resource} (request {}); wait for it to be decided",
                    q.level,
                    q.id
                );
            }
            return Ok(q.id.clone());
        }
        a.counter += 1;
        let q = Request {
            id: format!("p-{}", a.counter),
            worker: worker.into(),
            detail: String::new(),
            resource,
            level: want.level.clone(),
            class: level,
            reason: reason.trim().chars().take(2000).collect(),
            created: now(),
            renews,
            assignments,
            state: if decider.is_some() {
                "pending"
            } else {
                "escalated"
            }
            .into(),
            decider,
            decision: None,
        };
        a.requests.insert(q.id.clone(), q.clone());
        q
    };
    run.journal
        .append("credentials", json!({ "event": "requested", "request": q }));
    notify_decider(run, &q, "");
    Ok(q.id)
}

/// The decider (or the CEO above them, or the board) grants or denies.
#[allow(clippy::too_many_arguments)]
pub fn decide(
    run: &Arc<FirmRun>,
    by: &str,
    id: &str,
    grant: bool,
    level: Option<&str>,
    expires_in: Option<&str>,
    note: &str,
) -> Result<Value> {
    let q = run
        .access
        .lock()
        .unwrap()
        .requests
        .get(id)
        .cloned()
        .ok_or_else(|| anyhow!("no credential request {id}"))?;
    if !matches!(q.state.as_str(), "pending" | "escalated") {
        bail!("request {id} was already {}", q.state);
    }
    if q.worker == by {
        bail!("you can't decide your own request");
    }
    let ceo = run.chart().root().map(str::to_string);
    let allowed = by == BOARD
        || q.decider.as_deref() == Some(by)
        || (ceo.as_deref() == Some(by) && q.decider.is_some());
    if !allowed {
        bail!(
            "request {id} is {}'s to decide",
            q.decider.as_deref().unwrap_or("the board's")
        );
    }
    if !grant && note.trim().is_empty() {
        bail!("give a reason for the denial; the requester reads it");
    }
    // A lower level than asked may be granted.
    let want = match level {
        Some(l) => Want::of(Level::parse(l)?),
        None => Want::of(q.class),
    };
    if grant && (want.class == Level::None || !q.class.covers(want.class)) {
        bail!("grant at most what was asked ({})", q.level);
    }
    let expires = match expires_in {
        Some(e) => Some(now() + parse_duration(e)?.as_secs_f64()),
        None => Some(now() + parse_duration(&run.config.credentials.expiry)?.as_secs_f64()),
    };
    let decision = json!({ "by": by, "grant": grant, "level": want.level, "expires": if grant { expires } else { None }, "note": note.trim(), "at": now() });
    {
        let mut a = run.access.lock().unwrap();
        let r = a.requests.get_mut(id).expect("request");
        r.state = if grant { "granted" } else { "denied" }.into();
        r.decision = Some(decision.clone());
    }
    run.journal.append("credentials", json!({ "event": "decided", "id": id, "worker": q.worker, "resource": q.resource, "decision": decision }));
    if grant {
        put(
            run,
            &q.worker,
            &q.resource,
            &want,
            by,
            note,
            expires,
            Some(id),
            true,
        );
    } else {
        run.notice(
            &q.worker,
            &format!("Access denied: {}", q.resource),
            &format!(
                "[Automatic notice] {by} denied your request {id} for {} on {}: {}\n",
                q.level,
                q.resource,
                note.trim()
            ),
        );
    }
    Ok(
        json!({ "request": id, "state": if grant { "granted" } else { "denied" }, "level": want.level, "expires": expires }),
    )
}

/// The decider hands a request up: an officer to the CEO, the CEO to the
/// board.
pub fn escalate(run: &FirmRun, by: &str, id: &str, recommendation: &str) -> Result<Value> {
    let q = run
        .access
        .lock()
        .unwrap()
        .requests
        .get(id)
        .cloned()
        .ok_or_else(|| anyhow!("no credential request {id}"))?;
    if q.decider.as_deref() != Some(by) {
        bail!("request {id} isn't yours to hand up");
    }
    let next = next_decider(run, Some(by), &q.worker);
    let q = {
        let mut a = run.access.lock().unwrap();
        let r = a.requests.get_mut(id).expect("request");
        r.decider = next;
        r.state = "escalated".into();
        r.decision = Some(json!({ "escalated_by": by, "recommendation": recommendation.trim() }));
        r.clone()
    };
    run.journal.append("credentials", json!({ "event": "escalated", "id": id, "by": by, "to": q.decider.clone().unwrap_or_else(|| BOARD.into()), "recommendation": recommendation.trim() }));
    notify_decider(
        run,
        &q,
        &format!(
            "{by} handed this up to you, recommending:\n{}\n",
            recommendation
                .trim()
                .lines()
                .map(|l| format!("> {l}\n"))
                .collect::<String>()
        ),
    );
    Ok(json!({ "request": id, "to": q.decider.unwrap_or_else(|| BOARD.into()) }))
}

/// A policy owner (or the board) takes a grant back.
pub fn revoke(
    run: &Arc<FirmRun>,
    by: &str,
    worker: &str,
    resource: &str,
    reason: &str,
) -> Result<Value> {
    if by != BOARD && !is_owner(run, by) {
        bail!("only the security officers, the CEO and the board revoke access");
    }
    if reason.trim().is_empty() {
        bail!("give a reason");
    }
    if is_owner(run, worker) && by != BOARD {
        bail!("{worker} is a policy owner; only the board changes their access");
    }
    let resource = self::resource(run, resource)?;
    if !take(run, worker, &resource, by, reason, true) {
        bail!("{worker} has no grant on {resource}");
    }
    Ok(json!({ "worker": worker, "resource": resource, "revoked": true }))
}

/// A grant made when a hire is cleared: resource, level, expiry.
pub type Clearance = (String, Want, f64);

/// Checks the grants an officer (or the board) gives with a background
/// check, all before any is made: the resources exist, levels are within
/// them (a GitHub level for a GitHub repository, at most one of those), admin on
/// git and the cluster is only the CEO's or the board's to give, and an
/// officer hire (a policy owner once cleared) gets none.
pub fn check_clearance(
    run: &FirmRun,
    by: &str,
    officer_hire: bool,
    grants: &Value,
) -> Result<Vec<Clearance>> {
    let list = match grants {
        Value::Null => return Ok(vec![]),
        Value::Array(a) => a,
        _ => bail!("grants: a list of {{resource, level, expires_in?}}"),
    };
    if list.is_empty() {
        return Ok(vec![]);
    }
    if officer_hire {
        bail!("an officer hire owns policy once cleared: give no grants");
    }
    let ceo = run.chart().root().map(str::to_string);
    let mut out: Vec<Clearance> = Vec::new();
    for g in list {
        let r = resource(run, g["resource"].as_str().unwrap_or(""))?;
        let want = want(run, &r, g["level"].as_str().unwrap_or(""))?;
        if want.class.is_main()
            && !r.starts_with("drive:")
            && by != BOARD
            && ceo.as_deref() != Some(by)
        {
            bail!("{} on {r} is the CEO's to grant: leave it out, the hire asks for it with credentials_request", want.class.name());
        }
        let ttl = g["expires_in"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| run.config.credentials.expiry.clone());
        let expires = now() + parse_duration(&ttl)?.as_secs_f64();
        if out.iter().any(|(x, ..)| *x == r) {
            bail!("{r} is in the grants twice");
        }
        out.push((r, want, expires));
    }
    Ok(out)
}

/// Makes the grants of a background check, and tells the hire once.
pub fn grant_at_clearance(
    run: &Arc<FirmRun>,
    by: &str,
    worker: &str,
    screening: &str,
    grants: &[Clearance],
) {
    // Every hire reads the knowledge base, from being cleared (an officer
    // holds admin on it, as on all git, once cleared).
    let mut grants = grants.to_vec();
    if let Some(kb) = run.config.knowledge_base() {
        let r = format!("git:{kb}");
        if !run.chart().is_officer(worker)
            && !grants.iter().any(|(x, ..)| *x == r)
            && level(run, worker, &r) == Level::None
        {
            if let Ok(ttl) = parse_duration(&run.config.credentials.expiry) {
                grants.push((r, Want::of(Level::Read), now() + ttl.as_secs_f64()));
            }
        }
    }
    if grants.is_empty() {
        return;
    }
    let note = format!("cleared with background check {screening}");
    for (r, want, expires) in &grants {
        put(run, worker, r, want, by, &note, Some(*expires), None, false);
    }
    let lines: String = grants
        .iter()
        .map(|(r, l, e)| {
            format!(
                "  - {} on {r}, until {}\n",
                l.name(),
                super::prompts::when(*e)
            )
        })
        .collect();
    run.notice(
        worker,
        "Your access",
        &format!("[Automatic notice] {by} cleared you and granted:\n{lines}Ask for anything else your work needs with credentials_request.\n"),
    );
}

/// The board grants directly.
pub fn board_grant(
    run: &Arc<FirmRun>,
    worker: &str,
    resource: &str,
    level: &str,
    expires_in: Option<&str>,
    note: &str,
) -> Result<Value> {
    if run.rt(worker).is_none() {
        bail!("no worker {worker}");
    }
    let resource = self::resource(run, resource)?;
    let want = self::want(run, &resource, level)?;
    let ttl = expires_in
        .map(str::to_string)
        .unwrap_or_else(|| run.config.credentials.expiry.clone());
    let expires = now() + parse_duration(&ttl)?.as_secs_f64();
    put(
        run,
        worker,
        &resource,
        &want,
        BOARD,
        note,
        Some(expires),
        None,
        true,
    );
    Ok(json!({ "worker": worker, "resource": resource, "level": want.level, "expires": expires }))
}

/// Requests `from` was to decide (they left, or stopped being an officer):
/// to whoever decides them now.
pub fn reassign(run: &FirmRun, from: &str) {
    let mine: Vec<Request> = run
        .access
        .lock()
        .unwrap()
        .requests
        .values()
        .filter(|q| q.state == "pending" && q.decider.as_deref() == Some(from))
        .cloned()
        .collect();
    for q in mine {
        let next = first_decider(run, &q.worker, &q.resource, q.class)
            .filter(|d| d != from)
            .or_else(|| next_decider(run, Some(from), &q.worker));
        let q = {
            let mut a = run.access.lock().unwrap();
            let Some(r) = a.requests.get_mut(&q.id) else {
                continue;
            };
            r.decider = next;
            if r.decider.is_none() {
                r.state = "escalated".into();
            }
            r.created = now();
            r.clone()
        };
        run.journal.append("credentials", json!({ "event": "reassigned", "id": q.id, "from": from, "to": q.decider.clone().unwrap_or_else(|| BOARD.into()) }));
        notify_decider(
            run,
            &q,
            &format!("{from} no longer decides this; it is yours now.\n"),
        );
    }
}

/// Requests an officer left unanswered for `security.answer_within` go up
/// to the CEO.
fn escalate_late(run: &FirmRun) {
    let within = parse_duration(&run.config.security.answer_within)
        .map(|d| d.as_secs_f64())
        .unwrap_or(900.0);
    let ceo = run.chart().root().map(str::to_string);
    let late: Vec<Request> = run
        .access
        .lock()
        .unwrap()
        .requests
        .values()
        .filter(|q| {
            q.state == "pending"
                && q.decider.is_some()
                && q.decider != ceo
                && now() - q.created > within
        })
        .cloned()
        .collect();
    for q in late {
        let from = q.decider.clone().unwrap_or_default();
        let next = next_decider(run, Some(&from), &q.worker);
        let q = {
            let mut a = run.access.lock().unwrap();
            let Some(r) = a.requests.get_mut(&q.id) else {
                continue;
            };
            r.decider = next;
            r.state = "escalated".into();
            r.created = now();
            r.clone()
        };
        run.journal.append("credentials", json!({ "event": "escalated", "id": q.id, "by": "office", "to": q.decider.clone().unwrap_or_else(|| BOARD.into()), "recommendation": format!("{from} didn't answer within {}", run.config.security.answer_within) }));
        notify_decider(
            run,
            &q,
            &format!(
                "{from} didn't answer this within {}, so it comes to you.\n",
                run.config.security.answer_within
            ),
        );
    }
}

/// A worker and a resource they hold.
type Held = (String, String);

/// Grants that lapse, and a warning before they do.
pub fn tick(run: &Arc<FirmRun>) {
    escalate_late(run);
    let t = now();
    // (worker, resource) that lapsed; and those to warn, with their expiry.
    let (expired, warn): (Vec<Held>, Vec<(Held, f64)>) = {
        let mut a = run.access.lock().unwrap();
        let mut expired = Vec::new();
        let mut warn = Vec::new();
        for (w, gs) in a.grants.iter_mut() {
            for (r, g) in gs.iter_mut() {
                let Some(e) = g.expires else { continue };
                if e <= t {
                    expired.push((w.clone(), r.clone()));
                } else if !g.warned && e - t < renew_window(g.granted, e) {
                    g.warned = true;
                    warn.push(((w.clone(), r.clone()), e));
                }
            }
        }
        (expired, warn)
    };
    for (w, r) in expired {
        take(run, &w, &r, "office", "the grant expired", true);
    }
    for ((w, r), e) in warn {
        run.notice(&w, &format!("Access to {r} expires soon"), &format!("[Automatic notice] Your access to {r} expires at {}. If your work still needs it, renew it now: credentials_request for the same resource and level, saying what is left to do.\n", super::prompts::when(e)));
    }
}

/// A worker's own grants and requests, and what there is to ask for.
pub fn mine(run: &FirmRun, worker: &str) -> Value {
    let a = run.access.lock().unwrap();
    let grants: Vec<&Grant> = a
        .grants
        .get(worker)
        .map(|g| g.values().collect())
        .unwrap_or_default();
    let requests: Vec<&Request> = a.requests.values().filter(|q| q.worker == worker).collect();
    json!({
        "grants": grants, "requests": requests,
        "resources": every_resource(run),
        "levels": { "git:<repo>": "read (fetch), write (push branches; publish_branch for review where main is the remote's), admin (also main and tags, where main is the firm's)", "cluster": "read (view dev), write (deploy to dev), admin (also prod)", "drive:<space>": "read (list, fetch), write (also publish)" },
    })
}

/// Every grant and request, for the policy owners, the API and the UI.
pub fn all(run: &FirmRun) -> Value {
    let a = run.access.lock().unwrap();
    let grants: Vec<&Grant> = a.grants.values().flat_map(|g| g.values()).collect();
    let requests: Vec<&Request> = a.requests.values().collect();
    json!({ "grants": grants, "requests": requests, "resources": every_resource(run), "owners": owners(run) })
}

#[cfg(test)]
mod tests {
    use super::Level;

    #[test]
    fn levels_cover_what_they_allow() {
        use Level::*;
        // Write and merge each do what the other can't; admin does both.
        assert!(
            Write.covers(Read) && Merge.covers(Read) && Admin.covers(Merge) && Admin.covers(Write)
        );
        assert!(!Merge.covers(Write) && !Write.covers(Merge) && !Read.covers(Write));
        assert!(Merge.may_push() && Write.may_push() && !Read.may_push());
        assert!(Merge.is_main() && Admin.is_main() && !Write.is_main());
        assert_eq!(Level::parse("merge").unwrap(), Merge);
        assert_eq!(Level::parse("write").unwrap(), Level::Write);
        assert!(Level::parse("root").is_err());
        assert_eq!(serde_json::to_value(Level::Admin).unwrap(), "admin");
    }

    fn firm(git: &str) -> crate::firm::Firm {
        let yaml = format!(
            "profiles:\n  w: {{ agent: {{ kind: claude-code }} }}\nfirms:\n  acme:\n    roles: {{ ceo: {{ profile: w, agent: ceo }} }}\n    workers: [{{ name: ceo, role: ceo }}]\n{git}"
        );
        crate::firm::get(&zapper_policy::Config::from_yaml(&yaml).unwrap(), "acme").unwrap()
    }

    #[test]
    fn git_resources_are_per_repository() {
        use super::resource_in;
        let none = firm("");
        assert!(resource_in(&none, "git").is_err());
        assert!(resource_in(&none, "git:product").is_err());

        // One repository: plain `git` is it.
        let one = firm("    git: {}\n");
        assert_eq!(resource_in(&one, "git").unwrap(), "git:product");
        assert_eq!(resource_in(&one, "git:product").unwrap(), "git:product");

        let three = firm("    git: { repos: [product, product2, product3] }\n");
        assert_eq!(resource_in(&three, "git:product2").unwrap(), "git:product2");
        let e = resource_in(&three, "git").unwrap_err().to_string();
        assert!(e.contains("product, product2, product3"), "{e}");
        assert!(resource_in(&three, "git:other").is_err());
        assert!(resource_in(&three, "drive:general").is_ok());
    }
}
