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
use zapper_office::client::{socket_path, Zapper};

#[derive(Parser)]
#[command(name = "zapper-office", about = "The Office as a process of its own")]
struct Cli {
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
        #[arg(long)]
        state_dir: PathBuf,
        /// The daemon's `state/services` directory, where the office binds its
        /// facilities' sockets.
        #[arg(long)]
        socket_dir: PathBuf,
    },
    /// Print the dashboard's address.
    Url {
        #[arg(long)]
        listen: Option<SocketAddr>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    match Cli::parse().cmd {
        Cmd::Url { listen } => {
            println!("http://{}", listen.unwrap_or_else(default_listen));
            Ok(())
        }
        Cmd::Serve {
            listen,
            state_dir,
            socket_dir,
        } => serve(listen.unwrap_or_else(default_listen), state_dir, socket_dir).await,
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
    zapper_office::standalone::serve(tcp, zapper, state_dir, socket_dir, remote).await
}
