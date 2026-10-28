//! `zapper github` (set a GitHub App up, look at it) and `zapper mint`
//! (print a short-lived token for a repository). Neither needs the daemon.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::{Args, Subcommand};
use serde_json::json;
use zapper_github::{app_file, owner_file, setup, App, GitHub, Scope};

#[derive(Subcommand)]
pub enum GithubCmd {
    /// Create a GitHub App for an account if it has none, and make sure it is installed there.
    Setup {
        #[command(flatten)]
        who: Who,
        /// The App's name on GitHub, which must be unique there (default: zapper-<owner>).
        #[arg(long)]
        name: Option<String>,
        /// Also give the App write on these (comma-separated): checks, statuses, actions, workflows.
        #[arg(long, value_delimiter = ',')]
        with: Vec<String>,
        /// Let the App be installed on any account, not only the owner's.
        #[arg(long)]
        public: bool,
        /// Local port that receives GitHub's redirect.
        #[arg(long, default_value_t = 8765)]
        port: u16,
        /// An App made elsewhere (agentctl's credentials file has the same layout): use it instead of creating one.
        #[arg(long, value_name = "FILE")]
        credentials: Option<PathBuf>,
    },
    /// The App for an account: its installation, what it may do and the repositories it reaches.
    Status {
        #[command(flatten)]
        who: Who,
    },
}

#[derive(Args)]
pub struct Who {
    /// The organisation the App lives in.
    #[arg(long, conflicts_with = "user", required_unless_present = "user")]
    org: Option<String>,
    /// Your GitHub login, for an App under your own account.
    #[arg(long)]
    user: Option<String>,
}

impl Who {
    fn owner(&self) -> &str {
        self.org
            .as_deref()
            .or(self.user.as_deref())
            .unwrap_or_default()
    }
}

#[derive(Subcommand)]
pub enum MintCmd {
    /// Print `{"value": ..., "expires_at": ...}`: a token for one or more of an account's repositories, from its App. Run by the daemon.
    GithubApp {
        /// The App's credentials file, a plain NAME.yaml in ~/.config/zapper/github-apps.
        #[arg(long, value_name = "FILE")]
        app: String,
        /// A repository, OWNER/NAME; repeat for more, all of the account's. Every
        /// repository gets every permission.
        #[arg(long = "repo", value_name = "OWNER/NAME", required = true)]
        repos: Vec<String>,
        /// A permission as name=read|write|admin; repeat for more.
        #[arg(long = "perm", value_name = "NAME=LEVEL", required = true)]
        perms: Vec<String>,
    },
}

pub async fn github(cmd: GithubCmd) -> Result<i32> {
    match cmd {
        GithubCmd::Setup {
            who,
            name,
            with,
            public,
            port,
            credentials,
        } => {
            let mut o = setup::Options::new(who.owner(), who.user.is_some());
            if let Some(n) = name {
                o.name = n;
            }
            (o.with, o.public, o.port, o.credentials) = (with, public, port, credentials);
            let (tx, rx) = tokio::sync::mpsc::channel(4);
            tokio::spawn(async move {
                use tokio::io::AsyncBufReadExt;
                let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    if tx.send(l).await.is_err() {
                        break;
                    }
                }
            });
            setup::run(o, &mut std::io::stdout(), rx).await?;
            println!(
                "\nNext: zapper github status --{} {}",
                if who.user.is_some() { "user" } else { "org" },
                who.owner()
            );
            Ok(0)
        }
        GithubCmd::Status { who } => status(who.owner()).await,
    }
}

async fn status(owner: &str) -> Result<i32> {
    let file = owner_file(owner)?;
    if !file.exists() {
        bail!(
            "no GitHub App for {owner}: {} doesn't exist; create it with `zapper github setup`",
            file.display()
        );
    }
    let app = App::load(&file, &mut |w| eprintln!("{w}"))?;
    println!(
        "App credentials: {}{}",
        file.display(),
        app.slug
            .as_deref()
            .map(|s| format!(" ({s})"))
            .unwrap_or_default()
    );
    let gh = GitHub::default();
    let Some(inst) = gh.installation_for_owner(&app, owner).await? else {
        println!(
            "Not installed on {owner}. Install it: https://github.com/apps/{}/installations/new",
            app.slug.as_deref().unwrap_or("<app>")
        );
        return Ok(1);
    };
    println!(
        "Installed on {owner}: installation {}, repositories: {}",
        inst.id, inst.repository_selection
    );
    let perms: Vec<String> = inst
        .permissions
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    println!(
        "Permissions (the ceiling for every token): {}",
        perms.join(", ")
    );
    if inst.repository_selection == "all" {
        println!(
            "warning: installed on all repositories; limit it to the ones agents should reach."
        );
    }
    let repos = gh.installation_repositories(&app, inst.id).await?;
    println!("Reaches {} repositories:", repos.len());
    for r in &repos {
        println!("  {r}");
    }
    Ok(0)
}

pub async fn mint(cmd: MintCmd) -> Result<i32> {
    let MintCmd::GithubApp { app, repos, perms } = cmd;
    let mut permissions = BTreeMap::new();
    for p in &perms {
        let (k, v) = p
            .split_once('=')
            .with_context(|| format!("{p:?}: want NAME=LEVEL"))?;
        permissions.insert(k.to_string(), v.to_string());
    }
    let scope = Scope::new(&repos, permissions)?;
    // GitHub finds the installation by the repository's owner, so a
    // repository the App isn't installed on is refused there.
    let file = app_file(&app)?;
    if !file.exists() {
        bail!(
            "no GitHub App at {}; create it with `zapper github setup`",
            file.display()
        );
    }
    let app = App::load(&file, &mut |w| eprintln!("{w}"))?;
    let token = GitHub::default().mint(&app, &scope).await?;
    // The token and nothing else on stdout; never on stderr.
    println!(
        "{}",
        json!({ "value": token.value, "expires_at": token.expires_at })
    );
    Ok(0)
}
