//! Facilities: what the workbench offers besides zapper's own session views
//! (secrets, the drive, the configuration's repository, SSH keys, the
//! wizard). Each is a module behind one interface, so adding or removing
//! one is a build or config choice, and moving one into a process of its
//! own later is mechanical. Facilities don't talk to each other; the wizard
//! calls their routes as any client does.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Result};
use axum::Router;
use futures_util::StreamExt;
use hyper::{HeaderMap, Method};
use serde_json::{json, Value};

use crate::gateway::Ctx;
use crate::sse;

/// A facility's part of the UI: a section with its title, and the script
/// that fills it. The script is served as a file of its own
/// (`/panels/<name>.js`), and builds its elements, text as text, in the
/// element `[data-facility="<name>"]`, as the page's own scripts do: no
/// string is made markup or code.
#[derive(Debug, Clone, Copy)]
pub struct Panel {
    pub title: &'static str,
    pub script: &'static str,
}

/// What a facility gives the workbench.
pub trait Facility: Send + Sync + 'static {
    /// Its name: its routes are under `/w/v1/<name>/`.
    fn name(&self) -> &'static str;

    /// Its routes, behind the gateway: each request's `Grant` is in its
    /// extensions, for the facility to check against.
    fn routes(&self) -> Router<Ctx> {
        Router::new()
    }

    /// Its panel in the UI, if it has one.
    fn panel(&self) -> Option<Panel> {
        None
    }

    /// Services it registers in zapper when the workbench starts, each as
    /// `POST /v1/services` takes it (a name, a port, its own socket).
    fn services(&self) -> Vec<Value> {
        vec![]
    }

    /// Whether it follows zapper's events (`on_event`).
    fn wants_events(&self) -> bool {
        false
    }

    /// Each of zapper's events (`/v1/events`), as the stream carries it,
    /// for a facility that `wants_events`.
    fn on_event(&self, _ctx: &Ctx, _event: &Value) {}
}

/// Names the gateway's own routes have, which no facility may take.
const TAKEN: [&str; 7] = [
    "status",
    "profiles",
    "sessions",
    "events",
    "approvals",
    "facilities",
    "config",
];

/// The workbench's facilities.
#[derive(Clone, Default)]
pub struct Facilities(Vec<Arc<dyn Facility>>);

impl Facilities {
    pub fn new(list: Vec<Arc<dyn Facility>>) -> Result<Facilities> {
        let mut seen = std::collections::BTreeSet::new();
        for f in &list {
            let n = f.name();
            let word = n.bytes().next().is_some_and(|b| b.is_ascii_lowercase())
                && n.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
            if !word || TAKEN.contains(&n) || !seen.insert(n) {
                bail!("facility {n:?}: a lowercase word, its own, and not one of the gateway's");
            }
        }
        Ok(Facilities(list))
    }

    pub fn iter(&self) -> impl Iterator<Item = &Arc<dyn Facility>> {
        self.0.iter()
    }

    /// Their routes, each under its name.
    pub fn routes(&self) -> Router<Ctx> {
        self.0.iter().fold(Router::new(), |r, f| {
            r.nest(&format!("/{}", f.name()), f.routes())
        })
    }

    /// The script of the panel of the facility `name`.
    pub fn panel_script(&self, name: &str) -> Option<&'static str> {
        self.0
            .iter()
            .find(|f| f.name() == name)
            .and_then(|f| f.panel())
            .map(|p| p.script)
    }

    /// What the UI shows of them: each one's name, and its panel's title
    /// and script.
    pub fn view(&self) -> Value {
        Value::Array(
            self.0
                .iter()
                .map(|f| match f.panel() {
                    Some(p) => json!({ "name": f.name(), "title": p.title, "panel": format!("/panels/{}.js", f.name()) }),
                    None => json!({ "name": f.name() }),
                })
                .collect(),
        )
    }
}

/// Registers the facilities' services in zapper.
pub async fn register(ctx: &Ctx) -> Result<()> {
    for f in ctx.facilities.iter() {
        for spec in f.services() {
            ctx.zapper
                .call(Method::POST, "/v1/services", Some(&spec))
                .await?
                .ok()
                .map_err(|(s, why)| {
                    anyhow::anyhow!("facility {}: a service: {s} {why}", f.name())
                })?;
        }
    }
    Ok(())
}

/// Follows zapper's events for the facilities, for as long as the
/// workbench runs: again from where it left off when the stream drops
/// (`after`), so a restart of either side loses no event history still
/// holds.
pub async fn follow(ctx: Ctx) {
    let mut after: Option<String> = None;
    loop {
        let path = match &after {
            Some(a) => format!("/v1/events?after={a}"),
            None => "/v1/events".to_string(),
        };
        if let Ok(resp) = ctx.zapper.stream(&path, &HeaderMap::new()).await {
            if resp.status().is_success() {
                let mut events = sse::Blocks::new(resp.into_body());
                while let Some(block) = events.next().await {
                    if let Some(id) = &block.id {
                        after = Some(id.clone());
                    }
                    if let Some(event) = block.json() {
                        for f in ctx.facilities.iter() {
                            f.on_event(&ctx, &event);
                        }
                    }
                }
            } else {
                // A cursor from another daemon's run is still taken; one it
                // can't read starts afresh.
                after = None;
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}
