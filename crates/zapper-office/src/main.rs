//! `zapper-office`: the office as a process of its own, a client of the
//! zapper daemon's API over its unix socket (proposal 13, step 4), the way
//! `zapper-workbench` is.
//!
//! WORK IN PROGRESS. This binary stands up the shell of the separate
//! process — its own listener and its client of the daemon — but the firm
//! run-loop is not yet ported off the in-process `Core`, so the office is
//! still run in-process by `zapper serve` (feature `office`). The remaining
//! conversion is tracked in `docs/office-process-assumptions.md` (step 4):
//! each `&Core` call becomes a `client::Zapper` API call or the office's own
//! state, and `named_layer`/`add_rule_keeper` (the in-process couplings) move
//! onto the API's layer/rule routes.

use std::net::SocketAddr;

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
    /// Serve the office's own dashboard and board actions, over the zapper
    /// daemon's socket.
    Serve {
        /// Where to listen. Default: 127.0.0.1:8099.
        #[arg(long)]
        listen: Option<SocketAddr>,
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
        Cmd::Serve { listen } => serve(listen.unwrap_or_else(default_listen)).await,
    }
}

async fn serve(listen: SocketAddr) -> Result<()> {
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
    eprintln!(
        "zapper-office: WIP — the firm run-loop is not yet ported off the in-process Core; \
         the office still runs in-process via `zapper serve`."
    );

    let app = axum::Router::new().route("/", axum::routing::get(root));
    let tcp = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("binding {listen}"))?;
    eprintln!("zapper-office: placeholder dashboard on http://{listen}");
    axum::serve(tcp, app).await?;
    Ok(())
}

async fn root() -> &'static str {
    "zapper-office (client mode) — work in progress.\n\
     The firm run-loop is still the in-process office run by `zapper serve`.\n\
     See docs/office-process-assumptions.md (step 4).\n"
}
