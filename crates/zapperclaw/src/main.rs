//! `zapperclaw`: the office in its sandbox.
//!
//!   zapperclaw check   the config, as it would run
//!   zapperclaw serve   the post office, its edge, the board and the agent port

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use zapperclaw::config::{Config, Env};
use zapperclaw::workers::{Tickets, Warden};
use zapperclaw::{board, edge, Office};

#[derive(Parser)]
#[command(
    name = "zapperclaw",
    about = "The office in its sandbox: a post office, a foreman per worker, a drive"
)]
struct Cli {
    /// The office's config (default ~/.config/zapperclaw/zapperclaw.yaml).
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Checks the config.
    Check,
    /// Runs the office.
    Serve,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "zapperclaw=info,zapper_openshell=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    let path = cli.config.unwrap_or_else(zapperclaw::config::default_path);
    let config = Config::load(&path)?;
    match cli.command {
        Command::Check => {
            for (name, m) in &config.workers {
                config.instructions(name)?;
                println!("{name}: profile {}", m.profile);
            }
            println!(
                "{} workers, {} turns at once, {} a turn; state in {}",
                config.workers.len(),
                config.max_concurrent,
                config.turn_timeout,
                config.path(&config.state).display()
            );
            Ok(())
        }
        Command::Serve => serve(config).await,
    }
}

async fn serve(config: Config) -> Result<()> {
    let env = Env::from(|k| std::env::var(k).ok())?;
    let tickets = Tickets::default();
    let warden = Warden::new(&env.warden, tickets.clone());
    let jwks = warden.jwks_url();
    let office = Office::new(config, Arc::new(warden))?;
    let bind = |port: u16| {
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
        async move {
            tokio::net::TcpListener::bind(addr)
                .await
                .with_context(|| format!("listening on {addr}"))
        }
    };
    let edge_l = bind(env.edge_port).await?;
    let board_l = bind(env.board_port).await?;
    let agent_l = bind(env.agent_port).await?;
    let token = match env.board_token.clone() {
        Some(t) => t,
        None => {
            let t = board::new_token();
            let p = office.dir.join("board-token");
            write_private(&p, &t)?;
            eprintln!("zapperclaw: the board's token is in {}", p.display());
            t
        }
    };
    let edge = Arc::new(edge::for_office(office.clone(), edge_l.local_addr()?, jwks));
    let board = board::Board::new(office.clone(), token);
    office.start();
    eprintln!(
        "zapperclaw: {} workers; the edge on {}, the board on {}, agents on {}",
        office.workers.len(),
        edge_l.local_addr()?,
        board_l.local_addr()?,
        agent_l.local_addr()?
    );
    tokio::select! {
        _ = edge.serve(edge_l) => {}
        _ = board.serve(board_l) => {}
        _ = tickets.serve(agent_l) => {}
        _ = tokio::signal::ctrl_c() => {}
    }
    office.stop();
    eprintln!("zapperclaw: stopped; the workers' sandboxes stay");
    Ok(())
}

fn write_private(p: &std::path::Path, text: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(p)
        .with_context(|| format!("writing {}", p.display()))?;
    f.write_all(text.as_bytes())?;
    Ok(())
}
