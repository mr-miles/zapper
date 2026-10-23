//! zapper: run programs in Firecracker microVMs whose network, exec and
//! filesystem access is set by layered rules, with credentials injected on
//! the host and everything that happens recorded.

mod client;
use zapper_core::{util, vmm};
#[cfg(feature = "office")]
mod firm_cli;
mod image;
mod term;

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
        /// A label for the session, `key=value`, which services pick
        /// sessions by (repeatable).
        #[arg(short, long = "label")]
        labels: Vec<String>,
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
    /// Give a driven session's agent a task (`agent.driven`); it waits for
    /// the agent's next turn. Follow it with `zapper events --session ID`.
    Tell {
        id: String,
        /// The task; several words are joined with spaces.
        #[arg(required = true)]
        text: Vec<String>,
    },
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
    /// Services: names sessions use that the daemon connects itself.
    Service {
        #[command(subcommand)]
        cmd: ServiceCmd,
    },
}

#[derive(Subcommand)]
enum ServiceCmd {
    /// List services and zones.
    Ls,
    /// `zapper service add db.internal:5432 --to-session ID:5432 --for team=a`
    Add {
        /// The name and port sessions use; `*.` in front for any name
        /// under it.
        name: String,
        /// An address on the host's network, `ip:port`.
        #[arg(
            long,
            conflicts_with = "to_session",
            required_unless_present = "to_session"
        )]
        to: Option<String>,
        /// A port of a session's VM, `ID:port`.
        #[arg(long)]
        to_session: Option<String>,
        /// Only for sessions with this label, `key=value` (repeatable);
        /// every session without.
        #[arg(long = "for")]
        sessions: Vec<String>,
    },
    /// Remove a service added with `add`.
    Rm { id: u64 },
}

/// `key=value` pairs, as JSON.
fn label_pairs(pairs: &[String]) -> Result<Value> {
    let mut m = serde_json::Map::new();
    for p in pairs {
        let (k, v) = p
            .split_once('=')
            .with_context(|| format!("{p:?}: key=value"))?;
        m.insert(k.to_string(), json!(v));
    }
    Ok(Value::Object(m))
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
    /// Build an image from an OCI image: `zapper image build ubuntu-26.04 --from docker.io/library/ubuntu:26.04`
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

/// A config with firms needs a zapper built with the office, which checks
/// them.
fn office_config(c: &Config) -> Result<()> {
    #[cfg(feature = "office")]
    zapper_office::firm::check(c)?;
    if !cfg!(feature = "office") && !c.firms.is_empty() {
        bail!(
            "firms: this zapper is built without the office (`--no-default-features`); remove the `firms` section"
        );
    }
    Ok(())
}

fn load_config(path: &Path) -> Result<Config> {
    if !path.exists() {
        return Ok(Config::default());
    }
    Config::from_yaml(&std::fs::read_to_string(path)?)
        .and_then(|c| office_config(&c).map(|_| c))
        .with_context(|| format!("loading {}", path.display()))
}

/// What the office adds to the API and the UI; nothing without it.
fn mount(d: &zapper_core::Core) -> zapper_api::Mount {
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
                util::write_private(&config_path, example().as_bytes())?;
                println!("wrote {}", config_path.display());
            }
            // Skills are the office's workers'.
            #[cfg(feature = "office")]
            {
                let dir = config_path
                    .parent()
                    .unwrap()
                    .join(zapper_office::skills::DEFAULT_DIR);
                let written = zapper_office::skills::install(&dir)?;
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
            labels,
            command,
        } => {
            let workdir = match workdir {
                Some(w) => std::fs::canonicalize(w)?,
                None => std::env::current_dir()?,
            };
            eprintln!("zapper: starting a {profile} session…");
            let body = serde_json::to_string(
                &json!({ "profile": profile, "workdir": workdir, "labels": label_pairs(&labels)? }),
            )?;
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
            while let Some(f) = util::read_frame(&mut r).await? {
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
        Cmd::Tell { id, text } => {
            let body = json!({ "text": text.join(" ") }).to_string();
            let v = c
                .json(
                    Method::POST,
                    &format!("/v1/sessions/{id}/prompt"),
                    Some(body),
                )
                .await?;
            if cli.json {
                print_json(&v);
            } else {
                let status = v["driver"]["status"].as_str().unwrap_or("");
                match v["queued"].as_u64().unwrap_or(1) {
                    1 => println!("queued for {id} (the agent is {status})"),
                    n => println!("queued for {id}: {n} waiting (the agent is {status})"),
                }
            }
        }
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
                let tmp = std::env::temp_dir().join(format!("zapper-{}.yaml", util::random_id(6)));
                util::write_private(&tmp, current.as_bytes())?;
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
        Cmd::Service { cmd } => service(&c, cmd, cli.json).await?,
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

async fn service(c: &Client, cmd: ServiceCmd, json_out: bool) -> Result<()> {
    match cmd {
        ServiceCmd::Ls => {
            let v = c.json(Method::GET, "/v1/services", None).await?;
            if json_out {
                print_json(&v);
                return Ok(());
            }
            let rows = v
                .as_array()
                .unwrap()
                .iter()
                .map(|x| {
                    let sessions = x["sessions"]
                        .as_object()
                        .map(|m| {
                            m.iter()
                                .map(|(k, v)| format!("{k}={}", s(v)))
                                .collect::<Vec<_>>()
                                .join(",")
                        })
                        .filter(|t| !t.is_empty())
                        .unwrap_or_else(|| "all".into());
                    vec![
                        s(&x["id"]),
                        s(&x["kind"]),
                        s(&x["name"]),
                        s(&x["port"]),
                        sessions,
                        s(&x["to"]),
                        s(&x["owner"]),
                    ]
                })
                .collect();
            table(
                &["id", "kind", "name", "port", "sessions", "to", "owner"],
                rows,
            );
        }
        ServiceCmd::Add {
            name,
            to,
            to_session,
            sessions,
        } => {
            let (name, port) = name
                .rsplit_once(':')
                .context("name:port, e.g. db.internal:5432")?;
            let port: u16 = port.parse().context("the port")?;
            let to = match (to, to_session) {
                (Some(addr), _) => json!({ "addr": addr }),
                (None, Some(t)) => {
                    let (id, port) = t.rsplit_once(':').context("--to-session ID:port")?;
                    json!({ "session": id, "port": port.parse::<u32>().context("the port")? })
                }
                (None, None) => bail!("give --to or --to-session"),
            };
            let body = json!({ "name": name, "port": port, "sessions": label_pairs(&sessions)?, "to": to });
            let v = c
                .json(Method::POST, "/v1/services", Some(body.to_string()))
                .await?;
            println!(
                "service {}: {}:{} to {}; sessions also need a net rule for it",
                s(&v["id"]),
                s(&v["name"]),
                s(&v["port"]),
                s(&v["to"])
            );
        }
        ServiceCmd::Rm { id } => {
            let v = c
                .json(Method::DELETE, &format!("/v1/services/{id}"), None)
                .await?;
            println!("removed service {id} ({}:{})", s(&v["name"]), s(&v["port"]));
        }
    }
    Ok(())
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
        "mcp" => format!(
            "{} {}.{}{}",
            if e["ok"] == json!(true) {
                "ok"
            } else {
                "FAILED"
            },
            s(&e["server"]),
            s(&e["tool"]),
            e["error"]
                .as_str()
                .map(|x| format!(" ({x})"))
                .unwrap_or_default()
        ),
        other => format!("{other} {e}"),
    };
    let rule = e["rule"]
        .as_str()
        .map(|r| format!("  [{r}]"))
        .unwrap_or_default();
    format!("{t} {sess:<6} {:<7} {detail}{rule}", s(&e["type"]))
}

async fn serve(config_path: PathBuf) -> Result<()> {
    let d = zapper_core::Core::start(config_path.clone())?;
    #[cfg(feature = "office")]
    zapper_office::start(&d);
    d.add_config_check(office_config);
    d.check_config()
        .with_context(|| format!("loading {}", config_path.display()))?;
    let mount = mount(&d);
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
    let ui_addr = d.config().daemon.ui;
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
        let url = format!("http://{shown}/?token={}", d.ui_token());
        util::write_private(&d.state_dir().join("ui-url"), format!("{url}\n").as_bytes())?;
        if remote {
            eprintln!(
                "{}",
                util::exposed_warning(&format!("the UI (daemon.ui) listens on {bound}"))
            );
        }
        eprintln!("zapper: UI at {url}");
        let app = zapper_api::ui(d.clone(), port, remote, &mount);
        tokio::spawn(async move {
            if let Err(e) = axum::serve(tcp, app).await {
                tracing::error!("UI: {e}");
            }
        });
    }
    eprintln!("zapper: API on {}", socket.display());
    let app = zapper_api::api(d.clone(), &mount);
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
            d.shutdown().await;
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
            // `make deps` fetches vmlinux-6.1; `make kernel` builds the others.
            let fix = match p.engine.kernel.strip_prefix("vmlinux-6.1-") {
                Some(n) => format!("make kernel KERNEL={n}"),
                None => "make deps".into(),
            };
            report(
                k.is_file(),
                &format!("profile {name}: kernel {}", p.engine.kernel),
                &fix,
            );
        }
    }
    #[cfg(feature = "office")]
    for (name, firm) in &zapper_office::firm::all(&config) {
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
    if config.daemon.driver {
        let driven: Vec<&str> = config
            .profiles
            .keys()
            .filter(|n| config.resolve(n).is_ok_and(|p| p.agent.driven))
            .map(String::as_str)
            .collect();
        println!(
            "note   the driver is on: {}",
            if driven.is_empty() {
                "no profile is driven (agent.driven)".to_string()
            } else {
                format!("driven profiles: {}", driven.join(", "))
            }
        );
    } else {
        println!(
            "note   the driver is disabled (daemon.driver: false): sandboxing and monitoring only"
        );
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
    use serde_json::json;

    /// `zapper events`: a tool call through the gateway, and a refused one.
    #[test]
    fn mcp_events_read_as_the_tool_called() {
        let e = json!({ "ts": 3661.0, "session": "abc234", "type": "mcp", "server": "office", "tool": "mail_send", "ok": true, "ms": 2.5 });
        assert_eq!(
            super::event_line(&e),
            "01:01:01 abc234 mcp     ok office.mail_send"
        );
        let e = json!({ "ts": 0.0, "session": "abc234", "type": "mcp", "server": "x", "tool": "y", "ok": false, "ms": 0.1, "error": "no MCP server x for this session" });
        assert_eq!(
            super::event_line(&e),
            "00:00:00 abc234 mcp     FAILED x.y (no MCP server x for this session)"
        );
    }

    /// The fork's minimal config: shells, Claude Code and k3s, no firms.
    #[test]
    fn example_config_is_valid() {
        let c = zapper_policy::Config::from_yaml(super::EXAMPLE).unwrap();
        assert!(c.firms.is_empty());
        let names: Vec<&str> = c.profiles.keys().map(String::as_str).collect();
        assert_eq!(names, ["base", "claude", "default", "k3s"]);
        assert_eq!(c.resolve("base").unwrap().engine.image, "ubuntu-26.04");
    }

    /// `zapper run -p k3s`: k3s started at boot, a root shell with kubectl.
    #[test]
    fn k3s_profile() {
        let c = zapper_policy::Config::from_yaml(super::EXAMPLE).unwrap();
        let p = c.resolve("k3s").unwrap();
        assert_eq!(
            (p.engine.image.as_str(), p.engine.kernel.as_str()),
            ("k3s", "vmlinux-6.1-k8s")
        );
        assert!(p.agent.root);
        assert_eq!(p.agent.env["KUBECONFIG"], "/etc/rancher/k3s/k3s.yaml");
        assert!(p.setup.iter().any(|s| s.contains("nohup k3s server")));
        assert!(p.mounts.iter().any(|m| m.guest == "/work"));
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

    /// Firms need the office; the office is mounted on the API and UI only
    /// when it is in the build.
    #[test]
    fn firms_and_the_office_go_together() {
        let firms = zapper_policy::Config::from_yaml(
            "profiles:\n  w: { agent: { kind: claude-code } }\nfirms:\n  acme:\n    roles: { ceo: { profile: w, skills: { work: ceo } } }\n    workers: [{ name: ceo, role: ceo }]\n",
        )
        .unwrap();
        let e = super::office_config(&firms).err().map(|e| e.to_string());
        assert_eq!(e.is_none(), cfg!(feature = "office"), "{e:?}");
        assert!(super::office_config(&zapper_policy::Config::default()).is_ok());

        let t = zapper_core::testing::TestCore::new("");
        let m = super::mount(&t.core);
        assert_eq!(m.page.is_some(), cfg!(feature = "office"));
    }

    /// What `zapper init` writes loads in this build, office or not.
    #[test]
    fn init_writes_a_config_this_build_loads() {
        let c = zapper_policy::Config::from_yaml(super::example()).unwrap();
        super::office_config(&c).unwrap();
        assert!(c.profiles.contains_key("claude"));
        if !cfg!(feature = "office") {
            assert!(!super::example().contains("Firms (docs/office.md)"));
        }
    }
}
