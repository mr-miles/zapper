//! `zapper ssh`: SSH into sandboxes. `on` and `off` set whether a sandbox
//! takes SSH; `proxy` is the gateway (SSH on its stdin and stdout, the
//! host's sshd running it as a key's forced command); `config` prints what
//! to put where.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use clap::Subcommand;
use hyper::Method;
use serde_json::json;
use zapper_policy::expand_home;

use crate::client::Client;

#[derive(Subcommand)]
pub enum SshCmd {
    /// Let SSH reach a sandbox, while it runs.
    On { id: String },
    /// Stop SSH reaching a sandbox: refused from now on, and the terminals
    /// SSH has open in it closed (a persistent one is detached).
    Off { id: String },
    /// The gateway: speaks SSH on stdin and stdout, for a sandbox named
    /// `ID` or `ID.zapper` (else by `$SSH_ORIGINAL_COMMAND`, as the host's
    /// sshd sets it for a key's forced command).
    Proxy {
        target: Option<String>,
        /// The key's name, recorded with each terminal it opens.
        #[arg(long)]
        key: Option<String>,
    },
    /// Print the lines that set SSH up: `~/.ssh/config` and `known_hosts` on
    /// your machine, `authorized_keys` on this host.
    Config {
        /// This host as your machine reaches it (`user@host`); without it,
        /// the lines are for using SSH on this host itself.
        #[arg(long)]
        host: Option<String>,
    },
}

/// The gateway's host key: one for every sandbox, which `HostKeyAlias
/// zapper` lets clients check as one host.
fn host_key_path() -> PathBuf {
    PathBuf::from(expand_home("~/.config/zapper/ssh_host_ed25519"))
}

pub async fn ssh(c: &Client, cmd: SshCmd, json_out: bool) -> Result<i32> {
    match cmd {
        SshCmd::On { id } => switch(c, &id, true, json_out).await?,
        SshCmd::Off { id } => switch(c, &id, false, json_out).await?,
        SshCmd::Proxy { target, key } => proxy(c, target, key).await?,
        SshCmd::Config { host } => config(host)?,
    }
    Ok(0)
}

async fn switch(c: &Client, id: &str, enabled: bool, json_out: bool) -> Result<()> {
    let body = json!({ "enabled": enabled }).to_string();
    let v = c
        .json(Method::POST, &format!("/v1/sessions/{id}/ssh"), Some(body))
        .await?;
    if json_out {
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else if enabled {
        eprintln!("zapper: SSH reaches {id}: `ssh {id}.zapper`");
    } else {
        eprintln!("zapper: SSH no longer reaches {id}; its SSH terminals are closed");
    }
    Ok(())
}

/// A sandbox reached through the daemon's API, as the key `key`.
struct Api {
    c: Client,
    id: String,
    key: Option<String>,
}

impl zapper_ssh::Sandbox for Api {
    async fn terminal(&self, t: zapper_ssh::Terminal) -> Result<Box<dyn zapper_ssh::Stream>> {
        let io = self
            .c
            .ssh_terminal(&self.id, t.tty, &t.argv, &t.env, self.key.as_deref())
            .await?;
        Ok(Box::new(io))
    }
}

async fn proxy(c: &Client, target: Option<String>, key: Option<String>) -> Result<()> {
    // Stdout is the SSH stream: what goes wrong is said on stderr, which the
    // client shows.
    let target = target
        .or_else(|| std::env::var("SSH_ORIGINAL_COMMAND").ok())
        .context("no sandbox named: `ssh ID.zapper`")?;
    let id = zapper_ssh::sandbox_id(&target)?;
    let info = c
        .json(Method::GET, &format!("/v1/sessions/{id}"), None)
        .await?;
    if info["ssh"] != true {
        bail!("SSH is off for sandbox {id} (`zapper ssh on {id}` on the host)");
    }
    let host_key = zapper_ssh::host_key(&host_key_path())?;
    let stdio = tokio::io::join(tokio::io::stdin(), tokio::io::stdout());
    let sandbox = Arc::new(Api {
        c: Client::new(),
        id,
        key,
    });
    zapper_ssh::serve(sandbox, host_key, stdio).await
}

fn config(host: Option<String>) -> Result<()> {
    let key = zapper_ssh::host_key(&host_key_path())?;
    let exe = std::env::current_exe()?;
    let exe = exe.display();
    let socket = crate::client::socket_path();
    match host {
        Some(host) => {
            println!("# On your machine, in ~/.ssh/config:");
            println!("Host *.zapper");
            println!("    ProxyCommand ssh -T {host} %h");
            println!("    HostKeyAlias zapper");
            println!("    User user");
            println!();
            println!("# On your machine, in ~/.ssh/known_hosts:");
            println!("{}", zapper_ssh::known_hosts_line(&key)?);
            println!();
            println!(
                "# On this host, in ~/.ssh/authorized_keys: each key that may reach sandboxes,"
            );
            println!("# NAME the key's name in the record (it reaches nothing else on this host):");
            println!(
                "restrict,command=\"ZAPPER_SOCKET={} {exe} ssh proxy --key NAME\" ssh-ed25519 AAAA… NAME",
                socket.display()
            );
        }
        None => {
            println!("# In ~/.ssh/config:");
            println!("Host *.zapper");
            println!("    ProxyCommand {exe} ssh proxy %h");
            println!("    HostKeyAlias zapper");
            println!("    User user");
            println!();
            println!("# In ~/.ssh/known_hosts:");
            println!("{}", zapper_ssh::known_hosts_line(&key)?);
        }
    }
    println!();
    println!(
        "# Then, for a sandbox with SSH on (`zapper run --ssh`, `zapper ssh on ID`): ssh ID.zapper"
    );
    Ok(())
}
