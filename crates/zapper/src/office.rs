//! The office in the binary (feature `office`): its commands, its part of
//! the config, `init`, `check` and `serve`. Without the feature the same
//! functions refuse firms and add nothing, and there are no commands.

use std::path::Path;

use anyhow::Result;
use zapper_policy::Config;

use crate::client::Client;
use crate::EXAMPLE;

#[cfg(feature = "office")]
#[path = "firm_cli.rs"]
mod firm_cli;

#[cfg(feature = "office")]
#[derive(clap::Subcommand)]
pub enum Cmd {
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
}

#[cfg(not(feature = "office"))]
#[derive(clap::Subcommand)]
pub enum Cmd {}

pub async fn run(c: &Client, cmd: Cmd, json_out: bool) -> Result<i32> {
    #[cfg(feature = "office")]
    return match cmd {
        Cmd::Firm { cmd } => firm_cli::firm(c, cmd, json_out).await,
        Cmd::Mail { cmd } => firm_cli::mail(c, cmd, json_out).await,
        Cmd::Drive { cmd } => firm_cli::drive(c, cmd, json_out).await,
    };
    #[cfg(not(feature = "office"))]
    {
        let _ = (c, json_out);
        match cmd {}
    }
}

/// A config's firms: checked by the office, or refused without it.
pub fn config(c: &Config) -> Result<()> {
    #[cfg(feature = "office")]
    return zapper_office::firm::check(c);
    #[cfg(not(feature = "office"))]
    {
        if !c.firms.is_empty() {
            anyhow::bail!(
                "firms: this zapper is built without the office (`--no-default-features`); remove the `firms` section"
            );
        }
        Ok(())
    }
}

/// Starts the office in the daemon, which checks its firms with every
/// config change; without it, a config with firms is refused.
pub fn start(d: &zapper_core::Core) {
    #[cfg(feature = "office")]
    zapper_office::start(d);
    #[cfg(not(feature = "office"))]
    d.add_config_check(config);
}

/// What the office adds to the API and the UI; nothing without it.
pub fn mount(d: &zapper_core::Core) -> zapper_api::Mount {
    #[cfg(feature = "office")]
    return zapper_api::Mount {
        routes: zapper_office::routes(d),
        page: Some(zapper_office::PAGE),
    };
    #[cfg(not(feature = "office"))]
    {
        let _ = d;
        zapper_api::Mount::default()
    }
}

/// The example config `zapper init` writes: without the office, less its
/// `firms` section (the file's last, with the comment above it).
pub fn example() -> &'static str {
    if cfg!(feature = "office") {
        return EXAMPLE;
    }
    let Some(at) = EXAMPLE.find("\nfirms:") else {
        return EXAMPLE;
    };
    // Up to and including the newline before `firms:`, then without the
    // comment lines right above it.
    let mut head = &EXAMPLE[..at + 1];
    while let Some(nl) = head[..head.len() - 1].rfind('\n') {
        if !head[nl + 1..].starts_with('#') {
            break;
        }
        head = &head[..nl + 1];
    }
    head
}

/// `zapper init`: the workers' default skills next to the config.
pub fn init(config_dir: &Path) -> Result<()> {
    #[cfg(feature = "office")]
    {
        let dir = config_dir.join(zapper_office::skills::DEFAULT_DIR);
        let written = zapper_office::skills::install(&dir)?;
        if written.is_empty() {
            println!("{}: the default skills are all there", dir.display());
        } else {
            println!("wrote skills to {}: {}", dir.display(), written.join(", "));
        }
    }
    #[cfg(not(feature = "office"))]
    let _ = config_dir;
    Ok(())
}

/// `zapper check`: each firm's skills.
pub fn check(config: &Config, report: &mut impl FnMut(bool, &str, &str)) {
    #[cfg(feature = "office")]
    for (name, firm) in &zapper_office::firm::all(config) {
        let dir = zapper_office::skills::dir(firm);
        let missing = zapper_office::skills::missing(firm);
        report(
            missing.is_empty(),
            &format!("firm {name}: skills in {}", dir.display()),
            &format!(
                "zapper init (writes the default skills), or add <name>/SKILL.md there for: {}",
                missing.join(", ")
            ),
        );
    }
    #[cfg(not(feature = "office"))]
    let _ = (config, report);
}

/// The daemon's state directory, from the config.
#[cfg(feature = "office")]
fn daemon_state_dir() -> Result<std::path::PathBuf> {
    let path = crate::config_path(std::env::var("ZAPPER_CONFIG").ok().as_deref());
    Ok(std::path::PathBuf::from(zapper_policy::expand_home(
        &crate::load_config(&path)?.daemon.state,
    )))
}
