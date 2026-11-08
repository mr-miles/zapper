//! The profiles the warden makes sandboxes of, resolved once from the zapper
//! config and prepared as their templates and agents need them.
//!
//! A worker profile gets what the office gave each worker it made, but
//! once, in the template: the office's tools as an MCP server (Claude Code
//! speaks HTTP to the edge; other harnesses `office mcp`), its Stop hook
//! (`office stop-hook`), and `ZAPPER_OFFICE_URL` for the `office` command.
//! Its own instructions, skills and model, if the warden's config gives
//! them. Nothing per worker: a worker learns who it is from the edge.

use std::collections::BTreeMap;

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use zapper_openshell::{Labels, Setup, AGENT_PATH, OFFICE_MCP};
use zapper_policy::ResolvedProfile;

use crate::config::Config;
use crate::names;

/// A profile, ready to be a template and to run.
#[derive(Debug, Clone)]
pub struct Prepared {
    /// Its zapper profile, with the office's wiring.
    pub profile: ResolvedProfile,
    /// Its environment, files and setup commands.
    pub setup: Setup,
    /// The template its sandboxes are made from.
    pub template: String,
    pub labels: Labels,
    /// The gateway providers its sandboxes get.
    pub providers: Vec<String>,
    /// Whether its sandboxes may propose rules.
    pub proposals: bool,
}

/// The office's profile and the worker profiles, by name.
#[derive(Debug, Clone)]
pub struct Profiles {
    pub office: Prepared,
    pub workers: BTreeMap<String, Prepared>,
}

impl Profiles {
    /// The profiles `config` names, from the zapper config `zapper`; the
    /// workers' edge is at `edge_url` (`openshell.yaml`'s `edge.url`).
    pub fn prepare(
        config: &Config,
        zapper: &zapper_policy::Config,
        edge_url: &str,
    ) -> Result<Profiles> {
        let office = prepare_office(config, zapper)?;
        let mut workers = BTreeMap::new();
        for name in config.profiles.keys() {
            let p = prepare_worker(config, zapper, name, edge_url)
                .with_context(|| format!("worker profile {name}"))?;
            workers.insert(name.clone(), p);
        }
        Ok(Profiles { office, workers })
    }

    /// Every template, the office's first.
    pub fn all(&self) -> impl Iterator<Item = &Prepared> {
        std::iter::once(&self.office).chain(self.workers.values())
    }
}

/// The office's sandbox: its profile as it is, with where it reaches the
/// warden and where the warden's forwards reach it.
fn prepare_office(config: &Config, zapper: &zapper_policy::Config) -> Result<Prepared> {
    let name = &config.office.profile;
    let profile = zapper
        .resolve(name)
        .with_context(|| format!("office.profile {name}"))?;
    let ports = config.office.ports;
    let extra = BTreeMap::from([
        ("ZAPPERCLAW_WARDEN_URL".to_string(), config.url.clone()),
        ("ZAPPERCLAW_EDGE_PORT".to_string(), ports.edge.to_string()),
        ("ZAPPERCLAW_BOARD_PORT".to_string(), ports.board.to_string()),
        ("ZAPPERCLAW_AGENT_PORT".to_string(), ports.agent.to_string()),
    ]);
    let setup = Setup::of(&profile, extra)?;
    Ok(Prepared {
        profile,
        setup,
        template: names::OFFICE.to_string(),
        labels: names::office_labels(),
        providers: config.office.providers.clone(),
        proposals: false,
    })
}

/// Worker profile `name`, wired to the office's edge at `edge_url`.
fn prepare_worker(
    config: &Config,
    zapper: &zapper_policy::Config,
    name: &str,
    edge_url: &str,
) -> Result<Prepared> {
    let w = &config.profiles[name];
    let mut profile = zapper.resolve(name)?;
    // Only a harness the office can drive headless can be a worker.
    zapper_driver::harness::headless(&profile.agent.kind)?;
    let instructions = match &w.instructions {
        Some(p) => Some(
            std::fs::read_to_string(config.path(p))
                .with_context(|| format!("instructions {p}"))?,
        ),
        None => None,
    };
    let skills = w
        .skills
        .as_ref()
        .map(|p| config.path(p).to_string_lossy().into_owned());
    let overlay = overlay(instructions.as_deref(), skills.as_deref(), w.model.as_deref());
    let mut agent = serde_json::to_value(&profile.agent)?;
    zapper_policy::merge(&mut agent, &overlay);
    profile.agent = serde_json::from_value(agent).context("the office's wiring")?;
    let mcp = mcp_url(edge_url);
    zapper_openshell::office_over_http(&mut profile, &mcp);
    let extra = BTreeMap::from([
        ("ZAPPER_OFFICE_URL".to_string(), mcp),
        ("ZAPPERCLAW_PROFILE".to_string(), name.to_string()),
    ]);
    let setup = Setup::of(&profile, extra)?;
    Ok(Prepared {
        profile,
        setup,
        template: names::profile_template(name),
        labels: names::profile_labels(name),
        providers: w.providers.clone(),
        proposals: w.proposals,
    })
}

/// The office's tools as the workers reach them: `<edge>/mcp`.
pub fn mcp_url(edge_url: &str) -> String {
    format!("{}/mcp", edge_url.trim_end_matches('/'))
}

/// The office's wiring of a worker, in the harness's neutral terms, as
/// `zapper-office`'s `agent_overlay` gives it (but the per-worker preamble).
fn overlay(instructions: Option<&str>, skills: Option<&str>, model: Option<&str>) -> Value {
    let mut servers = serde_json::Map::new();
    servers.insert(
        OFFICE_MCP.to_string(),
        json!({ "command": AGENT_PATH, "args": ["office", "mcp"] }),
    );
    let mut o = json!({
        "mcp_servers": Value::Object(servers),
        "turn_end": format!("{AGENT_PATH} office stop-hook"),
    });
    if let Some(i) = instructions {
        o["instructions"] = json!(i);
    }
    if let Some(s) = skills {
        o["skills"] = json!(s);
    }
    if let Some(m) = model {
        o["model"] = json!(m);
    }
    o
}

/// What a worker's agent runs as: its harness's headless command, resuming
/// `resume` if given, with the template's environment and the worker's
/// sandbox, in its work directory. The warden's alone to say.
pub fn agent_command(
    p: &Prepared,
    sandbox: &str,
    resume: Option<&str>,
) -> Result<(Vec<String>, BTreeMap<String, String>, String)> {
    let h = zapper_driver::harness::headless(&p.profile.agent.kind)
        .map_err(|e| anyhow!("profile {}: {e:#}", p.profile.name))?;
    let argv = h.conversation(resume).argv();
    let mut env = p.setup.env.clone();
    env.insert("ZAPPER_SESSION".to_string(), sandbox.to_string());
    Ok((argv, env, p.profile.agent.workdir.clone()))
}
