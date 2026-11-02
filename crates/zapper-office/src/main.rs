//! `zapper-office`: the office as a process of its own, a client of the
//! zapper daemon's API over its unix socket (proposal 13, step 4), the way
//! `zapper-workbench` is. It drives firms over the daemon's API (`Remote`
//! `OfficeClient`) and serves its own dashboard; `zapper serve` spawns it
//! under `OFFICE=1` (P4-d).
//!
//! The office never crosses the VM boundary — it is a client of the daemon's
//! API — so this path is validated by unit tests against a `TestCore`-backed
//! API, not KVM. `make test-e2e-office` on a real host is the final gate.

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use hyper::Method;
use zapper_client::{socket_path, Zapper};

mod cli;
mod firm_cli;

use cli::Client;

/// The starter `office.yaml` `init` writes: the firm config (empty), as the
/// checkout's `config/office.yaml`.
const OFFICE_EXAMPLE: &str = include_str!("../../../config/office.yaml");

#[derive(Parser)]
#[command(name = "zapper-office", about = "The Office as a process of its own")]
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
    /// Serve the office's own dashboard and board actions, driving firms over
    /// the zapper daemon's socket.
    Serve {
        /// Where to listen. Default: 127.0.0.1:8099.
        #[arg(long)]
        listen: Option<SocketAddr>,
        /// The daemon's `state` directory (firms' runs live under
        /// `state/firms/`). The office and daemon share it by convention.
        /// Default: the `daemon.state` of the config (`config/zapper.yaml`,
        /// else `~/.config/zapper/zapper.yaml`; `$ZAPPER_CONFIG` overrides).
        #[arg(long)]
        state_dir: Option<PathBuf>,
        /// The daemon's `state/services` directory, where the office binds its
        /// facilities' sockets. Default: `<state_dir>/services`.
        #[arg(long)]
        socket_dir: Option<PathBuf>,
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
        } => {
            // Default to the daemon's config: its `state` dir, and `services`
            // under it — so `zapper-office serve` needs no args beside the same
            // config the daemon reads.
            let state_dir = match state_dir {
                Some(d) => d,
                None => cli::daemon_state_dir()?,
            };
            let socket_dir = socket_dir.unwrap_or_else(|| state_dir.join("services"));
            serve(listen.unwrap_or_else(default_listen), state_dir, socket_dir).await
        }
        Cmd::Firm { cmd } => std::process::exit(firm_cli::firm(&Client::new(), cmd, json).await?),
        Cmd::Mail { cmd } => std::process::exit(firm_cli::mail(&Client::new(), cmd, json).await?),
        Cmd::Drive { cmd } => std::process::exit(firm_cli::drive(&Client::new(), cmd, json).await?),
        Cmd::Config { cmd } => {
            std::process::exit(firm_cli::config(&Client::new(), cmd, json).await?)
        }
    }
}

async fn serve(listen: SocketAddr, state_dir: PathBuf, socket_dir: PathBuf) -> Result<()> {
    // Confirm the daemon is reachable over its socket before serving.
    let zapper = Zapper::new(socket_path());
    let answer = zapper
        .call(Method::GET, "/v1/sessions", None)
        .await
        .context("reaching the zapper daemon")?;
    if !answer.status.is_success() {
        bail!(
            "the zapper daemon answered {} for /v1/sessions",
            answer.status
        );
    }
    eprintln!(
        "zapper-office: connected to the daemon at {}",
        socket_path().display()
    );

    let tcp = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("binding {listen}"))?;
    // Loopback listeners get the strict Host check; off-loopback loosens it,
    // as the daemon's `daemon.ui` does.
    let remote = !listen.ip().is_loopback();
    let office_config = cli::office_config_path();
    zapper_office::standalone::serve(tcp, zapper, state_dir, socket_dir, office_config, remote)
        .await
}
