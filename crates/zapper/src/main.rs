//! zapper: run programs in Firecracker microVMs whose network, exec and
//! filesystem access is set by layered rules, with credentials injected on
//! the host and everything that happens recorded.

mod client;
mod github;
use zapper_core::prelude::*;
use zapper_core::{images, util};
mod office;
mod ssh;
mod term;

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
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
    /// Print API responses as JSON.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

/// `--config` for the commands that read the config file (`init`, `check`,
/// `serve`). It is their own arg, not global: the client commands reach the
/// daemon over its socket and never read the config, so they do not take it.
#[derive(clap::Args)]
struct ConfigArg {
    /// Startup config file. Default: `config/zapper.yaml` when run in a
    /// zapper checkout, else `~/.config/zapper/zapper.yaml`.
    #[arg(long, env = "ZAPPER_CONFIG")]
    config: Option<String>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Write a starter config to --config if it does not exist.
    Init {
        /// Which config to write. `example`: a full starter config and, with
        /// the office, the default skills of its workers into `skills/` next
        /// to it (keeping those already there). `empty`: just the daemon
        /// defaults `serve` needs, and nothing else.
        #[arg(value_enum, default_value_t = Template::Example)]
        template: Template,
        #[command(flatten)]
        cfg: ConfigArg,
    },
    /// Check prerequisites; prints the fix for anything missing.
    Check {
        #[command(flatten)]
        cfg: ConfigArg,
    },
    /// Run the daemon.
    Serve {
        #[command(flatten)]
        cfg: ConfigArg,
    },
    /// Start a session, open its command, remove it on exit.
    Run {
        #[arg(short, long, default_value = "default")]
        profile: String,
        /// Keep the session after the command exits.
        #[arg(long)]
        keep: bool,
        /// Keep its disks when it stops, to start it again with `zapper
        /// start` (implies --keep); `zapper rm` removes them.
        #[arg(long)]
        persistent: bool,
        /// With --persistent: make it from its image even if the image
        /// changed since it was built; recorded.
        #[arg(long, requires = "persistent")]
        accept_changed_image: bool,
        /// Let SSH reach it (`ssh ID.zapper`); `zapper ssh off` stops that.
        #[arg(long)]
        ssh: bool,
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
        /// Keep its program running when this terminal closes (your
        /// connection drops, or you close it), to attach again with
        /// `--terminal`; `zapper hangup` ends it.
        #[arg(long, conflicts_with = "terminal")]
        persist: bool,
        /// Attach again to this persistent terminal, which is running: you
        /// see what it kept, then live; a client attached already is closed.
        #[arg(long)]
        terminal: Option<u64>,
        #[arg(last = true)]
        command: Vec<String>,
    },
    /// End a persistent terminal on purpose: its program gets `SIGHUP`.
    Hangup { id: String, terminal: u64 },
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
    /// Stop a session; it stays listed with its audit log (and a persistent
    /// one with its disks).
    Stop { id: String },
    /// Start a stopped persistent session again, from its kept disks, once
    /// they're checked against the hashes recorded when it stopped.
    Start {
        id: String,
        /// Boot its disks even if they changed while it was stopped; the
        /// change and this are recorded.
        #[arg(long)]
        accept_changed_disks: bool,
    },
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
        #[command(flatten)]
        cfg: ConfigArg,
    },
    #[command(flatten)]
    Office(office::Cmd),
    /// The record: who keeps it, and checking it against the keeper's chains.
    Record {
        #[command(subcommand)]
        cmd: RecordCmd,
    },
    /// GitHub Apps: set one up for an account, and look at it.
    Github {
        #[command(subcommand)]
        cmd: github::GithubCmd,
    },
    /// Print a short-lived credential for the daemon to hold (the source of a secret).
    Mint {
        #[command(subcommand)]
        cmd: github::MintCmd,
    },
    /// Services: names sessions use that the daemon connects itself.
    Service {
        #[command(subcommand)]
        cmd: ServiceCmd,
    },
    /// SSH into sandboxes: turned on and off per sandbox, and the gateway
    /// the host's sshd runs for a key.
    Ssh {
        #[command(subcommand)]
        cmd: ssh::SshCmd,
    },
}

#[derive(Subcommand)]
enum ServiceCmd {
    /// List services and zones.
    Ls,
    /// Add a service: a name and port sessions use, and where the daemon
    /// connects them.
    ///
    /// `zapper service add db.internal:5432 --to-session ID:5432 --for team=a`.
    /// Sessions still need a net rule for the name.
    Add {
        /// The name and port sessions use; `*.` in front for any name
        /// under it.
        #[arg(value_name = "NAME:PORT")]
        name: String,
        /// An address on the host's network, `ip:port`.
        #[arg(
            long,
            conflicts_with_all = ["to_session", "to_socket"],
            required_unless_present_any = ["to_session", "to_socket"]
        )]
        to: Option<String>,
        /// A port of a session's VM, `ID:port`.
        #[arg(long, conflicts_with = "to_socket")]
        to_session: Option<String>,
        /// A socket in the daemon's `state/services/` directory, by file
        /// name, served by a program on the host; each connection starts
        /// with a line naming the calling session.
        #[arg(long)]
        to_socket: Option<String>,
        /// With `--to-socket`: the daemon terminates TLS with the session's
        /// CA, and the socket gets the plain stream.
        #[arg(long, requires = "to_socket")]
        tls: bool,
        /// Only for sessions with this label, `key=value` (repeatable);
        /// every session without.
        #[arg(long = "for")]
        sessions: Vec<String>,
    },
    /// Remove a service added with `add`; a firm's services are the
    /// office's.
    Rm {
        /// The service's id, from `zapper service ls`.
        id: u64,
    },
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
    /// Checks a session's record: its files against the keeper's chains
    /// (with a keeper), and its disks' hashes from each stop to the start
    /// after it; exits 1 on any finding.
    Session { id: String },
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

/// The config `zapper init` writes.
#[derive(Clone, Copy, ValueEnum)]
enum Template {
    /// A full starter config, plus the office workers' default skills.
    Example,
    /// Only the daemon defaults `serve` needs; no profiles, policy or skills.
    Empty,
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
    /// Build an image by its recipe in the config (`images:`), e.g. `zapper image build office`; or, with `--from`, from an OCI image and `--run` steps given here.
    Build {
        name: String,
        /// One of the recipe's files, `NAME=PATH` (repeatable), instead of its default.
        #[arg(long = "arg", value_name = "NAME=PATH")]
        args: Vec<String>,
        /// Without a recipe: the OCI image to start from.
        #[arg(long)]
        from: Option<String>,
        /// Without a recipe: a command run in the image at build time (repeatable).
        #[arg(long)]
        run: Vec<String>,
        /// Image size, e.g. `4GiB` (default: content plus headroom).
        #[arg(long)]
        size: Option<String>,
        /// Build here, from the config file, even when the daemon runs; by
        /// default the daemon builds from its running config when it is up.
        #[arg(long)]
        local: bool,
    },
    Ls,
}

/// The config in the repository, which `zapper init` writes.
const EXAMPLE: &str = include_str!("../../../config/zapper.yaml");

fn load_config(path: &Path) -> Result<Config> {
    if !path.exists() {
        return Ok(Config::default());
    }
    Config::from_yaml(&std::fs::read_to_string(path)?)
        .and_then(|c| office::config(&c).map(|_| c))
        .with_context(|| format!("loading {}", path.display()))
}

fn main() {
    // The VMM shim must start before any thread does.
    let args: Vec<String> = std::env::args().collect();
    if let Some(code) = zapper_core::vmm_shim(&args) {
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
    let c = Client::new();
    match cli.cmd {
        Cmd::Init { template, cfg } => {
            let config_path = config_path(cfg.config.as_deref());
            if config_path.exists() {
                println!("{} exists; not touching it", config_path.display());
            } else {
                std::fs::create_dir_all(config_path.parent().unwrap())?;
                let contents = match template {
                    Template::Example => office::example().to_string(),
                    Template::Empty => Config::default().to_yaml(),
                };
                util::write_private(&config_path, contents.as_bytes())?;
                println!("wrote {}", config_path.display());
            }
            // The example also installs the office workers' skills; `empty`
            // is config only.
            if matches!(template, Template::Example) {
                office::init(config_path.parent().unwrap())?;
            }
        }
        Cmd::Check { cfg } => return check(&config_path(cfg.config.as_deref())).await,
        Cmd::Serve { cfg } => {
            let config_path = config_path(cfg.config.as_deref());
            // `serve` requires a config file, whether named with --config or the
            // default: a typo'd or missing path is an error, not a silent fall
            // back to an empty config. `zapper init empty` writes the minimal
            // one serve needs. (`check` reports a missing config itself.)
            if !config_path.exists() {
                bail!(
                    "no config file at {}; run `zapper init` to create one",
                    config_path.display()
                );
            }
            serve(config_path).await?
        }
        Cmd::Run {
            profile,
            keep,
            persistent,
            accept_changed_image,
            ssh,
            workdir,
            labels,
            command,
        } => {
            let keep = keep || persistent;
            let workdir = match workdir {
                Some(w) => std::fs::canonicalize(w)?,
                None => std::env::current_dir()?,
            };
            eprintln!("zapper: starting a {profile} session…");
            let body = serde_json::to_string(
                &json!({ "profile": profile, "workdir": workdir, "labels": label_pairs(&labels)?, "persistent": persistent, "accept_changed_image": accept_changed_image, "ssh": ssh }),
            )?;
            let info = c.json(Method::POST, "/v1/sessions", Some(body)).await?;
            let id = s(&info["id"]);
            eprintln!("zapper: session {id} is ready");
            let code = attach(&c, &id, &command, false).await;
            if persistent {
                eprintln!("zapper: session {id} kept with its disks; `zapper attach {id}`, `zapper stop {id}`, `zapper start {id}`, `zapper rm {id}`");
            } else if keep {
                eprintln!("zapper: session {id} kept; `zapper attach {id}`, `zapper rm {id}`");
            } else {
                c.json(Method::DELETE, &format!("/v1/sessions/{id}"), None)
                    .await?;
                eprintln!("zapper: session {id} removed; its log: `zapper audit {id}`");
            }
            return code;
        }
        Cmd::Attach {
            id,
            persist,
            terminal,
            command,
        } => {
            let tty = term::stdin_is_tty();
            let size = tty.then(|| term::size().unwrap_or((24, 80)));
            let (io, n) = c
                .terminal(&id, size, &command, false, persist, terminal)
                .await?;
            if let (true, Some(n)) = (persist || terminal.is_some(), n) {
                eprintln!(
                    "zapper: terminal {n} keeps running if you go: `zapper attach {id} --terminal {n}`, `zapper hangup {id} {n}`"
                );
            }
            return term::run(io, tty).await;
        }
        Cmd::Hangup { id, terminal } => {
            let v = c
                .json(
                    Method::DELETE,
                    &format!("/v1/sessions/{id}/terminals/{terminal}"),
                    None,
                )
                .await?;
            if cli.json {
                print_json(&v);
            } else {
                eprintln!(
                    "zapper: terminal {terminal} hung up{}",
                    v["exit_code"]
                        .as_i64()
                        .map(|c| format!(", its program exited {c}"))
                        .unwrap_or_default()
                );
            }
        }
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
                            x["disk_bytes"]
                                .as_u64()
                                .map(|b| format!("{} MiB", b >> 20))
                                .unwrap_or_default(),
                            format!("{}/{}", k["net_allowed"], k["net_denied"]),
                            format!("{}/{}", k["exec_allowed"], k["exec_denied"]),
                            if x["ssh"] == true { "on" } else { "" }.to_string(),
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
                        "kept disks",
                        "net ok/no",
                        "exec ok/no",
                        "ssh",
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
        Cmd::Start {
            id,
            accept_changed_disks,
        } => {
            eprintln!("zapper: checking {id}'s disks and starting it…");
            let body = json!({ "accept_changed_disks": accept_changed_disks }).to_string();
            let v = c
                .json(
                    Method::POST,
                    &format!("/v1/sessions/{id}/start"),
                    Some(body),
                )
                .await?;
            if cli.json {
                print_json(&v);
            } else {
                eprintln!("zapper: session {id} is ready; `zapper attach {id}`");
            }
        }
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
        Cmd::Office(cmd) => return office::run(&c, cmd, cli.json).await,
        Cmd::Github { cmd } => return github::github(cmd).await,
        Cmd::Mint { cmd } => return github::mint(cmd).await,
        Cmd::Service { cmd } => service(&c, cmd, cli.json).await?,
        Cmd::Ssh { cmd } => return ssh::ssh(&c, cmd, cli.json).await,
        Cmd::Record { cmd } => {
            let (path, verify) = match &cmd {
                RecordCmd::Status => ("/v1/record".to_string(), false),
                #[cfg(feature = "office")]
                RecordCmd::Verify { firm, run } => {
                    (format!("/v1/firms/{firm}/runs/{run}/verify"), true)
                }
                RecordCmd::Session { id } => {
                    let v = c
                        .json(Method::GET, &format!("/v1/sessions/{id}/verify"), None)
                        .await?;
                    if cli.json {
                        print_json(&v);
                    } else {
                        print_session_check(&v);
                    }
                    return Ok(if v["ok"] == json!(true) { 0 } else { 1 });
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
        Cmd::Image { cmd, cfg } => {
            let config = load_config(&config_path(cfg.config.as_deref()))?;
            let dir = PathBuf::from(expand_home(&config.daemon.images));
            match cmd {
                ImageCmd::Build {
                    name,
                    args,
                    from,
                    run,
                    size,
                    local,
                } => {
                    let daemon = !local
                        && tokio::net::UnixStream::connect(client::socket_path())
                            .await
                            .is_ok();
                    if daemon {
                        return build_in_daemon(&c, &name, &args, from, run, size).await;
                    }
                    let r = images::recipe(&config, &name, from, run, &args)?;
                    let out = images::build(
                        images::Build {
                            name: &name,
                            recipe: &r,
                            size: size.as_deref(),
                            dir: &dir,
                        },
                        &|l| eprintln!("{l}"),
                    )?;
                    println!("built {}", out.display());
                }
                ImageCmd::Ls => {
                    let v = images::list(&dir)?;
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
                                    i["sha256"]
                                        .as_str()
                                        .map(|h| h[..h.len().min(12)].to_string())
                                        .unwrap_or_else(|| "-".into()),
                                ]
                            })
                            .collect();
                        table(&["name", "size", "from", "sha256"], rows);
                    }
                }
            }
        }
    }
    Ok(0)
}

/// `zapper record session`, as people read it.
fn print_session_check(v: &Value) {
    let chain = &v["chain"];
    match chain["ok"].as_bool() {
        None => println!(
            "note   chains not checked: {}",
            chain["note"].as_str().unwrap_or("")
        ),
        Some(_) => {
            for f in chain["files"].as_array().into_iter().flatten() {
                if f["ok"] == json!(true) {
                    println!("ok     {} ({} lines)", s(&f["file"]), f["lines"]);
                } else {
                    println!("BAD    {}: {}", s(&f["file"]), s(&f["error"]));
                }
            }
        }
    }
    let findings = v["findings"].as_array().cloned().unwrap_or_default();
    for f in &findings {
        println!("FOUND  event {}: {}", f["seq"], s(&f["finding"]));
    }
    if findings.is_empty() {
        println!("ok     the disks: each start checked what the stop before it recorded");
    }
}

/// `zapper image build` through the daemon: it builds from its running
/// config; its log is printed as it comes, and the exit code says how it
/// ended.
async fn build_in_daemon(
    c: &Client,
    name: &str,
    args: &[String],
    from: Option<String>,
    run: Vec<String>,
    size: Option<String>,
) -> Result<i32> {
    let mut named = serde_json::Map::new();
    for a in args {
        let (k, v) = a
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("--arg {a:?}: NAME=PATH"))?;
        // The daemon reads the files: give it absolute paths.
        let path = std::path::absolute(v).unwrap_or_else(|_| PathBuf::from(v));
        named.insert(k.into(), json!(path));
    }
    let body = json!({ "args": named, "from": from, "run": run, "size": size });
    let started = c
        .json(
            Method::POST,
            &format!("/v1/images/{name}/build"),
            Some(body.to_string()),
        )
        .await?;
    eprintln!(
        "the daemon is building {name} (build {})",
        s(&started["id"])
    );
    let mut body = c
        .stream(&format!("/v1/images/builds/{}", s(&started["id"])))
        .await?;
    let mut buf = String::new();
    while let Some(frame) = body.frame().await {
        let frame = frame?;
        let Some(data) = frame.data_ref() else {
            continue;
        };
        buf.push_str(&String::from_utf8_lossy(data));
        while let Some(i) = buf.find('\n') {
            let line: String = buf.drain(..=i).collect();
            let Some(v) = line
                .trim()
                .strip_prefix("data:")
                .and_then(|j| serde_json::from_str::<Value>(j.trim()).ok())
            else {
                continue;
            };
            if let Some(l) = v["line"].as_str() {
                eprintln!("{l}");
            } else if let Some(n) = v["dropped"].as_u64() {
                eprintln!("({n} lines not kept)");
            } else if let Some(path) = v["end"]["built"].as_str() {
                println!("built {path}");
                return Ok(0);
            } else if let Some(why) = v["end"]["failed"].as_str() {
                eprintln!("zapper: building {name} failed: {why}");
                return Ok(1);
            }
        }
    }
    bail!("the daemon's build log ended without saying how the build went")
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
            to_socket,
            tls,
            sessions,
        } => {
            let (name, port) = name
                .rsplit_once(':')
                .context("name:port, e.g. db.internal:5432")?;
            let port: u16 = port.parse().context("the port")?;
            let to = match (to, to_session, to_socket) {
                (Some(addr), _, _) => json!({ "addr": addr }),
                (None, Some(t), _) => {
                    let (id, port) = t.rsplit_once(':').context("--to-session ID:port")?;
                    json!({ "session": id, "port": port.parse::<u32>().context("the port")? })
                }
                (None, None, Some(name)) => json!({ "socket": name, "tls": tls }),
                (None, None, None) => bail!("give --to, --to-session or --to-socket"),
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

/// An event of the live stream as a line: its time, session and type, then
/// the core's verdict and summary; an `ask` with how to answer it.
fn event_line(e: &Value) -> String {
    let ts = e["ts"].as_f64().unwrap_or(0.0) as i64;
    let t = format!(
        "{:02}:{:02}:{:02}",
        (ts / 3600) % 24,
        (ts / 60) % 60,
        ts % 60
    );
    let sess = s(&e["session"]);
    let verdict = e["verdict"]
        .as_str()
        .map(|v| format!("{v} "))
        .unwrap_or_default();
    let hint = if e["type"] == "ask" {
        format!(
            "  (zapper approve {} | zapper deny {})",
            s(&e["id"]),
            s(&e["id"])
        )
    } else {
        String::new()
    };
    let rule = e["rule"]
        .as_str()
        .map(|r| format!("  [{r}]"))
        .unwrap_or_default();
    format!(
        "{t} {sess:<6} {:<7} {verdict}{}{hint}{rule}",
        s(&e["type"]),
        s(&e["summary"])
    )
}

async fn serve(config_path: PathBuf) -> Result<()> {
    let d = zapper_core::Core::start(config_path.clone())?;
    office::start(&d);
    d.check_config()
        .with_context(|| format!("loading {}", config_path.display()))?;
    let mount = office::mount(&d);
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
            let img = zapper_core::images::disk(&images, &p.engine.image);
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
    office::check(&config, &mut report);
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
        .any(|p| p.engine.nic());
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
    let oci = images::engine();
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
    // Firecracker's uid: a subordinate one per session, where the host has
    // them.
    if !d.vmm_subids {
        println!("note   Firecracker runs as this user (daemon.vmm_subids is off)");
    } else {
        match zapper_core::subids_host_range() {
            Ok((start, end)) => report(
                true,
                &format!(
                    "Firecracker runs as a uid of each session's own ({start}–{end}, from /etc/subuid)"
                ),
                "",
            ),
            Err(why) => println!("note   Firecracker runs as this user: {why} (sudo usermod --add-subuids 100000-165535 --add-subgids 100000-165535 $USER, and sudo apt install uidmap, give each session its own)"),
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
        // What it drives: firms' workers (through the office's foreman) and
        // sessions of driven profiles.
        let firms: Vec<&str> = config.firms.keys().map(String::as_str).collect();
        let mut uses = Vec::new();
        if !firms.is_empty() {
            uses.push(format!("the workers of firm {}", firms.join(", ")));
        }
        if !driven.is_empty() {
            uses.push(format!("driven profiles: {}", driven.join(", ")));
        }
        if uses.is_empty() {
            println!("note   the driver is on, unused: no firm, and no profile sets agent.driven");
        } else {
            println!("note   the driver is on, for {}", uses.join("; "));
        }
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

    /// An event as `zapper events` prints it: the core's, at 01:01:01, of
    /// session abc234 (none for a service).
    fn line(body: zapper_core::EventBody) -> String {
        let session =
            (!matches!(body, zapper_core::EventBody::Service { .. })).then(|| "abc234".to_string());
        let e = zapper_core::Event {
            seq: 1,
            epoch: "ab3cd4ef".into(),
            ts: 3661.0,
            session,
            body,
        };
        super::event_line(&e.view())
    }

    /// `zapper events`: one line for each type of event.
    #[test]
    fn events_read_as_a_line_each() {
        use zapper_core::EventBody as B;
        use zapper_policy::proto::GuestEvent;
        use zapper_policy::{Action, Kind};
        let net = |method: Option<&str>, status, error: Option<&str>| B::Net {
            host: "api.example.com".into(),
            port: 443,
            method: method.map(Into::into),
            path: method.map(|_| "/v1/x".into()),
            action: Action::Allow,
            rule: Some("g-1".into()),
            scope: Some("global".into()),
            inspected: method.is_some(),
            status,
            secrets: vec!["gh".into()],
            error: error.map(Into::into),
        };
        let cases = [
            (net(Some("GET"), Some(200), None), "01:01:01 abc234 net     allow GET api.example.com:443/v1/x → 200  [secrets: gh]  [g-1]"),
            (net(None, None, Some("refused")), "01:01:01 abc234 net     allow connect api.example.com:443  [secrets: gh]  refused  (tunnel)  [g-1]"),
            (
                B::Exec {
                    pid: 7,
                    uid: 0,
                    path: "/usr/bin/git".into(),
                    argv: vec!["git".into(), "push".into()],
                    cwd: None,
                    action: Action::Deny,
                    rule: None,
                    scope: None,
                    wait_ms: 1.25,
                },
                "01:01:01 abc234 exec    deny git push  (1.2 ms)",
            ),
            (
                B::Ask {
                    id: "a-1".into(),
                    kind: Kind::Net,
                    subject: "example.org:443".into(),
                },
                "01:01:01 abc234 ask     ask a-1: example.org:443  (zapper approve a-1 | zapper deny a-1)",
            ),
            (
                B::Answer {
                    id: "a-1".into(),
                    allow: true,
                    remember: Some("session".into()),
                },
                "01:01:01 abc234 answer  allow a-1 · session",
            ),
            (
                B::Answer {
                    id: "a-2".into(),
                    allow: false,
                    remember: None,
                },
                "01:01:01 abc234 answer  deny a-2",
            ),
            (
                B::Dns {
                    name: "example.org".into(),
                    qtype: 1,
                    allowed: true,
                    rcode: "noerror".into(),
                    answer: Some("100.64.0.9".into()),
                    reason: None,
                },
                "01:01:01 abc234 dns     allow noerror example.org → 100.64.0.9",
            ),
            (
                B::Dns {
                    name: "".into(),
                    qtype: 1,
                    allowed: false,
                    rcode: "formerr".into(),
                    answer: None,
                    reason: Some("truncated name".into()),
                },
                "01:01:01 abc234 dns     deny formerr (invalid query)  (truncated name)",
            ),
            (
                B::Guest {
                    event: GuestEvent::Exit {
                        pid: 9,
                        ppid: 1,
                        uid: 0,
                        comm: "make".into(),
                        exit_code: 2,
                        signal: None,
                        elapsed_ms: 40,
                        user_ms: 1,
                        sys_ms: 1,
                        mem_kib: 1,
                        io_bytes: 1,
                    },
                },
                "01:01:01 abc234 guest   make pid 9 exit 2 · 40 ms",
            ),
            (
                B::Guest {
                    event: GuestEvent::Setup {
                        step: "apt install".into(),
                        ok: false,
                        output: "".into(),
                    },
                },
                "01:01:01 abc234 guest   setup FAILED: apt install",
            ),
            (
                B::Guest {
                    event: GuestEvent::ShellExit {
                        id: 3,
                        exit_code: 0,
                    },
                },
                "01:01:01 abc234 guest   shell 3 exited with 0",
            ),
            (
                B::Shell {
                    id: 3,
                    state: "open".into(),
                    argv: Some(vec!["bash".into()]),
                    exit_code: None,
                    client: None,
                },
                "01:01:01 abc234 shell   shell 3 open: bash",
            ),
            (
                B::Session {
                    state: "failed".into(),
                    detail: Some("no image".into()),
                },
                "01:01:01 abc234 session failed: no image",
            ),
            (
                B::Config {
                    change: "rule g-2 added".into(),
                },
                "01:01:01 abc234 config  rule g-2 added",
            ),
            (
                B::Service {
                    change: "added".into(),
                    service: zapper_core::ServiceInfo {
                        id: 4,
                        kind: "service",
                        name: "git.acme.internal".into(),
                        port: Some(443),
                        sessions: [("firm".to_string(), "acme".to_string())].into(),
                        to: "in the daemon".into(),
                        owner: "office".into(),
                    },
                },
                "01:01:01        service added service git.acme.internal:443 → in the daemon for firm=acme (office)",
            ),
            (
                B::Driver {
                    event: "turn_end".into(),
                    detail: json!({ "outcome": "done", "tokens": 1200 }),
                },
                "01:01:01 abc234 driver  turn_end done · 1200 tokens",
            ),
            (
                B::Mcp {
                    server: "office".into(),
                    tool: "mail_send".into(),
                    ok: true,
                    ms: 2.5,
                    error: None,
                },
                "01:01:01 abc234 mcp     office.mail_send · 2.5 ms",
            ),
            (
                B::Mcp {
                    server: "x".into(),
                    tool: "y".into(),
                    ok: false,
                    ms: 0.1,
                    error: Some("no MCP server x for this session".into()),
                },
                "01:01:01 abc234 mcp     deny x.y · 0.1 ms  no MCP server x for this session",
            ),
            (
                B::Exec {
                    pid: 8,
                    uid: 1000,
                    path: "/usr/bin/curl".into(),
                    argv: vec!["curl".into()],
                    cwd: None,
                    action: Action::Ask,
                    rule: None,
                    scope: None,
                    wait_ms: 2000.0,
                },
                "01:01:01 abc234 exec    ask curl  (2000.0 ms)",
            ),
            (
                B::Guest {
                    event: GuestEvent::Exit {
                        pid: 10,
                        ppid: 1,
                        uid: 0,
                        comm: "sleep".into(),
                        exit_code: 0,
                        signal: Some(9),
                        elapsed_ms: 5,
                        user_ms: 0,
                        sys_ms: 0,
                        mem_kib: 1,
                        io_bytes: 0,
                    },
                },
                "01:01:01 abc234 guest   sleep pid 10 exit signal 9 · 5 ms",
            ),
            (
                B::Guest {
                    event: GuestEvent::Usage {
                        load1: 0.5,
                        mem_used_kib: 1024,
                        procs: 12,
                        disk_used_kib: 2048,
                    },
                },
                "01:01:01 abc234 guest   {\"event\":\"usage\",\"load1\":0.5,\"mem_used_kib\":1024,\"procs\":12,\"disk_used_kib\":2048}",
            ),
            (
                B::Shell {
                    id: 4,
                    state: "closed".into(),
                    argv: None,
                    exit_code: Some(130),
                    client: None,
                },
                "01:01:01 abc234 shell   shell 4 closed exit 130",
            ),
            (
                B::Service {
                    change: "removed".into(),
                    service: zapper_core::ServiceInfo {
                        id: 5,
                        kind: "zone",
                        name: "acme.internal".into(),
                        port: None,
                        sessions: Default::default(),
                        to: "nothing but its services".into(),
                        owner: "operator".into(),
                    },
                },
                "01:01:01        service removed zone acme.internal → nothing but its services (operator)",
            ),
            (
                B::Driver {
                    event: "error".into(),
                    detail: json!({ "error": "claude not found" }),
                },
                "01:01:01 abc234 driver  deny error  claude not found",
            ),
            (
                B::Driver {
                    event: "turn_end".into(),
                    detail: json!({ "outcome": "failed", "activity": "Edit a.md", "prompts": 2 }),
                },
                "01:01:01 abc234 driver  deny turn_end Edit a.md failed · 2 prompts",
            ),
        ];
        for (body, want) in cases {
            assert_eq!(line(body), want);
        }
    }

    /// An example's directory: a config this build loads, whose profiles
    /// are all on the one image the firms need (`zapper check` asks for
    /// every profile's image), each firm's skills there, and a task with a
    /// subject.
    #[cfg(feature = "office")]
    fn example(name: &str) -> zapper_policy::Config {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples")
            .join(name);
        let c = super::load_config(&dir.join("zapper.yaml")).unwrap();
        for p in c.profiles.keys() {
            assert_eq!(c.resolve(p).unwrap().engine.image, "office", "{name}: {p}");
        }
        for firm in zapper_office::firm::all(&c).values() {
            assert_eq!(zapper_office::skills::missing(firm), Vec::<String>::new());
        }
        let task = std::fs::read_to_string(dir.join("task.md")).unwrap();
        assert!(!task.lines().next().unwrap().trim().is_empty());
        c
    }

    /// `examples/mini-firm`: Ada alone, who may contract a writer, named
    /// Ben from the pool.
    #[cfg(feature = "office")]
    #[test]
    fn the_mini_firm_example_loads() {
        let c = example("mini-firm");
        assert_eq!(c.profiles.len(), 1);
        let mini = &zapper_office::firm::all(&c)["mini"];
        let founders: Vec<&str> = mini.workers.iter().map(|w| w.name.as_str()).collect();
        assert_eq!(founders, ["ada"]);
        assert_eq!(mini.roles["ceo"].hires, ["writer"]);
        assert!(mini.budget.seed > 0);
        assert_eq!(mini.names, ["ben"]);
    }

    /// `examples/shopping-list`: Ada alone, who may hire a product manager
    /// and engineers; a git repository, which the profile leaves to grants;
    /// npm for the engineers only.
    #[cfg(feature = "office")]
    #[test]
    fn the_shopping_list_example_loads() {
        let c = example("shopping-list");
        let shop = &zapper_office::firm::all(&c)["shop"];
        let founders: Vec<&str> = shop.workers.iter().map(|w| w.name.as_str()).collect();
        assert_eq!(founders, ["ada"]);
        assert_eq!(shop.roles["ceo"].hires, ["product-manager", "engineer"]);
        assert!(shop.budget.seed > 0);
        assert_eq!(shop.git_repos(), ["product"]);
        let hosts = |profile: &str| -> Vec<(String, zapper_policy::Action)> {
            c.resolve(profile)
                .unwrap()
                .layers
                .iter()
                .flat_map(|(_, l)| l.net.iter().map(|n| (n.host.clone(), n.action)))
                .collect()
        };
        let git = format!("git.{}", shop.domain("shop"));
        let npm = "registry.npmjs.org".to_string();
        for role in ["ceo", "product-manager", "engineer"] {
            let h = hosts(&shop.roles[role].profile);
            assert!(
                h.contains(&(git.clone(), zapper_policy::Action::Deny)),
                "{role}: {h:?}"
            );
            assert_eq!(
                h.iter().any(|(x, _)| *x == npm),
                role == "engineer",
                "{role}: {h:?}"
            );
        }
    }

    /// `examples/github-firm`: the shopping list company with its
    /// repositories on GitHub, behind the firm's own git server.
    #[cfg(feature = "office")]
    #[test]
    fn the_github_firm_example_loads() {
        let c = example("github-firm");
        let octo = &zapper_office::firm::all(&c)["octo"];
        let founders: Vec<&str> = octo.workers.iter().map(|w| w.name.as_str()).collect();
        assert_eq!(founders, ["ada", "lin"]);
        assert_eq!(octo.roles["librarian"].agent.as_deref(), Some("librarian"));
        assert!(octo.git_internal());
        assert_eq!(octo.git_remote().unwrap().config, "acme.yaml");
        assert!(octo.git.as_ref().unwrap().dir.is_some());
        // Its knowledge base is kb, whose main is the firm's.
        assert_eq!(octo.knowledge_base().as_deref(), Some("kb"));
        assert!(octo.main_is_local("kb") && !octo.main_is_local("shopping-list"));
    }

    /// `examples/research-firm`: Ada alone, who may hire a research
    /// manager, who may hire researchers, a prototype engineer, and a
    /// process lead; drive spaces for the research and the board pack, a
    /// repository for prototypes.
    #[cfg(feature = "office")]
    #[test]
    fn the_research_firm_example_loads() {
        let c = example("research-firm");
        let lab = &zapper_office::firm::all(&c)["lab"];
        let founders: Vec<&str> = lab.workers.iter().map(|w| w.name.as_str()).collect();
        assert_eq!(founders, ["ada"]);
        assert_eq!(
            lab.roles["ceo"].hires,
            [
                "research-manager",
                "prototype-engineer",
                "process-improvement"
            ]
        );
        assert_eq!(lab.roles["research-manager"].hires, ["researcher"]);
        assert!(lab.budget.seed > 0);
        assert_eq!(lab.git_repos(), ["prototypes"]);
        for space in ["research", "board"] {
            assert!(lab.credentials.spaces.iter().any(|s| s == space), "{space}");
        }
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
            "profiles:\n  w: { agent: { kind: claude-code } }\nfirms:\n  acme:\n    roles: { ceo: { profile: w, agent: ceo } }\n    workers: [{ name: ceo, role: ceo }]\n",
        )
        .unwrap();
        let e = super::office::config(&firms).err().map(|e| e.to_string());
        assert_eq!(e.is_none(), cfg!(feature = "office"), "{e:?}");
        assert!(super::office::config(&zapper_policy::Config::default()).is_ok());

        let t = zapper_core::testing::TestCore::new("");
        let m = super::office::mount(&t.core);
        assert_eq!(m.page.is_some(), cfg!(feature = "office"));
    }

    /// What `zapper init` writes loads in this build, office or not.
    #[test]
    fn init_writes_a_config_this_build_loads() {
        let c = zapper_policy::Config::from_yaml(super::office::example()).unwrap();
        super::office::config(&c).unwrap();
        assert!(c.profiles.contains_key("claude"));
        if !cfg!(feature = "office") {
            assert!(!super::office::example().contains("Firms (docs/office.md)"));
        }
    }
}
