//! `zapper-warden`: zapperclaw's host side. `apply` makes the templates,
//! `serve` makes the office sandbox if it isn't there and serves the
//! office's routes and the two forwards, `status` lists what it made,
//! `check` prepares the profiles without touching the gateway.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde_json::json;
use zapper_openshell::edge::Edge;
use zapper_openshell::{EdgeSettings, OpenShell};
use zapper_warden::audit::Audit;
use zapper_warden::config::Config;
use zapper_warden::gateway::Gateway;
use zapper_warden::profiles::Profiles;
use zapper_warden::warden::Warden;
use zapper_warden::{apply, forward, names};

#[derive(Parser)]
#[command(
    name = "zapper-warden",
    about = "zapperclaw's host side: worker sandboxes from fixed templates, for an office in a sandbox of its own"
)]
struct Cli {
    /// The warden's config (default ~/.config/zapperclaw/warden.yaml).
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Prepare the profiles the config names, without the gateway: what
    /// `apply` would make.
    Check,
    /// Make the templates: the office's and each worker profile's.
    Apply,
    /// Make the office sandbox if it isn't there, then serve its routes and
    /// the forwards into it, and keep the office's command running in it.
    Serve {
        /// Run nothing in the office sandbox (the end-to-end test's probe).
        #[arg(long)]
        idle_office: bool,
    },
    /// The office sandbox and the workers the warden made.
    Status,
}

struct Ctx {
    config: Config,
    settings: zapper_openshell::Settings,
    profiles: Profiles,
}

fn ctx(path: &Path) -> Result<Ctx> {
    let config = Config::load(path)?;
    let zapper_path = config.path(&config.zapper);
    if let Some(dir) = zapper_path.parent() {
        zapper_policy::set_config_dir(dir);
    }
    let text = std::fs::read_to_string(&zapper_path)
        .with_context(|| format!("reading {}", zapper_path.display()))?;
    let zapper = zapper_policy::Config::from_yaml(&text)
        .with_context(|| format!("loading {}", zapper_path.display()))?;
    let settings = zapper_openshell::Settings::load(&config.path(&config.openshell))?;
    let profiles = Profiles::prepare(&config, &zapper, &settings.edge.url)?;
    Ok(Ctx {
        config,
        settings,
        profiles,
    })
}

async fn connect(c: &Ctx) -> Result<OpenShell> {
    // The warden's profiles are its own: no zapper config for the client.
    OpenShell::connect(c.settings.clone(), None).await
}

/// The warden's own edge: the office's token, for the routes, with the
/// driver's identity settings of `openshell.yaml`.
fn control_edge(config: &Config, base: &EdgeSettings) -> EdgeSettings {
    EdgeSettings {
        listen: config.listen,
        url: config.url.clone(),
        token_url: config.token_url(),
        ..base.clone()
    }
}

async fn bind(addr: std::net::SocketAddr, what: &str) -> Result<tokio::net::TcpListener> {
    tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {what} on {addr}"))
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let cli = Cli::parse();
    let path = cli.config.unwrap_or_else(zapper_warden::default_config);
    let c = ctx(&path)?;
    match cli.cmd {
        Cmd::Check => {
            let all: Vec<_> = c
                .profiles
                .all()
                .map(|p| {
                    json!({
                        "template": p.template,
                        "profile": p.profile.name,
                        "image": p.profile.engine.image,
                        "kind": p.profile.agent.kind,
                        "providers": p.providers,
                        "proposals": p.proposals,
                        "left_out": p.setup.left_out,
                    })
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&all)?);
            Ok(())
        }
        Cmd::Apply => {
            let os = connect(&c).await?;
            let made = apply::apply(&os, &c.profiles, &|l| eprintln!("{l}")).await?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({ "templates": made }))?
            );
            Ok(())
        }
        Cmd::Status => {
            let os = connect(&c).await?;
            let office = os
                .gateway_sandboxes()
                .await?
                .into_iter()
                .find(|s| s.name == names::OFFICE)
                .map(|s| {
                    json!({
                        "sandbox": s.name,
                        "id": s.id,
                        "ready": s.ready,
                        "template": s.template,
                    })
                });
            let gw: Arc<dyn Gateway> = Arc::new(os);
            let id = office
                .as_ref()
                .and_then(|o| o["id"].as_str())
                .unwrap_or_default()
                .to_string();
            let w = Warden::new(gw, c.config.clone(), c.profiles.clone(), id, Audit::none());
            let workers = w.workers().await?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({ "office": office, "workers": workers }))?
            );
            Ok(())
        }
        Cmd::Serve { idle_office } => {
            let mut c = c;
            if idle_office {
                c.config.office.command.clear();
            }
            serve(c).await
        }
    }
}

async fn serve(c: Ctx) -> Result<()> {
    let state = c.config.path(&c.config.state);
    let audit = Arc::new(Audit::open(&state)?);
    let os = connect(&c).await?;
    let control = control_edge(&c.config, os.edge_settings());
    let workers_edge = os.edge_settings().listen;
    // Bind first: a port taken is an error before anything is made.
    let control_l = bind(control.listen, "the office's routes").await?;
    let edge_l = bind(workers_edge, "the workers' edge forward").await?;
    let board_l = bind(c.config.board, "the board's forward").await?;
    let gw: Arc<dyn Gateway> = Arc::new(os);
    let office = Warden::ensure_office(gw.as_ref(), &c.profiles, &audit).await?;
    let token = zapper_warden::office::board_token(&state)?;
    let ports = c.config.office.ports;
    let warden = Arc::new(Warden::new(
        gw.clone(),
        c.config.clone(),
        c.profiles.clone(),
        office.clone(),
        audit.clone(),
    ));
    let edge = Arc::new(Edge::new(control.clone(), warden));
    eprintln!(
        "zapper-warden: the office sandbox {} ({office}); its routes on http://{} ({} to it)",
        names::OFFICE,
        control.listen,
        control.url
    );
    eprintln!(
        "zapper-warden: workers' edge {workers_edge} -> office :{}, board http://{} -> office :{}",
        ports.edge, c.config.board, ports.board
    );
    if !c.config.office.command.is_empty() {
        eprintln!(
            "zapper-warden: running {:?} in the office sandbox; the board is http://{}/?token=$(cat {})",
            c.config.office.command,
            c.config.board,
            state.join(zapper_warden::office::BOARD_TOKEN).display()
        );
    }
    let running = zapper_warden::office::keep_running(
        gw.clone(),
        c.profiles.office.clone(),
        c.config.office.command.clone(),
        token,
        audit,
    );
    tokio::select! {
        _ = edge.serve(control_l) => {}
        _ = forward::serve(edge_l, gw.clone(), ports.edge, "workers' edge") => {}
        _ = forward::serve(board_l, gw.clone(), ports.board, "board") => {}
        _ = running => {}
        _ = tokio::signal::ctrl_c() => {}
    }
    Ok(())
}
