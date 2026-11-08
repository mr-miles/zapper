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
mod github_cli;

use cli::Client;

/// The starter configs `init` writes, as the checkout's: the profiles and
/// image recipes (`zapper.yaml`), the firms (`office.yaml`) and the gateway
/// (`openshell.yaml`).
const EXAMPLES: &[(&str, &str)] = &[
    ("zapper.yaml", include_str!("../../../config/zapper.yaml")),
    ("office.yaml", include_str!("../../../config/office.yaml")),
    (
        "openshell.yaml",
        include_str!("../../../config/openshell.yaml"),
    ),
];

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
        /// The OpenShell settings: the gateway, its TLS, images, providers
        /// and the edge. Default: `openshell.yaml` beside the config.
        #[arg(long)]
        openshell: Option<PathBuf>,
    },
    /// Build the image of a recipe in the config's `images:`, for the
    /// sandboxes (`localhost/zapper-openshell-NAME`).
    Image {
        /// The recipe.
        name: String,
        /// A file or directory for one of the recipe's arguments: NAME=PATH.
        #[arg(long = "arg")]
        args: Vec<String>,
        /// The account commands run as (the profiles' `agent.user`).
        #[arg(long, default_value = "agent")]
        user: String,
        /// The static guest binary installed as `office`.
        #[arg(long, default_value = "target/release/zapper-agent")]
        agent: PathBuf,
    },
    /// Print the dashboard's address.
    Url {
        #[arg(long)]
        listen: Option<SocketAddr>,
    },
    /// Write the example configs (`zapper.yaml`, `office.yaml`,
    /// `openshell.yaml`) and the workers' default agents and skills into the
    /// config directory, keeping any file already there.
    Init,
    /// GitHub Apps: set one up for an account (a firm's GitHub remote), and
    /// look at it.
    Github {
        #[command(subcommand)]
        cmd: github_cli::GithubCmd,
    },
    /// Print a short-lived credential: what the office runs for a firm's
    /// GitHub remote's tokens.
    Mint {
        #[command(subcommand)]
        cmd: github_cli::MintCmd,
    },
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
        Cmd::Image {
            name,
            args,
            user,
            agent,
        } => {
            let path = cli::config_path();
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            let config = zapper_policy::Config::from_yaml(&text)
                .with_context(|| format!("loading {}", path.display()))?;
            let b = zapper_office::image::Build {
                name,
                args,
                user,
                agent: Some(agent),
                tag: None,
            };
            let tag = zapper_office::image::build(&config, &b, &|l| eprintln!("{l}"))?;
            println!("{tag}");
            Ok(())
        }
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
            // The configs, each kept if already there.
            std::fs::create_dir_all(&dir)?;
            for (name, text) in EXAMPLES {
                let path = if *name == "office.yaml" {
                    cli::office_config_path()
                } else {
                    dir.join(name)
                };
                if path.exists() {
                    println!("{} exists; not touching it", path.display());
                } else {
                    std::fs::write(&path, text)?;
                    println!("wrote {}", path.display());
                }
            }
            Ok(())
        }
        Cmd::Serve {
            listen,
            state_dir,
            openshell,
        } => {
            let state_dir = match state_dir {
                Some(d) => d,
                None => cli::state_dir()?,
            };
            let listen = listen.unwrap_or_else(default_listen);
            let settings = openshell.unwrap_or_else(|| cli::config_dir().join("openshell.yaml"));
            serve(listen, &settings, state_dir).await
        }
        Cmd::Github { cmd } => std::process::exit(github_cli::github(cmd).await?),
        Cmd::Mint { cmd } => std::process::exit(github_cli::mint(cmd).await?),
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
async fn serve(listen: SocketAddr, settings: &std::path::Path, state_dir: PathBuf) -> Result<()> {
    use zapper_office::openshell::{OpenShell, Settings};
    let settings = Settings::load(settings)?;
    let gateway = settings.gateway.clone();
    let os = OpenShell::connect(settings, Some(cli::config_path())).await?;
    os.require_officers().await?;
    for w in os.ignored() {
        eprintln!("zapper-office: zapper.yaml: {w}");
    }
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
        office_config,
        remote,
    )
    .await
}
