//! `zapper-inspect`: The Office's past runs, read from the office's state
//! directory (and the keeper's, when it keeps the record). Lists the runs
//! and the people in each, names a scenario's operatives (the red team's
//! contractor, say), and exports a worker's transcript. Read-only; the
//! office needn't run.

mod export;
mod runs;
mod tui;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use runs::{fmt_dur, fmt_time, Store};
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use zapper_policy::{expand_home, DaemonConfig};

#[derive(Parser)]
#[command(
    name = "zapper-inspect",
    about = "Inspect The Office's past runs: the people in them, a scenario's operatives, transcripts",
    long_about = "Inspect The Office's past runs: the people in them, a scenario's operatives, transcripts.\n\nWith no command on a terminal it opens a small TUI: enter a run to see its people, e exports the selected worker's transcript as Markdown, r as raw JSONL, o jumps to the next operative."
)]
struct Cli {
    /// The zapper config, for the state directory (and the keeper's).
    /// Default: config/zapper.yaml in a checkout, else ~/.config/zapper/zapper.yaml.
    #[arg(long, env = "ZAPPER_CONFIG", global = true)]
    config: Option<String>,
    /// The office's state directory, instead of the config's.
    #[arg(long, global = true)]
    state: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// The runs, newest first, with their scenario and operatives.
    Runs,
    /// The people in a run: who hired them, their model, whether they left,
    /// and who filled a scenario's slots.
    People {
        /// The run's id, or a prefix of it.
        run: String,
    },
    /// The operatives of a run's scenario (the red team's contractor), one
    /// name per line.
    Operatives {
        /// The run's id, or a prefix of it.
        run: String,
    },
    /// A worker's transcript: as Markdown, or the raw JSONL with --raw.
    Export {
        /// The run's id, or a prefix of it.
        run: String,
        /// The worker's name; `operative` for the scenario's operative.
        worker: String,
        /// Copy the transcript as recorded (JSONL) instead of Markdown.
        #[arg(long)]
        raw: bool,
        /// Where to write; `-` for stdout. Default: RUN-WORKER.md (or .jsonl).
        #[arg(short, long)]
        output: Option<String>,
    },
    /// The terminal UI (the default on a terminal).
    Tui,
}

fn main() {
    // Die quietly when the reader goes away (`| head`), like other tools.
    // SAFETY: restores the default action for one signal, before any thread.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    if let Err(e) = real_main() {
        eprintln!("zapper-inspect: {e:#}");
        std::process::exit(1);
    }
}

fn real_main() -> Result<()> {
    let cli = Cli::parse();
    let store = store(&cli)?;
    let cmd = cli.cmd.unwrap_or(if std::io::stdout().is_terminal() {
        Cmd::Tui
    } else {
        Cmd::Runs
    });
    match cmd {
        Cmd::Tui => tui::run(&store),
        Cmd::Runs => {
            let runs = store.runs()?;
            println!(
                "{:<20} {:<8} {:<22} {:<20} {:>6} {:>5}  {:<14} TASK",
                "RUN", "FIRM", "COMPANY", "SCENARIO", "PEOPLE", "MAIL", "OPERATIVE"
            );
            for r in &runs {
                let ops: Vec<&str> = r.operatives().map(|p| p.name.as_str()).collect();
                println!(
                    "{:<20} {:<8} {:<22} {:<20} {:>6} {:>5}  {:<14} {}",
                    r.id,
                    r.firm,
                    clip(&r.company, 22),
                    r.scenario.as_deref().unwrap_or("-"),
                    r.people.len(),
                    r.messages,
                    if ops.is_empty() {
                        "-".into()
                    } else {
                        ops.join(",")
                    },
                    clip(&r.task, 60)
                );
            }
            Ok(())
        }
        Cmd::People { run } => {
            let r = store.find(&run)?;
            println!(
                "{} · {} · firm {} · started {} · lasted {} · {} messages",
                r.id,
                if r.company.is_empty() {
                    "-"
                } else {
                    &r.company
                },
                r.firm,
                fmt_time(r.started),
                fmt_dur(r.last - r.started),
                r.messages
            );
            println!("scenario: {}", r.scenario.as_deref().unwrap_or("none"));
            println!("task: {}\n", if r.task.is_empty() { "-" } else { &r.task });
            println!(
                "  {:<11} {:<17} {:<28} {:<26} {:<9} STATUS",
                "NAME", "ROLE", "TITLE", "MODEL", "HIRED BY"
            );
            for p in &r.people {
                let mut status = p.status();
                if let Some(o) = &p.operative {
                    status = match &o.released {
                        Some(why) => format!("slot released ({why}); {status}"),
                        None => format!("OPERATIVE ({}); {status}", o.scenario),
                    };
                }
                println!(
                    "{} {:<11} {:<17} {:<28} {:<26} {:<9} {}",
                    if p.operative.is_some() { "★" } else { " " },
                    p.name,
                    p.role,
                    clip(&p.title, 28),
                    clip(&p.model, 26),
                    p.hired_by.as_deref().unwrap_or("board"),
                    status
                );
            }
            Ok(())
        }
        Cmd::Operatives { run } => {
            for p in store.find(&run)?.operatives() {
                println!("{}", p.name);
            }
            Ok(())
        }
        Cmd::Export {
            run,
            worker,
            raw,
            output,
        } => {
            let r = store.find(&run)?;
            let p = if worker == "operative" {
                let mut ops = r.operatives();
                let p = ops.next().context("the run has no operative")?;
                if ops.next().is_some() {
                    anyhow::bail!("the run has several operatives; name one");
                }
                p
            } else {
                r.person(&worker)?
            };
            let src = r
                .transcript(p)
                .with_context(|| format!("{} has no transcript (never took a turn?)", p.name))?;
            let body = if raw {
                std::fs::read(&src)?
            } else {
                export::markdown(&r, p, &src)?.into_bytes()
            };
            let out = output.unwrap_or_else(|| {
                format!("{}-{}.{}", r.id, p.name, if raw { "jsonl" } else { "md" })
            });
            if out == "-" {
                std::io::stdout().lock().write_all(&body)?;
            } else {
                std::fs::write(&out, body).with_context(|| format!("writing {out}"))?;
                eprintln!("{out}");
            }
            Ok(())
        }
    }
}

/// The state directory from --state, else the config's, and the keeper's
/// directory if the config sets one.
fn store(cli: &Cli) -> Result<Store> {
    let path = PathBuf::from(expand_home(cli.config.as_deref().unwrap_or_else(|| {
        if std::path::Path::new("config/zapper.yaml").is_file() {
            "config/zapper.yaml"
        } else {
            "~/.config/zapper/zapper.yaml"
        }
    })));
    let path = std::path::absolute(&path).unwrap_or(path);
    if let Some(dir) = path.parent() {
        zapper_policy::set_config_dir(dir);
    }
    // Only two keys matter here, so the config isn't validated as a whole:
    // an older or newer config still names its state directory.
    let config: serde_yaml::Value = if path.is_file() {
        serde_yaml::from_str(&std::fs::read_to_string(&path)?)
            .with_context(|| format!("parsing {}", path.display()))?
    } else {
        serde_yaml::Value::Null
    };
    let daemon = &config["daemon"];
    let state = match (&cli.state, daemon["state"].as_str()) {
        (Some(s), _) => s.clone(),
        (None, Some(s)) => PathBuf::from(expand_home(s)),
        (None, None) => PathBuf::from(expand_home(&DaemonConfig::default().state)),
    };
    let keeper = daemon["keeper"]["dir"]
        .as_str()
        .map(|d| PathBuf::from(expand_home(d)));
    Ok(Store { state, keeper })
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n - 1).collect::<String>() + "…"
    }
}
