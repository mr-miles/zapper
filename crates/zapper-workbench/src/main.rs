//! `zapper-workbench`: serve the workbench, and keep its tokens.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use zapper_workbench::facility::{self, Facilities};
use zapper_workbench::gateway::{self, Ctx};
use zapper_workbench::tokens::{Right, Tokens};
use zapper_workbench::zapper::{socket_path, Zapper};

#[derive(Parser)]
#[command(
    name = "zapper-workbench",
    about = "A layer for single users on zapper"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

const DEFAULT_LISTEN: &str = "127.0.0.1:7880";

#[derive(Subcommand)]
enum Cmd {
    /// Serve the workbench's API and UI, over the zapper daemon's socket.
    Serve {
        /// Where to listen. Beyond loopback it is plain HTTP: for a trusted
        /// network, or behind a TLS proxy.
        #[arg(long, default_value = DEFAULT_LISTEN)]
        listen: SocketAddr,
    },
    /// Print the UI's address (sign in there with a token).
    Url {
        #[arg(long, default_value = DEFAULT_LISTEN)]
        listen: SocketAddr,
    },
    /// The workbench's tokens.
    Token {
        #[command(subcommand)]
        cmd: TokenCmd,
    },
    /// What the host's sshd runs for a key the workbench added: SSH on
    /// stdin and stdout, for the sandbox named (else by
    /// `$SSH_ORIGINAL_COMMAND`), if the key reaches it.
    SshProxy {
        target: Option<String>,
        #[arg(long)]
        key: String,
    },
}

#[derive(Subcommand)]
enum TokenCmd {
    /// Make a token, printed once: for sessions with the labels given (all
    /// sessions without), with the rights given.
    Add {
        name: String,
        /// `key=value`: the token reaches sessions with every one, and the
        /// sessions it creates get them (repeatable).
        #[arg(short, long = "label")]
        labels: Vec<String>,
        /// Comma-separated: read, create, control, terminal, answer, rules.
        #[arg(long, value_delimiter = ',', default_value = "read")]
        rights: Vec<String>,
    },
    /// Print the owner's token, alone on stdout (to sign in to the UI).
    Show,
    /// List the tokens (never their values).
    Ls,
    /// Remove a token: refused from then on.
    Rm { name: String },
}

/// The UI's address: never with a token in it, which would be kept in
/// histories and logs.
fn url(listen: SocketAddr) -> String {
    let host = if listen.ip().is_unspecified() {
        format!("127.0.0.1:{}", listen.port())
    } else {
        listen.to_string()
    };
    format!("http://{host}/")
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "zapper_workbench=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    let tokens = Tokens::open(zapper_workbench::config_dir())?;
    match cli.cmd {
        Cmd::Serve { listen } => {
            let work = zapper_workbench::data_dir().join("work");
            std::fs::create_dir_all(&work)?;
            let ctx = Ctx {
                zapper: Zapper::new(socket_path()),
                tokens: tokens.clone(),
                facilities: Arc::new(Facilities::new(vec![
                    Arc::new(zapper_workbench::secrets::Secrets::new(
                        zapper_workbench::config_dir().join("secrets"),
                    )?),
                    Arc::new(zapper_workbench::ssh::Ssh::new(
                        zapper_workbench::ssh_keys(&std::env::current_exe()?)?,
                        zapper_ssh::known_hosts_line(&zapper_ssh::host_key(
                            &zapper_workbench::ssh_host_key(),
                        )?)?,
                    )),
                ])?),
                work,
                port: listen.port(),
                remote: !listen.ip().is_loopback(),
            };
            facility::register(&ctx).await?;
            if ctx.facilities.iter().any(|f| f.wants_events()) {
                tokio::spawn(facility::follow(ctx.clone()));
            }
            let listener = tokio::net::TcpListener::bind(listen)
                .await
                .with_context(|| format!("listening on {listen}"))?;
            if ctx.remote {
                eprintln!("zapper-workbench: listening on {listen}, beyond loopback, in plain HTTP: for a trusted network or behind a TLS proxy");
            }
            eprintln!("zapper-workbench: the UI is at {}", url(listen));
            // The owner's token, the first time only: a service's output is
            // logged, and this would be at each start.
            if tokens.owner_is_new() {
                eprintln!("zapper-workbench: sign in with the owner's token, made now (again: `zapper-workbench token show`):");
                eprintln!("{}", tokens.owner()?);
            } else {
                eprintln!(
                    "zapper-workbench: sign in with the token `zapper-workbench token show` prints"
                );
            }
            axum::serve(listener, gateway::app(ctx)).await?;
        }
        Cmd::Url { listen } => println!("{}", url(listen)),
        Cmd::SshProxy { target, key } => {
            let keys = zapper_workbench::ssh_keys(&std::env::current_exe()?)?;
            zapper_workbench::ssh::proxy(
                Zapper::new(socket_path()),
                &keys,
                &key,
                target,
                &zapper_workbench::ssh_host_key(),
            )
            .await?
        }
        Cmd::Token { cmd } => match cmd {
            TokenCmd::Add {
                name,
                labels,
                rights,
            } => {
                let labels: BTreeMap<String, String> = labels
                    .iter()
                    .map(|l| {
                        l.split_once('=')
                            .map(|(k, v)| (k.to_string(), v.to_string()))
                            .with_context(|| format!("label {l:?}: key=value"))
                    })
                    .collect::<Result<_>>()?;
                let rights: BTreeSet<Right> = rights
                    .iter()
                    .map(|r| Right::parse(r.trim()))
                    .collect::<Result<_>>()?;
                let token = tokens.add(&name, labels, rights)?;
                eprintln!("zapper-workbench: token {name}, shown this once:");
                println!("{token}");
            }
            TokenCmd::Show => println!("{}", tokens.owner()?),
            TokenCmd::Ls => {
                println!("{:<16} {:<24} RIGHTS", "NAME", "SESSIONS");
                println!("{:<16} {:<24} all", "owner", "all");
                for k in tokens.list()? {
                    let labels = if k.labels.is_empty() {
                        "all".to_string()
                    } else {
                        k.labels
                            .iter()
                            .map(|(k, v)| format!("{k}={v}"))
                            .collect::<Vec<_>>()
                            .join(",")
                    };
                    let rights: Vec<String> = k
                        .rights
                        .iter()
                        .map(|r| {
                            serde_json::to_value(r)
                                .unwrap()
                                .as_str()
                                .unwrap()
                                .to_string()
                        })
                        .collect();
                    println!("{:<16} {:<24} {}", k.name, labels, rights.join(","));
                }
            }
            TokenCmd::Rm { name } => {
                if !tokens.remove(&name)? {
                    bail!("no token {name}");
                }
                eprintln!("zapper-workbench: token {name} removed");
            }
        },
    }
    Ok(())
}
