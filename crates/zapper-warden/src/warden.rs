//! The warden's routes and what they do. Every route is the office
//! sandbox's alone: the warden's edge (`zapper_openshell::edge`) checks the
//! token its supervisor added, and [`Warden::caller`] knows no other
//! sandbox.
//!
//! | Route | Does |
//! |---|---|
//! | `GET /workers` | The workers the warden made: name, profile, sandbox, id, ready |
//! | `PUT /workers/{name}` `{"profile": P}` | Makes worker `name` from profile `P`'s template, if it isn't there |
//! | `DELETE /workers/{name}` | Deletes worker `name` |
//! | `POST /workers/{name}/agent` `{"ticket": T, "resume": R?}` | Starts the worker's agent and pipes it to the office's agent port, opened with `ZCAGENT T` |
//! | `GET /jwks` | The driver's keys, for the office to check its workers' SVIDs |
//!
//! The agent's stream goes into the office sandbox the way the workers'
//! edge does, by `ForwardTcp`: the office never needs more of its
//! supervisor than plain requests.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use futures_util::future::BoxFuture;
use hyper::body::Incoming;
use hyper::{Method, Request, Response, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use zapper_openshell::edge::{self, Backend, Body, Caller};
use zapper_openshell::GatewaySandbox;

use crate::audit::Audit;
use crate::config::Config;
use crate::gateway::Gateway;
use crate::names::{self, LABEL_PROFILE, LABEL_ROLE, LABEL_WORKER};
use crate::profiles::{self, Profiles};

/// The most a request's body may be (a small JSON object).
const MAX_BODY: usize = 16 << 10;
/// What opens an agent's stream to the office, before its ticket.
pub const AGENT_GREETING: &str = "ZCAGENT";

/// A refusal the office can act on: its HTTP status says which.
#[derive(Debug, thiserror::Error)]
pub enum Refusal {
    #[error("{0}")]
    Bad(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    Full(String),
}

impl Refusal {
    fn status(&self) -> StatusCode {
        match self {
            Refusal::Bad(_) => StatusCode::BAD_REQUEST,
            Refusal::NotFound(_) => StatusCode::NOT_FOUND,
            Refusal::Conflict(_) => StatusCode::CONFLICT,
            Refusal::Full(_) => StatusCode::TOO_MANY_REQUESTS,
        }
    }
}

/// A worker the warden made.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Worker {
    pub name: String,
    pub profile: String,
    /// Its sandbox's name at the gateway.
    pub sandbox: String,
    /// The gateway's id for it (its supervisor's SPIFFE ID names it).
    pub id: String,
    pub ready: bool,
}

/// The worker sandbox `s` is, if the warden made it: named and labelled as
/// it names and labels them, from its profile's template.
fn worker_of(s: &GatewaySandbox) -> Option<Worker> {
    let name = s.labels.get(LABEL_WORKER)?;
    let profile = s.labels.get(LABEL_PROFILE)?;
    let ours = s.name == names::worker_sandbox(name)
        && s.template.as_deref() == Some(names::profile_template(profile).as_str());
    ours.then(|| Worker {
        name: name.clone(),
        profile: profile.clone(),
        sandbox: s.name.clone(),
        id: s.id.clone(),
        ready: s.ready,
    })
}

pub struct Warden {
    gw: Arc<dyn Gateway>,
    config: Config,
    profiles: Profiles,
    /// The office sandbox's id: the one caller.
    office: String,
    audit: Audit,
    /// One make or delete at a time, so the caps and names hold.
    control: tokio::sync::Mutex<()>,
    /// The workers whose agent's stream is open: one at a time each.
    streams: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
}

/// Takes worker `name`'s place in `streams` off when dropped (its stream
/// ended, or never got going).
struct Open {
    streams: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    name: String,
}

impl Drop for Open {
    fn drop(&mut self) {
        self.streams.lock().unwrap().remove(&self.name);
    }
}

impl Warden {
    /// Makes sure the office sandbox is there, made from its template (made
    /// now if it isn't), and answers its id.
    pub async fn ensure_office(
        gw: &dyn Gateway,
        profiles: &Profiles,
        audit: &Audit,
    ) -> Result<String> {
        let o = &profiles.office;
        let found = gw
            .sandboxes()
            .await?
            .into_iter()
            .find(|s| s.name == names::OFFICE);
        if let Some(s) = found {
            let ours = s.template.as_deref() == Some(names::OFFICE)
                && s.labels.get(LABEL_ROLE).map(String::as_str) == Some("office");
            if !ours {
                anyhow::bail!(
                    "a sandbox {} is at the gateway that the warden didn't make (from template {:?}); delete it or rename it",
                    names::OFFICE,
                    s.template
                );
            }
            if !s.ready {
                anyhow::bail!(
                    "the office sandbox {} isn't ready yet; start the warden again once it is",
                    names::OFFICE
                );
            }
            return Ok(s.id);
        }
        // Made but not ready, it would be found next time and its setup
        // skipped: so a failed make is deleted, as a failed setup is.
        let id = match gw
            .create(&o.template, names::OFFICE, &o.labels, &o.providers)
            .await
        {
            Ok(id) => id,
            Err(e) => {
                if let Err(d) = gw.delete(names::OFFICE).await {
                    tracing::warn!("deleting the office sandbox after a failed make: {d:#}");
                }
                return Err(e.context("making the office sandbox"));
            }
        };
        if let Err(e) = gw.run_setup(names::OFFICE, &o.setup).await {
            if let Err(d) = gw.delete(names::OFFICE).await {
                tracing::warn!("deleting the office sandbox after a failed setup: {d:#}");
            }
            return Err(e.context("setting the office sandbox up"));
        }
        audit.record(
            "office_made",
            json!({ "sandbox": names::OFFICE, "id": id, "profile": o.profile.name }),
        );
        Ok(id)
    }

    pub fn new(
        gw: Arc<dyn Gateway>,
        config: Config,
        profiles: Profiles,
        office: String,
        audit: Audit,
    ) -> Warden {
        Warden {
            gw,
            config,
            profiles,
            office,
            audit,
            control: tokio::sync::Mutex::new(()),
            streams: Arc::default(),
        }
    }

    /// The office sandbox's id.
    pub fn office_id(&self) -> &str {
        &self.office
    }

    /// The workers the warden made, by name.
    pub async fn workers(&self) -> Result<Vec<Worker>> {
        let mut all: Vec<Worker> = self
            .gw
            .sandboxes()
            .await?
            .iter()
            .filter_map(worker_of)
            .collect();
        all.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(all)
    }

    async fn worker(&self, name: &str) -> Result<Option<Worker>> {
        Ok(self.workers().await?.into_iter().find(|w| w.name == name))
    }

    /// Makes worker `name` of `profile`, unless it is there already (of
    /// that profile: it, and `false`); within the caps.
    pub async fn put_worker(&self, name: &str, profile: &str) -> Result<(Worker, bool)> {
        names::check("worker", name).map_err(|e| Refusal::Bad(format!("{e:#}")))?;
        let Some(p) = self.profiles.workers.get(profile) else {
            return Err(Refusal::Bad(format!(
                "profile {profile:?} isn't one the warden offers ({})",
                self.profiles
                    .workers
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
            .into());
        };
        let _one = self.control.lock().await;
        let all = self.workers().await?;
        if let Some(w) = all.iter().find(|w| w.name == name) {
            if w.profile == profile {
                return Ok((w.clone(), false));
            }
            return Err(Refusal::Conflict(format!(
                "worker {name} is there, of profile {}",
                w.profile
            ))
            .into());
        }
        let sandbox = names::worker_sandbox(name);
        if self
            .gw
            .sandboxes()
            .await?
            .iter()
            .any(|s| s.name == sandbox)
        {
            return Err(Refusal::Conflict(format!(
                "a sandbox {sandbox} is at the gateway that the warden didn't make"
            ))
            .into());
        }
        if all.len() >= self.config.max_workers {
            return Err(Refusal::Full(format!(
                "{} workers already, the most the warden makes",
                all.len()
            ))
            .into());
        }
        let of_profile = all.iter().filter(|w| w.profile == profile).count();
        let max = self.config.max_of(profile);
        if of_profile >= max {
            return Err(Refusal::Full(format!(
                "{of_profile} workers of profile {profile} already, the most it allows"
            ))
            .into());
        }
        let labels = names::worker_labels(name, profile);
        // A failed make, setup or not, is deleted: never a worker half made.
        let made = async {
            let id = self
                .gw
                .create(&p.template, &sandbox, &labels, &p.providers)
                .await?;
            if p.proposals {
                if let Err(e) = self.gw.proposals_on(&sandbox).await {
                    tracing::warn!(sandbox = %sandbox, "{e:#}; its agent can't propose rules");
                }
            }
            self.gw.run_setup(&sandbox, &p.setup).await?;
            Ok::<_, anyhow::Error>(id)
        }
        .await;
        let id = match made {
            Ok(id) => id,
            Err(e) => {
                if let Err(d) = self.gw.delete(&sandbox).await {
                    tracing::warn!(sandbox = %sandbox, "deleting it after a failed make: {d:#}");
                }
                self.audit.record(
                    "worker_failed",
                    json!({ "worker": name, "profile": profile, "error": format!("{e:#}") }),
                );
                return Err(e.context(format!("making worker {name}")));
            }
        };
        self.audit.record(
            "worker_made",
            json!({ "worker": name, "profile": profile, "sandbox": sandbox, "id": id }),
        );
        let w = Worker {
            name: name.to_string(),
            profile: profile.to_string(),
            sandbox,
            id,
            ready: true,
        };
        Ok((w, true))
    }

    /// Deletes worker `name`.
    pub async fn delete_worker(&self, name: &str) -> Result<()> {
        names::check("worker", name).map_err(|e| Refusal::Bad(format!("{e:#}")))?;
        let _one = self.control.lock().await;
        let w = self
            .worker(name)
            .await?
            .ok_or_else(|| Refusal::NotFound(format!("no worker {name}")))?;
        self.gw.delete(&w.sandbox).await?;
        self.audit.record(
            "worker_deleted",
            json!({ "worker": name, "profile": w.profile, "sandbox": w.sandbox }),
        );
        Ok(())
    }

    /// Starts worker `name`'s agent (its profile's harness, resuming
    /// `resume` if given) and pipes it to the office's agent port, opened
    /// with `ZCAGENT <ticket>`. Answers once both ends are open; the stream
    /// runs until either ends.
    pub async fn start_agent(&self, name: &str, ticket: &str, resume: Option<&str>) -> Result<()> {
        names::check("worker", name).map_err(|e| Refusal::Bad(format!("{e:#}")))?;
        names::check_ticket(ticket).map_err(|e| Refusal::Bad(format!("{e:#}")))?;
        if let Some(r) = resume {
            names::check_resume(r).map_err(|e| Refusal::Bad(format!("{e:#}")))?;
        }
        let w = self
            .worker(name)
            .await?
            .ok_or_else(|| Refusal::NotFound(format!("no worker {name}")))?;
        if !w.ready {
            return Err(Refusal::Conflict(format!("worker {name} isn't ready")).into());
        }
        let p = self.profiles.workers.get(&w.profile).ok_or_else(|| {
            Refusal::Conflict(format!(
                "worker {name}'s profile {} is no longer one the warden offers",
                w.profile
            ))
        })?;
        let (argv, env, workdir) = profiles::agent_command(p, &w.sandbox, resume)?;
        // One agent at a time per worker: the office starts another only
        // once the last one's stream has ended.
        if !self.streams.lock().unwrap().insert(name.to_string()) {
            return Err(Refusal::Conflict(format!(
                "worker {name}'s agent is running; its stream must end before another starts"
            ))
            .into());
        }
        let open = Open {
            streams: self.streams.clone(),
            name: name.to_string(),
        };
        let mut agent = self.gw.exec(&w.sandbox, argv, env, workdir).await?;
        let mut office = self
            .gw
            .forward(names::OFFICE, self.config.office.ports.agent)
            .await?;
        office
            .write_all(format!("{AGENT_GREETING} {ticket}\n").as_bytes())
            .await?;
        office.flush().await?;
        self.audit.record(
            "agent_started",
            json!({ "worker": name, "sandbox": w.sandbox, "resume": resume }),
        );
        let worker = name.to_string();
        tokio::spawn(async move {
            let _open = open;
            match tokio::io::copy_bidirectional(&mut agent, &mut office).await {
                Ok((to_office, to_agent)) => tracing::debug!(
                    worker = %worker,
                    to_office,
                    to_agent,
                    "agent stream ended"
                ),
                Err(e) => tracing::debug!(worker = %worker, "agent stream: {e}"),
            }
        });
        Ok(())
    }

    /// The driver's JWKS.
    pub async fn jwks(&self) -> Result<Value> {
        let body = self.gw.jwks().await?;
        serde_json::from_slice(&body).map_err(|e| anyhow!("the driver's JWKS isn't JSON: {e}"))
    }

    /// Records a refused call.
    fn refused(&self, route: &str, e: &anyhow::Error) {
        self.audit
            .record("refused", json!({ "route": route, "error": format!("{e:#}") }));
    }
}

/// A route of the warden's.
#[derive(Debug, Clone, PartialEq)]
pub enum Route {
    List,
    Put(String),
    Delete(String),
    Agent(String),
    Jwks,
    /// A path it serves, with a method it doesn't: the methods it takes.
    Method(&'static str),
}

/// The route `method` and `path` name, if the warden serves `path`.
pub fn route(method: &Method, path: &str) -> Option<Route> {
    let segs: Vec<&str> = path.trim_end_matches('/').split('/').skip(1).collect();
    let m = method.clone();
    Some(match segs.as_slice() {
        ["workers"] if m == Method::GET => Route::List,
        ["workers"] => Route::Method("GET"),
        ["workers", name] if m == Method::PUT => Route::Put(name.to_string()),
        ["workers", name] if m == Method::DELETE => Route::Delete(name.to_string()),
        ["workers", _] => Route::Method("PUT, DELETE"),
        ["workers", name, "agent"] if m == Method::POST => Route::Agent(name.to_string()),
        ["workers", _, "agent"] => Route::Method("POST"),
        ["jwks"] if m == Method::GET => Route::Jwks,
        ["jwks"] => Route::Method("GET"),
        _ => return None,
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PutBody {
    profile: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentBody {
    ticket: String,
    #[serde(default)]
    resume: Option<String>,
}

/// `r` as the warden's answer: JSON, a refusal's status and why, or 502
/// for what went wrong at the gateway.
fn answer(r: Result<(StatusCode, Value)>) -> Response<Body> {
    match r {
        Ok((status, v)) => edge::json_response(status, &v),
        Err(e) => edge::json_response(status_of(&e), &json!({ "error": format!("{e:#}") })),
    }
}

/// An error's status: a refusal's own, else 502 (it went wrong at the
/// gateway).
pub fn status_of(e: &anyhow::Error) -> StatusCode {
    e.downcast_ref::<Refusal>()
        .map(Refusal::status)
        .unwrap_or(StatusCode::BAD_GATEWAY)
}

/// A request's body as `T`.
async fn body<T: serde::de::DeserializeOwned>(req: Request<Incoming>) -> Result<T> {
    let bytes = edge::read_body(req, MAX_BODY)
        .await
        .ok_or_else(|| Refusal::Bad("the body is too large or broken".into()))?;
    serde_json::from_slice(&bytes).map_err(|e| Refusal::Bad(format!("the body: {e}")).into())
}

impl Warden {
    /// Serves `route` (the body, if it takes one, from `req`).
    pub async fn serve_route(&self, route: Route, req: Request<Incoming>) -> Response<Body> {
        let what = format!("{route:?}");
        let r: Result<(StatusCode, Value)> = match route {
            Route::Method(allow) => return edge::not_allowed(allow),
            Route::List => self
                .workers()
                .await
                .map(|w| (StatusCode::OK, json!({ "workers": w }))),
            Route::Put(name) => match body::<PutBody>(req).await {
                Ok(b) => self.put_worker(&name, &b.profile).await.map(|(w, made)| {
                    let status = if made { StatusCode::CREATED } else { StatusCode::OK };
                    (status, json!({ "worker": w, "made": made }))
                }),
                Err(e) => Err(e),
            },
            Route::Delete(name) => self
                .delete_worker(&name)
                .await
                .map(|()| (StatusCode::OK, json!({ "deleted": name }))),
            Route::Agent(name) => match body::<AgentBody>(req).await {
                Ok(b) => self
                    .start_agent(&name, &b.ticket, b.resume.as_deref())
                    .await
                    .map(|()| (StatusCode::OK, json!({ "started": name }))),
                Err(e) => Err(e),
            },
            Route::Jwks => self.jwks().await.map(|v| (StatusCode::OK, v)),
        };
        if let Err(e) = &r {
            self.refused(&what, e);
        }
        answer(r)
    }
}

impl Backend for Warden {
    fn name(&self) -> &str {
        "warden"
    }

    /// Only the office sandbox: no other sandbox gets a token, or is served
    /// with one.
    fn caller(&self, uuid: &str) -> Option<Caller> {
        (uuid == self.office).then(|| Caller {
            session: names::OFFICE.to_string(),
            labels: BTreeMap::from([(LABEL_ROLE.to_string(), "office".to_string())]),
        })
    }

    fn serve<'a>(
        &'a self,
        _caller: &'a Caller,
        path: &'a str,
        req: Request<Incoming>,
    ) -> Option<BoxFuture<'a, Response<Body>>> {
        let route = route(req.method(), path)?;
        Some(Box::pin(self.serve_route(route, req)))
    }
}
