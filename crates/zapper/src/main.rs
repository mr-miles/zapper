//! zapper: run programs in Firecracker microVMs whose network, exec and
//! filesystem access is set by layered rules, with credentials injected on
//! the host and everything that happens recorded.

mod client;
mod daemon;
#[cfg(feature = "office")]
mod firm_cli;
mod image;
mod netns;
mod term;
mod vmm;

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use http_body_util::BodyExt;
use hyper::Method;
use serde_json::{json, Value};
use zapper_policy::{expand_home, Config};

use client::Client;

#[derive(Parser)]
#[command(
    name = "zapper",
    version,
    about = "Run agents and people in policy-bound Firecracker microVMs"
)]
struct Cli {
    /// Startup config file. Default: `config/zapper.yaml` when run in a
    /// zapper checkout, else `~/.config/zapper/zapper.yaml`.
    #[arg(long, global = true, env = "ZAPPER_CONFIG")]
    config: Option<String>,
    /// Print API responses as JSON.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Write the example config to --config if it does not exist; with the
    /// office, also the default skills of its workers into `skills/` next to
    /// it, keeping those already there.
    Init,
    /// Check prerequisites; prints the fix for anything missing.
    Check,
    /// Run the daemon.
    Serve,
    /// Start a session, open its command, remove it on exit.
    Run {
        #[arg(short, long, default_value = "default")]
        profile: String,
        /// Keep the session after the command exits.
        #[arg(long)]
        keep: bool,
        /// Directory `{workdir}` mounts refer to.
        #[arg(short = 'C', long)]
        workdir: Option<PathBuf>,
        /// Command instead of the profile's.
        #[arg(last = true)]
        command: Vec<String>,
    },
    /// Open a terminal in a running session.
    Attach {
        id: String,
        #[arg(last = true)]
        command: Vec<String>,
    },
    /// Run a command in a session without a terminal.
    Exec {
        id: String,
        /// As root, without exec supervision: for the operator's own
        /// maintenance (e.g. a firm's cluster VM).
        #[arg(long)]
        system: bool,
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// Copy a guest path out of a session: `zapper cp ID:/work ./out`.
    Cp { from: String, to: PathBuf },
    /// List sessions.
    Ps,
    /// Stop a session; it stays listed with its audit log.
    Stop { id: String },
    /// Stop and forget a session (its audit log stays on disk).
    Rm { id: String },
    /// Print a session's audit log.
    Audit { id: String },
    /// Follow events.
    Events {
        #[arg(short, long)]
        session: Option<String>,
        /// Start with recent history.
        #[arg(long)]
        history: bool,
    },
    /// List pending `ask` decisions.
    Approvals,
    /// Allow a pending request.
    Approve {
        id: String,
        /// Also add a rule to the session, kept in memory until it ends.
        #[arg(long, value_parser = ["session"])]
        remember: Option<String>,
    },
    /// Deny a pending request.
    Deny {
        id: String,
        /// Also add a deny rule to the session, kept in memory until it ends.
        #[arg(long, value_parser = ["session"])]
        remember: Option<String>,
    },
    /// List rules.
    Rules {
        /// `global`, `profile:NAME` or `session:ID`.
        #[arg(long)]
        scope: Option<String>,
    },
    /// Add or remove a rule in the running config.
    Rule {
        #[command(subcommand)]
        cmd: RuleCmd,
    },
    /// Dry-run a request against the policy.
    Eval {
        #[arg(short, long)]
        session: Option<String>,
        #[arg(short, long)]
        profile: Option<String>,
        #[command(subcommand)]
        what: EvalCmd,
    },
    /// Running and startup config.
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
    /// Profiles in the running config.
    Profile {
        #[command(subcommand)]
        cmd: ProfileCmd,
    },
    /// Root filesystem images.
    Image {
        #[command(subcommand)]
        cmd: ImageCmd,
    },
    /// Firms of workers: start, stop, watch, replay.
    #[cfg(feature = "office")]
    Firm {
        #[command(subcommand)]
        cmd: firm_cli::FirmCmd,
    },
    /// Mail as the board.
    #[cfg(feature = "office")]
    Mail {
        #[command(subcommand)]
        cmd: firm_cli::MailCmd,
    },
    /// A firm's shared drive.
    #[cfg(feature = "office")]
    Drive {
        #[command(subcommand)]
        cmd: firm_cli::DriveCmd,
    },
    /// The record: who keeps it, and checking it against the keeper's chains.
    Record {
        #[command(subcommand)]
        cmd: RecordCmd,
    },
}

#[derive(Subcommand)]
enum RecordCmd {
    /// Who keeps the record and whether it is being written.
    Status,
    /// Checks a firm's run against the keeper's chains and signatures.
    #[cfg(feature = "office")]
    Verify { firm: String, run: String },
}

#[derive(Subcommand)]
enum RuleCmd {
    /// `zapper rule add global '{kind: net, action: allow, host: "*.github.com"}'`
    Add {
        scope: String,
        /// The rule as YAML or JSON, with `kind: net|exec|fs`.
        rule: String,
        /// Position within its kind's list (default: last).
        #[arg(long)]
        at: Option<usize>,
    },
    /// Remove a rule by id.
    Rm { id: String },
}

#[derive(Subcommand)]
enum EvalCmd {
    /// `zapper eval net api.github.com:443 GET /user`
    Net {
        target: String,
        method: Option<String>,
        path: Option<String>,
    },
    /// `zapper eval exec /usr/bin/curl curl example.com`
    Exec { path: String, argv: Vec<String> },
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Print the running config.
    Show,
    /// Print the startup config.
    Startup,
    /// What `save` would change.
    Diff,
    /// Write the running config to the startup file.
    Save,
    /// Discard running changes: load the startup file.
    Reload,
    /// Edit the running config in $EDITOR.
    Edit,
    /// Replace the running config with a file.
    Apply { file: PathBuf },
}

#[derive(Subcommand)]
enum ProfileCmd {
    Ls,
    Show {
        name: String,
    },
    /// Set a profile from a YAML file.
    Set {
        name: String,
        file: PathBuf,
    },
    Rm {
        name: String,
    },
}

#[derive(Subcommand)]
enum ImageCmd {
    /// Build an image from an OCI image: `zapper image build ubuntu-24.04 --from docker.io/library/ubuntu:24.04`
    Build {
        name: String,
        #[arg(long)]
        from: String,
        /// Command run in the image at build time (repeatable).
        #[arg(long)]
        run: Vec<String>,
        /// Image size, e.g. `4GiB` (default: content plus headroom).
        #[arg(long)]
        size: Option<String>,
    },
    Ls,
}

const EXAMPLE: &str = include_str!("../../../config/zapper.yaml");

/// The example config `zapper init` writes: without the office, less its
/// `firms` section (the file's last, with the comment above it).
fn example() -> &'static str {
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

fn load_config(path: &Path) -> Result<Config> {
    if !path.exists() {
        return Ok(Config::default());
    }
    Config::from_yaml(&std::fs::read_to_string(path)?)
        .and_then(|c| daemon::office_config(&c).map(|_| c))
        .with_context(|| format!("loading {}", path.display()))
}

fn main() {
    // The VMM shim must start before any thread does.
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("__vmm") {
        let code = match args.get(2).map(String::as_str) {
            Some("--probe") => vmm::probe(args.get(3).map(String::as_str) == Some("nic")),
            _ => vmm::main(&args[2..]),
        };
        std::process::exit(code);
    }
    // Commands whose output is piped into `head` end quietly.
    if args.get(1).map(String::as_str) != Some("serve") {
        unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(async_main());
}

async fn async_main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "zapper=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    match run(cli).await {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("zapper: {e:#}");
            std::process::exit(1);
        }
    }
}

/// The config file: the one given, else the checkout's, else the user's.
/// Relative paths in it resolve against its directory.
fn config_path(given: Option<&str>) -> PathBuf {
    let path = match given {
        Some(p) => PathBuf::from(expand_home(p)),
        None => {
            let local = PathBuf::from("config/zapper.yaml");
            if local.is_file() {
                local
            } else {
                PathBuf::from(expand_home("~/.config/zapper/zapper.yaml"))
            }
        }
    };
    let path = std::path::absolute(&path).unwrap_or(path);
    if let Some(dir) = path.parent() {
        zapper_policy::set_config_dir(dir);
    }
    path
}

/// The daemon's state directory, from the config.
#[cfg(feature = "office")]
fn daemon_state_dir() -> Result<PathBuf> {
    let path = config_path(std::env::var("ZAPPER_CONFIG").ok().as_deref());
    Ok(PathBuf::from(expand_home(
        &load_config(&path)?.daemon.state,
    )))
}

fn print_json(v: &Value) {
    println!("{}", serde_json::to_string_pretty(v).unwrap());
}

/// Prints rows as aligned columns.
fn table(header: &[&str], rows: Vec<Vec<String>>) {
    let mut widths: Vec<usize> = header.iter().map(|h| h.len()).collect();
    for r in &rows {
        for (i, c) in r.iter().enumerate() {
            widths[i] = widths[i].max(c.chars().count());
        }
    }
    let line = |cells: Vec<String>| {
        let s: Vec<String> = cells
            .iter()
            .enumerate()
            .map(|(i, c)| format!("{c:<w$}", w = widths[i]))
            .collect();
        println!("{}", s.join("  ").trim_end());
    };
    line(header.iter().map(|h| h.to_uppercase()).collect());
    for r in rows {
        line(r);
    }
}

fn s(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

async fn run(cli: Cli) -> Result<i32> {
    let config_path = config_path(cli.config.as_deref());
    let c = Client::new();
    match cli.cmd {
        Cmd::Init => {
            if config_path.exists() {
                println!("{} exists; not touching it", config_path.display());
            } else {
                std::fs::create_dir_all(config_path.parent().unwrap())?;
                daemon::write_private(&config_path, example().as_bytes())?;
                println!("wrote {}", config_path.display());
            }
            // Skills are the office's workers'.
            #[cfg(feature = "office")]
            {
                let dir = config_path
                    .parent()
                    .unwrap()
                    .join(daemon::office::skills::DEFAULT_DIR);
                let written = daemon::office::skills::install(&dir)?;
                if written.is_empty() {
                    println!("{}: the default skills are all there", dir.display());
                } else {
                    println!("wrote skills to {}: {}", dir.display(), written.join(", "));
                }
            }
        }
        Cmd::Check => return check(&config_path).await,
        Cmd::Serve => serve(config_path).await?,
        Cmd::Run {
            profile,
            keep,
            workdir,
            command,
        } => {
            let workdir = match workdir {
                Some(w) => std::fs::canonicalize(w)?,
                None => std::env::current_dir()?,
            };
            eprintln!("zapper: starting a {profile} session…");
            let body = serde_json::to_string(&json!({ "profile": profile, "workdir": workdir }))?;
            let info = c.json(Method::POST, "/v1/sessions", Some(body)).await?;
            let id = s(&info["id"]);
            eprintln!("zapper: session {id} is ready");
            let code = attach(&c, &id, &command, false).await;
            if keep {
                eprintln!("zapper: session {id} kept; `zapper attach {id}`, `zapper rm {id}`");
            } else {
                c.json(Method::DELETE, &format!("/v1/sessions/{id}"), None)
                    .await?;
                eprintln!("zapper: session {id} removed; its log: `zapper audit {id}`");
            }
            return code;
        }
        Cmd::Attach { id, command } => return attach(&c, &id, &command, false).await,
        Cmd::Exec {
            id,
            system,
            command,
        } => {
            let io = c.attach(&id, None, &command, system).await?;
            return term::run(io, false).await;
        }
        Cmd::Cp { from, to } => {
            let (id, path) = from.split_once(':').context("expected ID:/guest/path")?;
            std::fs::create_dir_all(&to)?;
            let argv = vec![
                "tar".into(),
                "-C".into(),
                path.into(),
                "--exclude=./lost+found".into(),
                "-cf".into(),
                "-".into(),
                ".".into(),
            ];
            let io = c.attach(id, None, &argv, true).await?;
            // Unpack locally from the tar stream on stdout.
            let mut tar = tokio::process::Command::new("tar")
                .args(["--no-same-owner", "--no-same-permissions", "-xf", "-", "-C"])
                .arg(&to)
                .stdin(std::process::Stdio::piped())
                .spawn()?;
            let mut stdin = tar.stdin.take().unwrap();
            let (mut r, _w) = tokio::io::split(io);
            let mut code = 255;
            while let Some(f) = daemon::session::read_frame(&mut r).await? {
                use tokio::io::AsyncWriteExt;
                match f {
                    zapper_policy::proto::Frame::Data(d) => stdin.write_all(&d).await?,
                    zapper_policy::proto::Frame::Stderr(d) => {
                        eprint!("{}", String::from_utf8_lossy(&d))
                    }
                    zapper_policy::proto::Frame::Exit(c) => {
                        code = c;
                        break;
                    }
                    _ => {}
                }
            }
            drop(stdin);
            let status = tar.wait().await?;
            if code != 0 || !status.success() {
                bail!("copy failed (guest tar exited {code}, local tar {status})");
            }
            println!("copied {from} to {}", to.display());
        }
        Cmd::Ps => {
            let v = c.json(Method::GET, "/v1/sessions", None).await?;
            if cli.json {
                print_json(&v);
            } else {
                let rows = v
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|x| {
                        let k = &x["counters"];
                        vec![
                            s(&x["id"]),
                            s(&x["profile"]),
                            s(&x["state"]),
                            s(&x["image"]),
                            format!("{}/{}", k["net_allowed"], k["net_denied"]),
                            format!("{}/{}", k["exec_allowed"], k["exec_denied"]),
                            s(&x["detail"]),
                        ]
                    })
                    .collect();
                table(
                    &[
                        "id",
                        "profile",
                        "state",
                        "image",
                        "net ok/no",
                        "exec ok/no",
                        "detail",
                    ],
                    rows,
                );
            }
        }
        Cmd::Stop { id } => print_json(
            &c.json(Method::POST, &format!("/v1/sessions/{id}/stop"), None)
                .await?,
        ),
        Cmd::Rm { id } => print_json(
            &c.json(Method::DELETE, &format!("/v1/sessions/{id}"), None)
                .await?,
        ),
        Cmd::Audit { id } => print!("{}", c.text(&format!("/v1/sessions/{id}/audit")).await?),
        Cmd::Events { session, history } => {
            let mut path = format!("/v1/events?history={history}");
            if let Some(s) = session {
                path.push_str(&format!("&session={s}"));
            }
            let mut body = c.stream(&path).await?;
            let mut buf = String::new();
            while let Some(frame) = body.frame().await {
                let frame = frame?;
                let Some(data) = frame.data_ref() else {
                    continue;
                };
                buf.push_str(&String::from_utf8_lossy(data));
                while let Some(i) = buf.find('\n') {
                    let line: String = buf.drain(..=i).collect();
                    if let Some(json) = line.trim().strip_prefix("data:") {
                        if cli.json {
                            println!("{}", json.trim());
                        } else if let Ok(v) = serde_json::from_str::<Value>(json.trim()) {
                            println!("{}", event_line(&v));
                        }
                    }
                }
            }
        }
        Cmd::Approvals => {
            let v = c.json(Method::GET, "/v1/approvals", None).await?;
            if cli.json {
                print_json(&v);
            } else {
                let rows = v
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|p| {
                        vec![
                            s(&p["id"]),
                            s(&p["session"]),
                            s(&p["kind"]),
                            s(&p["subject"]),
                        ]
                    })
                    .collect();
                table(&["id", "session", "kind", "request"], rows);
            }
        }
        Cmd::Approve { id, remember } => answer(&c, &id, true, remember).await?,
        Cmd::Deny { id, remember } => answer(&c, &id, false, remember).await?,
        Cmd::Rules { scope } => {
            let path = match scope {
                Some(s) => format!("/v1/rules?scope={}", client::url_encode(&s)),
                None => "/v1/rules".into(),
            };
            let v = c.json(Method::GET, &path, None).await?;
            if cli.json {
                print_json(&v);
            } else {
                let rows = v.as_array().unwrap().iter().map(rule_row).collect();
                table(&["scope", "id", "kind", "action", "match", "comment"], rows);
            }
        }
        Cmd::Rule {
            cmd: RuleCmd::Add { scope, rule, at },
        } => {
            let rule: Value = serde_yaml::from_str(&rule).context("rule")?;
            let body = json!({ "scope": scope, "rule": rule, "position": at });
            let v = c
                .json(Method::POST, "/v1/rules", Some(body.to_string()))
                .await?;
            println!(
                "added {} to {}; `zapper config save` to keep it",
                s(&v["id"]),
                s(&v["scope"])
            );
        }
        Cmd::Rule {
            cmd: RuleCmd::Rm { id },
        } => {
            let v = c
                .json(Method::DELETE, &format!("/v1/rules/{id}"), None)
                .await?;
            println!("removed {} from {}", s(&v["id"]), s(&v["scope"]));
        }
        Cmd::Eval {
            session,
            profile,
            what,
        } => {
            let mut body = json!({ "session": session, "profile": profile });
            match what {
                EvalCmd::Net {
                    target,
                    method,
                    path,
                } => {
                    let (host, port) = match target.rsplit_once(':') {
                        Some((h, p)) => (h.to_string(), p.parse()?),
                        None => (target.clone(), 443u16),
                    };
                    body["net"] =
                        json!({ "host": host, "port": port, "method": method, "path": path });
                }
                EvalCmd::Exec { path, argv } => {
                    body["exec"] = json!({ "path": path, "argv": argv })
                }
            }
            print_json(
                &c.json(Method::POST, "/v1/evaluate", Some(body.to_string()))
                    .await?,
            );
        }
        Cmd::Config { cmd } => match cmd {
            ConfigCmd::Show => print!("{}", c.text("/v1/config").await?),
            ConfigCmd::Startup => print!("{}", c.text("/v1/config/startup").await?),
            ConfigCmd::Diff => {
                let d = c.text("/v1/config/diff").await?;
                if d.trim().is_empty() {
                    println!("no unsaved changes");
                } else {
                    print!("{d}");
                }
            }
            ConfigCmd::Save => {
                let v = c.json(Method::POST, "/v1/config/save", None).await?;
                println!("saved to {}", s(&v["saved"]));
            }
            ConfigCmd::Reload => {
                c.json(Method::POST, "/v1/config/reload", None).await?;
                println!("running config replaced by the startup file");
            }
            ConfigCmd::Edit => {
                let current = c.text("/v1/config").await?;
                let tmp =
                    std::env::temp_dir().join(format!("zapper-{}.yaml", daemon::random_id(6)));
                daemon::write_private(&tmp, current.as_bytes())?;
                let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".into());
                let status = std::process::Command::new("sh")
                    .arg("-c")
                    .arg(format!("{editor} \"$1\""))
                    .arg("sh")
                    .arg(&tmp)
                    .status()?;
                let edited = std::fs::read_to_string(&tmp)?;
                let _ = std::fs::remove_file(&tmp);
                if !status.success() || edited == current {
                    println!("no change");
                } else {
                    c.json(Method::PUT, "/v1/config", Some(edited)).await?;
                    println!("running config updated; `zapper config diff`, then `zapper config save` to keep it");
                }
            }
            ConfigCmd::Apply { file } => {
                c.json(
                    Method::PUT,
                    "/v1/config",
                    Some(std::fs::read_to_string(file)?),
                )
                .await?;
                println!("running config replaced; `zapper config save` to keep it");
            }
        },
        Cmd::Profile { cmd } => match cmd {
            ProfileCmd::Ls => {
                let v = c.json(Method::GET, "/v1/profiles", None).await?;
                if cli.json {
                    print_json(&v);
                } else {
                    let rows = v
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|p| {
                            vec![
                                s(&p["name"]),
                                s(&p["extends"]),
                                s(&p["image"]),
                                p["command"]
                                    .as_array()
                                    .map(|a| a.iter().map(s).collect::<Vec<_>>().join(" "))
                                    .unwrap_or_default(),
                                s(&p["description"]),
                            ]
                        })
                        .collect();
                    table(
                        &["name", "extends", "image", "command", "description"],
                        rows,
                    );
                }
            }
            ProfileCmd::Show { name } => {
                print!("{}", c.text(&format!("/v1/profiles/{name}")).await?)
            }
            ProfileCmd::Set { name, file } => {
                c.json(
                    Method::PUT,
                    &format!("/v1/profiles/{name}"),
                    Some(std::fs::read_to_string(file)?),
                )
                .await?;
                println!("profile {name} set; `zapper config save` to keep it");
            }
            ProfileCmd::Rm { name } => {
                c.json(Method::DELETE, &format!("/v1/profiles/{name}"), None)
                    .await?;
                println!("profile {name} removed");
            }
        },
        #[cfg(feature = "office")]
        Cmd::Firm { cmd } => return firm_cli::firm(&c, cmd, cli.json).await,
        Cmd::Record { cmd } => {
            let (path, verify) = match &cmd {
                RecordCmd::Status => ("/v1/record".to_string(), false),
                #[cfg(feature = "office")]
                RecordCmd::Verify { firm, run } => {
                    (format!("/v1/firms/{firm}/runs/{run}/verify"), true)
                }
            };
            let v = c.json(Method::GET, &path, None).await?;
            if cli.json || !verify {
                print_json(&v);
                return Ok(0);
            }
            for f in v["files"].as_array().into_iter().flatten() {
                if f["ok"] == json!(true) {
                    println!(
                        "ok     {} ({} lines, {} signed)",
                        f["file"].as_str().unwrap_or(""),
                        f["lines"],
                        f["signed"]
                    );
                } else {
                    println!(
                        "CHANGED {}: {}",
                        f["file"].as_str().unwrap_or(""),
                        f["error"].as_str().unwrap_or("")
                    );
                }
            }
            let ok = v["ok"] == json!(true);
            println!(
                "{}",
                if ok {
                    "the run's record is intact"
                } else {
                    "the run's record was changed"
                }
            );
            return Ok(if ok { 0 } else { 1 });
        }
        #[cfg(feature = "office")]
        Cmd::Mail { cmd } => return firm_cli::mail(&c, cmd, cli.json).await,
        #[cfg(feature = "office")]
        Cmd::Drive { cmd } => return firm_cli::drive(&c, cmd, cli.json).await,
        Cmd::Image { cmd } => {
            let config = load_config(&config_path)?;
            let dir = PathBuf::from(expand_home(&config.daemon.images));
            match cmd {
                ImageCmd::Build {
                    name,
                    from,
                    run,
                    size,
                } => {
                    let out = image::build(image::Build {
                        name: &name,
                        from: &from,
                        run: &run,
                        size: size.as_deref(),
                        dir: &dir,
                    })?;
                    println!("built {}", out.display());
                }
                ImageCmd::Ls => {
                    let v = image::list(&dir)?;
                    if cli.json {
                        print_json(&json!(v));
                    } else {
                        let rows = v
                            .iter()
                            .map(|i| {
                                vec![
                                    s(&i["name"]),
                                    format!("{} MiB", i["size"].as_u64().unwrap_or(0) >> 20),
                                    s(&i["from"]),
                                ]
                            })
                            .collect();
                        table(&["name", "size", "from"], rows);
                    }
                }
            }
        }
    }
    Ok(0)
}

async fn attach(c: &Client, id: &str, command: &[String], system: bool) -> Result<i32> {
    let tty = term::stdin_is_tty();
    let size = if tty {
        Some(term::size().unwrap_or((24, 80)))
    } else {
        None
    };
    let io = c.attach(id, size, command, system).await?;
    term::run(io, tty).await
}

async fn answer(c: &Client, id: &str, allow: bool, remember: Option<String>) -> Result<()> {
    let body = json!({ "allow": allow, "remember": remember });
    c.json(
        Method::POST,
        &format!("/v1/approvals/{id}"),
        Some(body.to_string()),
    )
    .await?;
    println!("{} {id}", if allow { "allowed" } else { "denied" });
    Ok(())
}

fn rule_row(r: &Value) -> Vec<String> {
    let m = match r["kind"].as_str() {
        Some("net") => {
            let mut parts = vec![s(&r["host"])];
            if let Some(p) = r["ports"].as_array() {
                parts[0] = format!(
                    "{}:{}",
                    parts[0],
                    p.iter().map(s).collect::<Vec<_>>().join(",")
                );
            }
            if let Some(m) = r["methods"].as_array() {
                parts.push(m.iter().map(s).collect::<Vec<_>>().join(","));
            }
            if !r["path"].is_null() {
                parts.push(s(&r["path"]));
            }
            if let Some(sec) = r["secrets"].as_array() {
                parts.push(format!(
                    "secrets={}",
                    sec.iter().map(s).collect::<Vec<_>>().join(",")
                ));
            }
            parts.join(" ")
        }
        Some("exec") => {
            let mut m = s(&r["path"]);
            if !r["args"].is_null() {
                m.push_str(&format!(" args={}", s(&r["args"])));
            }
            m
        }
        _ => format!("{} {}", s(&r["path"]), s(&r["access"])),
    };
    let action = if r["final"] == json!(true) {
        format!("{} (final)", s(&r["action"]))
    } else {
        s(&r["action"])
    };
    vec![
        s(&r["scope"]),
        s(&r["id"]),
        s(&r["kind"]),
        action,
        m,
        s(&r["comment"]),
    ]
}

fn event_line(e: &Value) -> String {
    let ts = e["ts"].as_f64().unwrap_or(0.0) as i64;
    let t = format!(
        "{:02}:{:02}:{:02}",
        (ts / 3600) % 24,
        (ts / 60) % 60,
        ts % 60
    );
    let sess = s(&e["session"]);
    let detail = match e["type"].as_str().unwrap_or("") {
        "net" => format!(
            "{} {} {}:{}{}{}{}",
            s(&e["action"]),
            e["method"].as_str().unwrap_or("CONNECT"),
            s(&e["host"]),
            e["port"],
            s(&e["path"]),
            e["status"]
                .as_u64()
                .map(|c| format!(" → {c}"))
                .unwrap_or_default(),
            e["error"]
                .as_str()
                .map(|x| format!(" ({x})"))
                .unwrap_or_default()
        ),
        "exec" => format!(
            "{} {}",
            s(&e["action"]),
            e["argv"]
                .as_array()
                .map(|a| a.iter().map(s).collect::<Vec<_>>().join(" "))
                .unwrap_or_default()
        ),
        "ask" => format!(
            "ASK {}: {}  (zapper approve {} | zapper deny {})",
            s(&e["id"]),
            s(&e["subject"]),
            s(&e["id"]),
            s(&e["id"])
        ),
        "answer" => format!(
            "{} {}",
            if e["allow"] == json!(true) {
                "allowed"
            } else {
                "denied"
            },
            s(&e["id"])
        ),
        "guest" => {
            let g = &e["event"];
            match g["event"].as_str() {
                Some("exit") => format!(
                    "exit {} pid {} code {} {}ms",
                    s(&g["comm"]),
                    g["pid"],
                    g["exit_code"],
                    g["elapsed_ms"]
                ),
                Some("setup") => format!(
                    "setup {}: {}",
                    if g["ok"] == json!(true) {
                        "ok"
                    } else {
                        "FAILED"
                    },
                    s(&g["step"])
                ),
                Some("shell_exit") => format!("shell {} exited with {}", g["id"], g["exit_code"]),
                _ => g.to_string(),
            }
        }
        "session" => format!("session {} {}", s(&e["state"]), s(&e["detail"])),
        "dns" => format!(
            "{} {}{}{}",
            s(&e["rcode"]),
            if e["name"] == json!("") {
                "(invalid query)".to_string()
            } else {
                s(&e["name"])
            },
            e["answer"]
                .as_str()
                .map(|a| format!(" → {a}"))
                .unwrap_or_default(),
            e["reason"]
                .as_str()
                .map(|r| format!(" ({r})"))
                .unwrap_or_default()
        ),
        "shell" => format!("shell {} {}", e["id"], s(&e["state"])),
        "config" => format!("config {}", s(&e["change"])),
        other => format!("{other} {e}"),
    };
    let rule = e["rule"]
        .as_str()
        .map(|r| format!("  [{r}]"))
        .unwrap_or_default();
    format!("{t} {sess:<6} {:<7} {detail}{rule}", s(&e["type"]))
}

async fn serve(config_path: PathBuf) -> Result<()> {
    let d = daemon::Daemon::new(config_path)?;
    let socket = client::socket_path();
    let dir = socket.parent().unwrap();
    std::fs::create_dir_all(dir)?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    if socket.exists() {
        if tokio::net::UnixStream::connect(&socket).await.is_ok() {
            bail!("a daemon is already listening on {}", socket.display());
        }
        std::fs::remove_file(&socket)?;
    }
    let listener = tokio::net::UnixListener::bind(&socket)?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
    }
    let ui_addr = d.config.read().unwrap().daemon.ui.clone();
    if !ui_addr.is_empty() {
        let addr: std::net::SocketAddr = ui_addr
            .parse()
            .with_context(|| format!("daemon.ui {ui_addr:?}"))?;
        let remote = !addr.ip().is_loopback();
        let tcp = tokio::net::TcpListener::bind(addr)
            .await
            .with_context(|| format!("UI on {addr}"))?;
        let bound = tcp.local_addr()?;
        let port = bound.port();
        // The bound address: with port 0 in the config, the kernel picks one.
        // Bound to every interface, the URL names this machine instead.
        let shown = if bound.ip().is_unspecified() {
            format!("{}:{port}", host_name())
        } else {
            bound.to_string()
        };
        let url = format!("http://{shown}/?token={}", d.ui_token);
        daemon::write_private(&d.paths.state.join("ui-url"), format!("{url}\n").as_bytes())?;
        if remote {
            eprintln!(
                "{}",
                exposed_warning(&format!("the UI (daemon.ui) listens on {bound}"))
            );
        }
        eprintln!("zapper: UI at {url}");
        let app = daemon::api::ui(d.clone(), port, remote);
        tokio::spawn(async move {
            if let Err(e) = axum::serve(tcp, app).await {
                tracing::error!("UI: {e}");
            }
        });
    }
    eprintln!("zapper: API on {}", socket.display());
    let app = daemon::api::api(d.clone());
    let shutdown = {
        let d = d.clone();
        async move {
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
            eprintln!("zapper: stopping sessions");
            let runs: Vec<_> = d.office.runs.read().unwrap().values().cloned().collect();
            for r in runs {
                daemon::office::down(&d, &r, "daemon shutdown").await;
            }
            let sessions: Vec<_> = d.sessions.read().unwrap().values().cloned().collect();
            for s in sessions {
                daemon::session::stop(&d, &s, "daemon shutdown").await;
            }
        }
    };
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await?;
    let _ = std::fs::remove_file(&socket);
    Ok(())
}

/// Reports what is missing, with the command that fixes it. Changes nothing.
/// This machine's host name, for a URL to a UI on every interface.
fn host_name() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: gethostname writes at most buf.len() bytes into buf.
    let ok = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } == 0;
    let end = buf.iter().position(|&b| b == 0).unwrap_or(0);
    match std::str::from_utf8(&buf[..end]) {
        Ok(h) if ok && !h.is_empty() => h.to_string(),
        _ => "localhost".into(),
    }
}

/// What `zapper serve` prints when an operator's port is reachable from
/// the network: the UI (`daemon.ui`) or a firm's Kubernetes dashboard.
pub(crate) fn exposed_warning(what: &str) -> String {
    format!(
        "\n\
         ################################################################\n\
         WARNING: {what}, reachable from the network.\n\
         Whoever has the token controls zapper: sessions, rules, approvals,\n\
         firms, and the firm's cluster as cluster-admin. It is plain HTTP:\n\
         use it on a network you trust, or reach it through an SSH tunnel\n\
         or a TLS proxy. zapper is not built for production. The VMs still\n\
         can't reach it: the host's own addresses are closed to them.\n\
         ################################################################\n"
    )
}

async fn check(config_path: &Path) -> Result<i32> {
    let mut bad = 0;
    let mut report = |ok: bool, what: &str, fix: &str| {
        if ok {
            println!("ok     {what}");
        } else {
            bad += 1;
            println!("ERROR  {what}\n       fix: {fix}");
        }
    };
    let config = match load_config(config_path) {
        Ok(c) => {
            report(
                config_path.exists(),
                &format!("config {}", config_path.display()),
                "zapper init",
            );
            c
        }
        Err(e) => {
            report(
                false,
                &format!("config {}: {e:#}", config_path.display()),
                "fix the file, or move it away and run `zapper init`",
            );
            Config::default()
        }
    };
    let d = &config.daemon;
    let kvm = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")
        .is_ok();
    report(
        kvm,
        "read/write access to /dev/kvm",
        "sudo usermod -aG kvm $USER, then log in again",
    );
    let fc = PathBuf::from(expand_home(&d.firecracker));
    let fc_ok = std::process::Command::new(&fc)
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    report(
        fc_ok,
        &format!("firecracker at {}", fc.display()),
        "make deps",
    );
    let kernels = PathBuf::from(expand_home(&d.kernels));
    let agent_ok = !d.agent.is_empty() && Path::new(&expand_home(&d.agent)).is_file() || {
        let exe = std::env::current_exe()?;
        let dir = exe.parent().unwrap().to_path_buf();
        [
            dir.join("zapper-agent"),
            dir.join("../x86_64-unknown-linux-musl/release/zapper-agent"),
        ]
        .iter()
        .any(|p| p.is_file())
    };
    report(
        agent_ok,
        "zapper-agent next to zapper (static build)",
        "make build",
    );
    let images = PathBuf::from(expand_home(&d.images));
    for name in config.profiles.keys() {
        if let Ok(p) = config.resolve(name) {
            let img = images.join(format!("{}.ext4", p.engine.image));
            report(
                img.is_file(),
                &format!("profile {name}: image {}", p.engine.image),
                &format!("make image IMAGE={} (see README: Images)", p.engine.image),
            );
            let k = kernels.join(&p.engine.kernel);
            report(
                k.is_file(),
                &format!("profile {name}: kernel {}", p.engine.kernel),
                "make deps",
            );
        }
    }
    #[cfg(feature = "office")]
    for (name, firm) in &config.firms {
        let dir = daemon::office::skills::dir(firm);
        let missing = daemon::office::skills::missing(firm);
        report(
            missing.is_empty(),
            &format!("firm {name}: skills in {}", dir.display()),
            &format!(
                "zapper init (writes the default skills), or add <name>/SKILL.md there for: {}",
                missing.join(", ")
            ),
        );
    }
    let probe = |nic: bool| {
        let mut cmd = std::process::Command::new(std::env::current_exe()?);
        cmd.args(["__vmm", "--probe"]);
        if nic {
            cmd.arg("nic");
        }
        Ok::<_, anyhow::Error>(match cmd.output() {
            Ok(o) => (
                o.status.success(),
                String::from_utf8_lossy(&o.stderr).trim().to_string(),
            ),
            Err(e) => (false, e.to_string()),
        })
    };
    let (ok, why) = probe(false)?;
    let what = if ok {
        "unprivileged user and mount namespaces (VMM confinement)".to_string()
    } else {
        format!("unprivileged user and mount namespaces (VMM confinement): {why}")
    };
    report(ok, &what, "sudo sysctl kernel.apparmor_restrict_unprivileged_userns=0 (or an AppArmor profile allowing userns for zapper)");
    let uses_nic = config
        .profiles
        .keys()
        .filter_map(|n| config.resolve(n).ok())
        .any(|p| p.engine.network == zapper_policy::NetworkMode::Nic);
    if uses_nic && ok {
        let (ok, why) = probe(true)?;
        let what = if ok {
            "tap and nftables in the VMM's network namespace (network: nic)".to_string()
        } else {
            format!("tap and nftables in the VMM's network namespace (network: nic): {why}")
        };
        report(
            ok,
            &what,
            "sudo apt install iproute2 nftables, or use network: vsock",
        );
    }
    let oci = image::engine();
    let oci_ok = std::process::Command::new(&oci)
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    report(
        oci_ok,
        &format!("{oci} (to build images)"),
        "sudo apt install podman, or install docker",
    );
    let mkfs = std::process::Command::new("mkfs.ext4")
        .arg("-V")
        .output()
        .is_ok_and(|o| o.status.success());
    report(
        mkfs,
        "mkfs.ext4 with -d (e2fsprogs ≥ 1.43)",
        "sudo apt install e2fsprogs",
    );
    if let Ok(meta) = std::fs::metadata(config_path) {
        use std::os::unix::fs::PermissionsExt;
        if config.has_inline_secrets() {
            report(
                meta.permissions().mode() & 0o077 == 0,
                "config with inline secrets readable only by you",
                &format!("chmod 600 {}", config_path.display()),
            );
        }
    }
    // The record's keeper, when configured: it must answer, and its
    // directory must be readable and not writable by us.
    match &d.keeper {
        None => println!("note   the record is written by this user (daemon.keeper is unset; scripts/install-keeper.sh sets up a keeper)"),
        Some(k) => {
            let socket = PathBuf::from(expand_home(&k.socket));
            let dir = PathBuf::from(expand_home(&k.dir));
            let hello = zapper_keeper::Client::new(&socket).hello();
            report(hello.is_ok(), &format!("keeper answers on {}", socket.display()), "sudo systemctl start zapper-keeper (scripts/install-keeper.sh sets it up)");
            let pubkey = std::fs::read_to_string(dir.join("keeper.pub")).unwrap_or_default();
            let matches = hello.as_ref().is_ok_and(|h| h["public_key"].as_str() == Some(pubkey.trim()));
            report(matches, &format!("keeper's record readable in {}", dir.display()), "daemon.keeper.dir must be the directory the keeper keeps for this user");
            let writable = std::fs::OpenOptions::new().create(true).truncate(true).write(true).open(dir.join(".zapper-write-test")).is_ok();
            if writable {
                let _ = std::fs::remove_file(dir.join(".zapper-write-test"));
            }
            report(!writable, "the record can't be written by this user", "the keeper's directory must belong to the keeper's user (scripts/install-keeper.sh)");
        }
    }
    // Not a prerequisite: the daemon is started after setup.
    let socket = client::socket_path();
    if tokio::net::UnixStream::connect(&socket).await.is_ok() {
        println!("ok     daemon listening on {}", socket.display());
    } else {
        println!("note   the daemon isn't running; start it with `zapper serve`");
    }
    Ok(if bad == 0 { 0 } else { 1 })
}

#[cfg(test)]
mod tests {
    #[test]
    #[cfg(feature = "office")]
    fn example_config_is_valid() {
        let c = zapper_policy::Config::from_yaml(super::EXAMPLE).unwrap();
        let acme = &c.firms["acme"];
        assert!(acme.roles["security-officer"].officer);
        assert!(acme.roles["ceo"]
            .hires
            .contains(&"security-officer".to_string()));
        // Access comes with grants: the spaces and their expiry, no roles.
        assert_eq!(acme.roles["cto"].profile, "office-engineer");
        assert_eq!(acme.credentials.expiry, "24h");
        assert!(acme.credentials.spaces.contains(&"general".to_string()));
        assert!(acme.roles["ceo"].hires.contains(&"cto".to_string()));
        // The simulations, validated with the firm.
        let names: Vec<&str> = acme.scenarios.keys().map(String::as_str).collect();
        assert_eq!(
            names,
            [
                "key-engineer-quits",
                "mid-project-pivot",
                "production-incident",
                "red-team"
            ]
        );
        assert!(acme.scenarios["red-team"].hires[0].board_line);
    }

    /// `zapper run -p claude`, the README's quickstart: Claude Code on a
    /// copy of the working directory, `default`'s fs rules, the token added
    /// by the host, everything else asked.
    #[test]
    fn claude_profile() {
        let c = zapper_policy::Config::from_yaml(super::EXAMPLE).unwrap();
        let p = c.resolve("claude").unwrap();
        assert_eq!(p.agent.kind, "claude-code");
        assert_eq!(p.agent.command, ["claude"]);
        assert_eq!(p.agent.user, "agent");
        assert_eq!(p.engine.image, "office");
        assert!(p
            .mounts
            .iter()
            .any(|m| m.host == "{workdir}" && m.guest == "/work"));
        let yaml = serde_yaml::to_string(&p.layers).unwrap();
        assert!(yaml.contains("/work/**/.env"), "{yaml}");
        assert!(yaml.contains("api.anthropic.com"), "{yaml}");
        assert!(yaml.contains("net: ask"), "{yaml}");
        assert_eq!(p.agent.env["CLAUDE_CODE_OAUTH_TOKEN"], "{{secret:claude}}");
    }

    /// What `zapper init` writes loads in this build, office or not.
    #[test]
    fn init_writes_a_config_this_build_loads() {
        let c = zapper_policy::Config::from_yaml(super::example()).unwrap();
        super::daemon::office_config(&c).unwrap();
        assert!(c.profiles.contains_key("claude"));
        if !cfg!(feature = "office") {
            assert!(!super::example().contains("Firms (docs/office.md)"));
        }
    }
}
