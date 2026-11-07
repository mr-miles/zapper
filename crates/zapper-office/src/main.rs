//! `zapper-office`: The Office, its firms' workers in sandboxes of an
//! OpenShell gateway (`docs/openshell-migration.md`). It serves its
//! dashboard, the board's actions and its CLI's socket, and the office edge
//! the workers reach its tools and git through.

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

mod cli;
mod firm_cli;

use cli::Client;

/// The starter `office.yaml` `init` writes: the firm config (empty), as the
/// checkout's `config/office.yaml`.
const OFFICE_EXAMPLE: &str = include_str!("../../../config/office.yaml");

#[derive(Parser)]
#[command(
    name = "zapper-office",
    about = "The Office: firms of AI workers on OpenShell"
)]
struct Cli {
    /// Print API responses as JSON.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

fn default_listen() -> SocketAddr {
    "127.0.0.1:8099".parse().unwrap()
}

#[derive(Subcommand)]
enum Cmd {
    /// Serve the office: its dashboard and the board's actions, with firms
    /// run on the OpenShell gateway `openshell.yaml` names.
    Serve {
        /// Where to listen. Default: 127.0.0.1:8099.
        #[arg(long)]
        listen: Option<SocketAddr>,
        /// The office's `state` directory (firms' runs live under
        /// `state/firms/`). Default: the `daemon.state` of the config
        /// (`config/zapper.yaml`, else `~/.config/zapper/zapper.yaml`;
        /// `$ZAPPER_CONFIG` overrides).
        #[arg(long)]
        state_dir: Option<PathBuf>,
        /// Where the office binds its sockets. Default: `<state_dir>/services`.
        #[arg(long)]
        socket_dir: Option<PathBuf>,
        /// The OpenShell settings: the gateway, its TLS, images, providers
        /// and the edge. Default: `openshell.yaml` beside the config.
        #[arg(long)]
        openshell: Option<PathBuf>,
    },
    /// Print the dashboard's address.
    Url {
        #[arg(long)]
        listen: Option<SocketAddr>,
    },
    /// Install the office workers' default agents and skills beside the
    /// config file (what firms read through `agents_dir`/`skills_dir`),
    /// keeping any file already there.
    Init,
    /// Firms of workers: start, stop, watch, replay.
    Firm {
        #[command(subcommand)]
        cmd: firm_cli::FirmCmd,
    },
    /// Mail as the board.
    Mail {
        #[command(subcommand)]
        cmd: firm_cli::MailCmd,
    },
    /// A firm's shared drive.
    Drive {
        #[command(subcommand)]
        cmd: firm_cli::DriveCmd,
    },
    /// The office's own config (`office.yaml`: the firms).
    Config {
        #[command(subcommand)]
        cmd: firm_cli::ConfigCmd,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let cli = Cli::parse();
    let json = cli.json;
    match cli.cmd {
        Cmd::Url { listen } => {
            println!("http://{}", listen.unwrap_or_else(default_listen));
            Ok(())
        }
        Cmd::Init => {
            let dir = cli::config_dir();
            let written = zapper_office::skills::install(&dir)?;
            if written.is_empty() {
                println!(
                    "{}: the default agents and skills are all there",
                    dir.display()
                );
            } else {
                println!(
                    "wrote agents and skills to {}: {}",
                    dir.display(),
                    written.join(", ")
                );
            }
            // The office's own firm config, kept if already there (like
            // `zapper init` keeps zapper.yaml).
            let office_yaml = cli::office_config_path();
            if office_yaml.exists() {
                println!("{} exists; not touching it", office_yaml.display());
            } else {
                if let Some(parent) = office_yaml.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&office_yaml, OFFICE_EXAMPLE)?;
                println!("wrote {}", office_yaml.display());
            }
            Ok(())
        }
        Cmd::Serve {
            listen,
            state_dir,
            socket_dir,
            openshell,
        } => {
            let state_dir = match state_dir {
                Some(d) => d,
                None => cli::daemon_state_dir()?,
            };
            let socket_dir = socket_dir.unwrap_or_else(|| state_dir.join("services"));
            let listen = listen.unwrap_or_else(default_listen);
            let settings = openshell.unwrap_or_else(|| cli::config_dir().join("openshell.yaml"));
            serve(listen, &settings, state_dir, socket_dir).await
        }
        Cmd::Firm { cmd } => std::process::exit(firm_cli::firm(&Client::new(), cmd, json).await?),
        Cmd::Mail { cmd } => std::process::exit(firm_cli::mail(&Client::new(), cmd, json).await?),
        Cmd::Drive { cmd } => std::process::exit(firm_cli::drive(&Client::new(), cmd, json).await?),
        Cmd::Config { cmd } => {
            std::process::exit(firm_cli::config(&Client::new(), cmd, json).await?)
        }
    }
}

/// The office on an OpenShell gateway: profiles from the zapper config,
/// sandboxes from the gateway in `settings`.
async fn serve(
    listen: SocketAddr,
    settings: &std::path::Path,
    state_dir: PathBuf,
    socket_dir: PathBuf,
) -> Result<()> {
    use zapper_office::openshell::{OpenShell, Settings};
    let settings = Settings::load(settings)?;
    let gateway = settings.gateway.clone();
    let os = OpenShell::connect(settings, cli::config_path()).await?;
    eprintln!("zapper-office: connected to the OpenShell gateway at {gateway}");
    let tcp = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("binding {listen}"))?;
    // Loopback listeners get the strict Host check; off-loopback loosens it.
    let remote = !listen.ip().is_loopback();
    let office_config = cli::office_config_path();
    zapper_office::standalone::serve(
        tcp,
        std::sync::Arc::new(os),
        state_dir,
        socket_dir,
        office_config,
        remote,
    )
    .await
}
