//! `zapper-workbench`: serve the drive and repository to OpenShell
//! sandboxes, and apply the repository to the gateway as templates.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use zapper_openshell::edge::Edge;
use zapper_openshell::OpenShell;
use zapper_workbench::apply::Applier;
use zapper_workbench::backend::Workbench;
use zapper_workbench::drive::Drive;
use zapper_workbench::repo::Repo;
use zapper_workbench::settings::Settings;

#[derive(Parser)]
#[command(
    name = "zapper-workbench",
    about = "One person's drive, repository and sandbox templates on OpenShell"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Serve the drive and the repository to the gateway's sandboxes, on the
    /// workbench's edge.
    Serve,
    /// Apply a commit of the repository (`main` by default) to the gateway:
    /// its images built where they changed, each profile a template.
    Apply {
        #[arg(default_value = "main")]
        r#ref: String,
    },
    /// What applying a branch or commit would build and make, without doing it.
    Check { r#ref: String },
    /// The commit applied last, and where the drive and repository are.
    Status,
}

struct Ctx {
    settings: Settings,
    drive: Drive,
    repo: Repo,
    applier: Applier,
}

fn ctx() -> Result<Ctx> {
    let settings = Settings::load(&zapper_workbench::config_dir().join("workbench.yaml"))?;
    let data = zapper_workbench::data_dir();
    let drive = Drive::new(data.join("drive"))?;
    let repo = Repo::new(data.clone())?;
    let agent = settings
        .agent
        .clone()
        .unwrap_or_else(|| PathBuf::from(zapper_policy::expand_home("~/.local/bin/zapper-agent")));
    let applier = Applier::new(repo.path(), data.join("applied"), agent);
    std::fs::create_dir_all(data.join("applied"))?;
    Ok(Ctx {
        settings,
        drive,
        repo,
        applier,
    })
}

async fn connect(settings: &Settings) -> Result<OpenShell> {
    let path = settings.openshell.clone().unwrap_or_else(|| {
        PathBuf::from(zapper_policy::expand_home(
            "~/.config/zapper/openshell.yaml",
        ))
    });
    let mut s = zapper_openshell::Settings::load(&path)?;
    s.edge = settings.edge.clone();
    // The workbench's profiles are its repository's: no zapper config.
    OpenShell::connect(s, PathBuf::from("/nonexistent/zapper.yaml")).await
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let cli = Cli::parse();
    let c = ctx()?;
    match cli.cmd {
        Cmd::Serve => {
            let os = Arc::new(connect(&c.settings).await?);
            let listen = c.settings.edge.listen;
            let listener = tokio::net::TcpListener::bind(listen)
                .await
                .with_context(|| format!("binding the workbench's edge on {listen}"))?;
            let backend = Arc::new(Workbench::new(os, c.drive, c.repo.clone()));
            let edge = Arc::new(Edge::new(c.settings.edge.clone(), backend));
            eprintln!(
                "zapper-workbench: the edge on http://{listen} (sandboxes: {}/drive/ and {}/git/{})",
                c.settings.edge.url,
                c.settings.edge.url,
                zapper_workbench::repo::REPO
            );
            eprintln!(
                "zapper-workbench: the repository is {}; clone it, and push main",
                c.repo.path().display()
            );
            tokio::select! {
                _ = edge.serve(listener) => {}
                _ = tokio::signal::ctrl_c() => {}
            }
            Ok(())
        }
        Cmd::Apply { r#ref } => {
            let os = connect(&c.settings).await?;
            let v = c.applier.apply(&os, &r#ref, &|l| eprintln!("{l}")).await?;
            println!("{}", serde_json::to_string_pretty(&v)?);
            Ok(())
        }
        Cmd::Check { r#ref } => {
            println!(
                "{}",
                serde_json::to_string_pretty(&c.applier.check(&r#ref)?)?
            );
            Ok(())
        }
        Cmd::Status => {
            let mut v = c.applier.status()?;
            v["repository"] = serde_json::json!(c.repo.path());
            v["drive"] = serde_json::json!(zapper_workbench::data_dir().join("drive"));
            v["edge"] = serde_json::json!(c.settings.edge.listen.to_string());
            println!("{}", serde_json::to_string_pretty(&v)?);
            Ok(())
        }
    }
}
