//! `zapper firm`, `zapper mail` and `zapper drive`: the office from the
//! command line, as the board.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::Subcommand;
use http_body_util::BodyExt;
use hyper::Method;
use serde_json::{json, Value};

use crate::client::{url_encode, Client};
use crate::{print_json, s, table};

#[derive(Subcommand)]
pub enum FirmCmd {
    /// List firms and their runs' states.
    Ls,
    /// Start a fresh run: boot the facilities, then the founders, idle.
    Up {
        firm: String,
        /// The company's name for this run (default: the config's).
        #[arg(long)]
        company: Option<String>,
        /// The CEO's name for this run.
        #[arg(long)]
        ceo: Option<String>,
        /// The seed budget, like 500k.
        #[arg(long)]
        seed: Option<String>,
        /// A file with extra instructions for the CEO, over their skill.
        #[arg(long, value_name = "FILE")]
        ceo_prompt: Option<std::path::PathBuf>,
        /// A file with the board's own articles for the charter, which every
        /// worker reads.
        #[arg(long, value_name = "FILE")]
        articles: Option<std::path::PathBuf>,
        /// A GitHub repository to work in, OWNER/NAME; repeat for more (a
        /// firm with `git.github`). Checked against the owner's GitHub App.
        #[arg(long = "repo", value_name = "OWNER/NAME")]
        repo: Vec<String>,
        /// Security's share of every grant, from 0 to 1.
        #[arg(long)]
        security_share: Option<f64>,
        /// Workers in a turn at the same time.
        #[arg(long)]
        max_concurrent: Option<u32>,
        /// Turns the whole firm may take per day; 0 is no limit.
        #[arg(long)]
        daily_turns: Option<u32>,
        /// A scenario of the firm's config to play: hires with extra
        /// instructions and board actions (`zapper firm scenarios`).
        #[arg(long)]
        scenario: Option<String>,
        /// A file with the board's task for the CEO, sent as the first
        /// assignment once the firm is up: its first line is the subject.
        #[arg(long, value_name = "FILE")]
        task: Option<std::path::PathBuf>,
        /// A file to attach to the task, on the shared drive; repeat for
        /// more (up to 10, 5 MiB together).
        #[arg(long = "attach", value_name = "FILE", requires = "task")]
        attach: Vec<std::path::PathBuf>,
    },
    /// The firm's scenarios, with their hire slots and script.
    Scenarios {
        firm: String,
    },
    /// Stop the run; its journal, transcripts and shared drive stay on disk.
    Down {
        firm: String,
    },
    /// Workers' states, budgets and the run.
    Status {
        firm: String,
    },
    /// Stop starting turns; workers finish the turn they are in.
    Pause {
        firm: String,
    },
    Resume {
        firm: String,
    },
    /// Past runs, newest first.
    Runs {
        firm: String,
    },
    /// Put a note on a run (any text, to find it again later); an empty
    /// note removes it.
    Note {
        firm: String,
        run: String,
        note: String,
    },
    /// Delete a past run's directory.
    Prune {
        firm: String,
        run: String,
    },
    /// The run's journal (mail, states, turns, access, publishes).
    Journal {
        firm: String,
        #[arg(long, default_value = "current")]
        run: String,
        #[arg(short, long)]
        follow: bool,
    },
    /// A worker's transcript: what their agent (Claude Code or pi) did.
    Transcript {
        firm: String,
        worker: String,
        #[arg(long, default_value = "current")]
        run: String,
        #[arg(short, long)]
        follow: bool,
    },
    /// Print the UI address that replays a run.
    Replay {
        firm: String,
        run: String,
    },
    /// Access requests and standing approvals.
    Security {
        firm: String,
    },
    /// Decide an access request as the board.
    Decide {
        firm: String,
        request: String,
        #[arg(long, conflicts_with = "deny")]
        allow: bool,
        #[arg(long)]
        deny: bool,
        /// once, worker or role.
        #[arg(long, default_value = "once")]
        scope: String,
        #[arg(long, default_value = "decided by the board")]
        reason: String,
    },
    /// Remove a standing approval.
    Revoke {
        firm: String,
        rule: String,
    },
    /// Approve a new hire's background check as the board, or object.
    Screen {
        firm: String,
        screening: String,
        #[arg(long, alias = "clear", conflicts_with = "object")]
        approve: bool,
        #[arg(long)]
        object: bool,
        #[arg(long, default_value = "")]
        note: String,
    },
    /// The run's contracts: offers, quotes, counters, agreed terms, how they ended.
    Contracts {
        firm: String,
    },
    /// Grants and credential requests; or the board's action:
    /// `grant WORKER RESOURCE LEVEL`, `approve REQUEST`, `deny REQUEST`,
    /// `revoke WORKER RESOURCE` (resources: git, cluster, drive:<space>).
    Credentials {
        firm: String,
        /// grant, approve, deny or revoke
        action: Option<String>,
        /// The action's arguments.
        args: Vec<String>,
        /// For approve: a lower level than asked.
        #[arg(long)]
        level: Option<String>,
        /// How long the grant lasts, e.g. 8h (default: the firm's).
        #[arg(long)]
        expires: Option<String>,
        /// A note to the worker (required to deny; the reason to revoke).
        #[arg(long, default_value = "")]
        note: String,
    },
    /// The board gives a worker tokens, on top of the approved budget
    /// (security officers who ran out, for one).
    Fund {
        firm: String,
        worker: String,
        /// Tokens, like 200k
        tokens: String,
        #[arg(long)]
        reason: String,
    },
    /// The board lets a worker go (the only way to let a security officer
    /// go).
    Release {
        firm: String,
        worker: String,
        #[arg(long)]
        reason: String,
    },
    /// The run's budget: what the board approved, balances, the CEO's
    /// requests; with approve or decline, the board's answer to a request.
    Budget {
        firm: String,
        /// approve or decline
        action: Option<String>,
        /// The request, e.g. b-1
        request: Option<String>,
        /// A note to the CEO, e.g. why it was declined.
        #[arg(long, default_value = "")]
        note: String,
    },
}

#[derive(Subcommand)]
pub enum MailCmd {
    /// Write as the board: `zapper mail send acme --to ceo --subject … --kind assignment < brief.md`
    Send {
        firm: String,
        #[arg(long, value_delimiter = ',', required = true)]
        to: Vec<String>,
        #[arg(long, value_delimiter = ',')]
        cc: Vec<String>,
        #[arg(long, default_value = "")]
        subject: String,
        /// The body; read from stdin if not given.
        #[arg(long)]
        body: Option<String>,
        /// assignment, question, fyi, reply or report.
        #[arg(long, default_value = "assignment")]
        kind: String,
        /// Relative, e.g. 2h or 1d.
        #[arg(long)]
        due: Option<String>,
        /// Answer this message, in its thread.
        #[arg(long)]
        reply_to: Option<String>,
    },
    /// The board's inbox (or a worker's with --for).
    Ls {
        firm: String,
        #[arg(long = "for")]
        who: Option<String>,
        #[arg(long)]
        unread: bool,
    },
    /// A thread with its messages.
    Thread { firm: String, thread: String },
}

#[derive(Subcommand)]
pub enum DriveCmd {
    /// Artifacts of the current run.
    Ls { firm: String },
    /// Download an artifact's files: `zapper drive get acme a-3@v2 ./out`
    Get {
        firm: String,
        artifact: String,
        dir: PathBuf,
        #[arg(long, default_value = "current")]
        run: String,
    },
}

fn time(ts: &Value) -> String {
    let t = ts.as_f64().unwrap_or(0.0) as i64;
    format!("{:02}:{:02}:{:02}", (t / 3600) % 24, (t / 60) % 60, t % 60)
}

/// One journal entry as a line of text.
pub fn journal_line(e: &Value) -> String {
    let t = time(&e["ts"]);
    let what = match e["type"].as_str().unwrap_or("") {
        "mail" => format!(
            "mail {} {} → {} [{}] \"{}\"{}",
            s(&e["id"]),
            s(&e["from"]),
            e["to"]
                .as_array()
                .map(|a| a.iter().map(s).collect::<Vec<_>>().join(","))
                .unwrap_or_default(),
            s(&e["kind"]),
            s(&e["subject"]),
            if e["automatic"] == json!(true) {
                " (automatic)"
            } else {
                ""
            }
        ),
        "worker" => format!(
            "{} is {}{}",
            s(&e["worker"]),
            s(&e["state"]),
            e["detail"]
                .as_str()
                .map(|d| format!(": {d}"))
                .unwrap_or_default()
        ),
        "turn" => format!(
            "turn {} {}{}",
            s(&e["worker"]),
            s(&e["event"]),
            e["ms"]
                .as_u64()
                .map(|m| format!(" {:.1}s", m as f64 / 1000.0))
                .unwrap_or_default()
        ),
        "access" => {
            let r = &e["request"];
            format!(
                "access {} {} {} {}:{}",
                s(&e["event"]),
                s(&r["id"]),
                s(&r["worker"]),
                s(&r["host"]),
                r["port"]
            )
        }
        "published" => format!(
            "published {}@v{} \"{}\" by {}",
            s(&e["artifact"]),
            e["version"],
            s(&e["title"]),
            s(&e["author"])
        ),
        "run" => format!(
            "run {}{}",
            s(&e["state"]),
            e["reason"]
                .as_str()
                .map(|r| format!(" ({r})"))
                .unwrap_or_default()
        ),
        "hired" => format!(
            "hired {} as {} by {}, budget {}: {}",
            s(&e["worker"]["name"]),
            s(&e["worker"]["title"]),
            s(&e["by"]),
            e["budget"],
            s(&e["reason"])
        ),
        "screening" => match e["event"].as_str().unwrap_or("") {
            "requested" => format!(
                "screening {} of {} by {}",
                s(&e["screening"]["id"]),
                s(&e["screening"]["worker"]),
                e["screening"]["officer"].as_str().unwrap_or("the board")
            ),
            "decided" => format!(
                "screening {} {} {} by {}{}",
                s(&e["id"]),
                s(&e["worker"]),
                if e["cleared"] == json!(true) {
                    "cleared"
                } else {
                    "objected"
                },
                s(&e["by"]),
                e["note"]
                    .as_str()
                    .filter(|n| !n.is_empty())
                    .map(|n| format!(": {n}"))
                    .unwrap_or_default()
            ),
            other => format!("screening {other} {}", s(&e["id"])),
        },
        "security" => match e["event"].as_str().unwrap_or("") {
            "officers" => format!(
                "security officers: {} ({}, by {})",
                e["officers"]
                    .as_array()
                    .map(|a| a.iter().map(s).collect::<Vec<_>>().join(", "))
                    .filter(|x| !x.is_empty())
                    .unwrap_or_else(|| "none".into()),
                s(&e["reason"]),
                s(&e["by"])
            ),
            "reassigned" => format!(
                "security {} to {}",
                s(&e["id"]),
                e["officer"].as_str().unwrap_or("the board")
            ),
            other => format!("security {other} {e}"),
        },
        "credentials" => match e["event"].as_str().unwrap_or("") {
            "requested" => format!(
                "credentials {} {} asks {} {}",
                s(&e["request"]["id"]),
                s(&e["request"]["worker"]),
                s(&e["request"]["resource"]),
                s(&e["request"]["level"])
            ),
            "decided" => format!(
                "credentials {} {} {} by {}",
                s(&e["id"]),
                if e["decision"]["grant"] == json!(true) {
                    "granted"
                } else {
                    "denied"
                },
                s(&e["resource"]),
                s(&e["decision"]["by"])
            ),
            "escalated" => format!(
                "credentials {} escalated by {} to {}",
                s(&e["id"]),
                s(&e["by"]),
                s(&e["to"])
            ),
            "granted" => format!(
                "credentials grant {} {} {} by {}",
                s(&e["grant"]["worker"]),
                s(&e["grant"]["resource"]),
                s(&e["grant"]["level"]),
                s(&e["grant"]["by"])
            ),
            "revoked" => format!(
                "credentials revoked {} {} by {} ({})",
                s(&e["worker"]),
                s(&e["resource"]),
                s(&e["by"]),
                s(&e["reason"])
            ),
            other => format!("credentials {other} {e}"),
        },
        "contract" => match e["event"].as_str().unwrap_or("") {
            "offered" => format!(
                "contract {} offered by {} to {} as {} ({})",
                s(&e["contract"]["id"]),
                s(&e["contract"]["by"]),
                s(&e["contract"]["worker"]),
                s(&e["contract"]["title"]),
                s(&e["contract"]["role"])
            ),
            "quoted" => format!(
                "contract {} quoted by {}: {} tokens",
                s(&e["id"]),
                s(&e["worker"]),
                e["terms"]["budget"]
            ),
            "countered" => format!(
                "contract {} countered by {} (round {}): {} tokens",
                s(&e["id"]),
                s(&e["by"]),
                e["round"],
                e["terms"]["budget"]
            ),
            "accepted" => format!(
                "contract {} accepted by {}: {} tokens",
                s(&e["id"]),
                s(&e["by"]),
                e["terms"]["budget"]
            ),
            "added" => format!("contract {} adds {}", s(&e["id"]), s(&e["worker"])),
            other => format!(
                "contract {} {other}: {}",
                s(&e["id"]),
                e["reason"].as_str().unwrap_or("")
            ),
        },
        "scenario" => match e["event"].as_str().unwrap_or("") {
            "hire" => format!(
                "scenario {}: {} ({}) fills hire slot {}{}",
                s(&e["scenario"]),
                s(&e["worker"]),
                s(&e["role"]),
                e["slot"],
                if e["board_line"] == json!(true) {
                    ", may mail the board"
                } else {
                    ""
                }
            ),
            "step" => format!(
                "scenario {}: step {} played: {}",
                s(&e["scenario"]),
                e["index"],
                e["what"]
            ),
            "failed" => format!(
                "scenario {}: step {} failed: {}",
                s(&e["scenario"]),
                e["index"],
                s(&e["error"])
            ),
            other => format!("scenario {other} {e}"),
        },
        "left" => format!(
            "left {}, let go by {} ({}); to {}, returned {}",
            s(&e["worker"]),
            s(&e["by"]),
            s(&e["reason"]),
            s(&e["to"]),
            e["returned"]
        ),
        "funds" => match e["event"].as_str().unwrap_or("") {
            "move" => format!(
                "funds {} → {}: {} ({})",
                s(&e["from"]),
                s(&e["to"]),
                e["tokens"],
                s(&e["reason"])
            ),
            "request" => format!(
                "funds request {} by {}: {}",
                s(&e["id"]),
                s(&e["by"]),
                e["total"]
            ),
            "decided" => format!(
                "funds {} {}{}",
                s(&e["id"]),
                if e["approved"] == json!(true) {
                    "approved"
                } else {
                    "declined"
                },
                e["note"]
                    .as_str()
                    .filter(|n| !n.is_empty())
                    .map(|n| format!(": {n}"))
                    .unwrap_or_default()
            ),
            "exhausted" => format!("funds {} ran out", s(&e["worker"])),
            other => format!("funds {other} {e}"),
        },
        other => format!("{other} {}", e),
    };
    format!("{t} {what}")
}

async fn follow(
    c: &Client,
    path: &str,
    render: impl Fn(&Value) -> String,
    json_out: bool,
) -> Result<()> {
    let mut body = c.stream(path).await?;
    let mut buf = String::new();
    while let Some(frame) = body.frame().await {
        let frame = frame?;
        let Some(data) = frame.data_ref() else {
            continue;
        };
        buf.push_str(&String::from_utf8_lossy(data));
        while let Some(i) = buf.find('\n') {
            let line: String = buf.drain(..=i).collect();
            if let Some(j) = line.trim().strip_prefix("data:") {
                if json_out {
                    println!("{}", j.trim());
                } else if let Ok(v) = serde_json::from_str::<Value>(j.trim()) {
                    let line = render(&v);
                    if !line.is_empty() {
                        println!("{line}");
                    }
                }
            }
        }
    }
    Ok(())
}

fn transcript_line(e: &Value) -> String {
    let t = time(&e["ts"]);
    match e["type"].as_str().unwrap_or("") {
        "wake" => format!("{t} ── wake ──\n{}", s(&e["text"])),
        "assistant" => {
            let parts: Vec<String> = e["message"]["content"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|c| match c["type"].as_str() {
                    Some("text") => s(&c["text"]),
                    Some("tool_use") => format!("→ {} {}", s(&c["name"]), c["input"]),
                    Some("thinking") => "(thinking)".into(),
                    _ => String::new(),
                })
                .filter(|x| !x.is_empty())
                .collect();
            format!("{t} {}", parts.join("\n"))
        }
        "user" => {
            let n = e["message"]["content"]
                .as_array()
                .map(|a| a.len())
                .unwrap_or(0);
            format!("{t} ({n} tool result(s))")
        }
        "result" => format!(
            "{t} ── turn ended: {} ──",
            s(&e["result"]).chars().take(200).collect::<String>()
        ),
        "system" => format!("{t} ({})", s(&e["subtype"])),
        // pi's events.
        "tool_execution_start" => format!("{t} → {} {}", s(&e["toolName"]), e["args"]),
        "tool_execution_end" => format!(
            "{t} ({}tool result)",
            if e["isError"] == json!(true) {
                "failed "
            } else {
                ""
            }
        ),
        "message_end" if e["message"]["role"] == "assistant" => {
            let m = &e["message"];
            let mut parts: Vec<String> = m["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|c| match c["type"].as_str() {
                    Some("text") => Some(s(&c["text"])).filter(|x| !x.trim().is_empty()),
                    Some("thinking") => Some("(thinking)".into()),
                    _ => None,
                })
                .collect();
            if m["stopReason"] == "error" {
                parts.push(format!("(failed: {})", s(&m["errorMessage"])));
            }
            format!("{t} {}", parts.join("\n"))
        }
        "message_start" | "message_end" | "turn_start" | "turn_end" | "agent_start"
        | "agent_end" | "response" => String::new(),
        "agent_settled" => format!("{t} ── turn ended ──"),
        other => format!("{t} {other} {}", e["text"].as_str().unwrap_or("")),
    }
}

pub async fn firm(c: &Client, cmd: FirmCmd, json_out: bool) -> Result<i32> {
    match cmd {
        FirmCmd::Ls => {
            let v = c.json(Method::GET, "/v1/firms", None).await?;
            if json_out {
                print_json(&v);
            } else {
                let rows = v
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|f| {
                        vec![
                            s(&f["name"]),
                            s(&f["workers"]),
                            s(&f["state"]),
                            s(&f["run"]),
                            s(&f["description"]),
                        ]
                    })
                    .collect();
                table(&["firm", "workers", "state", "run", "description"], rows);
            }
        }
        FirmCmd::Scenarios { firm } => {
            let v = c
                .json(Method::GET, &format!("/v1/firms/{firm}/scenarios"), None)
                .await?;
            if json_out {
                print_json(&v);
            } else {
                for (name, sc) in v.as_object().into_iter().flatten() {
                    println!(
                        "{name}: {}{}",
                        s(&sc["description"]),
                        sc["note"]
                            .as_str()
                            .map(|n| format!(" ({n})"))
                            .unwrap_or_default()
                    );
                    for h in sc["hires"].as_array().into_iter().flatten() {
                        println!(
                            "  hire {} x{}{}{}",
                            s(&h["role"]),
                            h["count"],
                            if h["contract"] == json!(true) {
                                " (contractor)"
                            } else {
                                ""
                            },
                            if h["board_line"] == json!(true) {
                                " (reports to the board)"
                            } else {
                                ""
                            }
                        );
                    }
                    for st in sc["script"].as_array().into_iter().flatten() {
                        let when = [("after", &st["after"]), ("spent", &st["spent"])]
                            .iter()
                            .filter(|(_, x)| !x.is_null())
                            .map(|(k, x)| format!("{k} {}", s(x)))
                            .collect::<Vec<_>>()
                            .join(", ");
                        let what = ["kill", "break_cluster", "mail"]
                            .iter()
                            .find(|k| !st[**k].is_null())
                            .copied()
                            .unwrap_or("?");
                        println!("  at {when}: {what}");
                    }
                }
            }
        }
        FirmCmd::Up {
            firm,
            company,
            ceo,
            seed,
            ceo_prompt,
            articles,
            repo,
            security_share,
            max_concurrent,
            daily_turns,
            scenario,
            task,
            attach,
        } => {
            use base64::Engine as _;
            let attachments = attach
                .iter()
                .map(|p| {
                    let data = std::fs::read(p).map_err(|e| anyhow::anyhow!("{}: {e}", p.display()))?;
                    let name = p
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    Ok(json!({ "name": name, "data": base64::engine::general_purpose::STANDARD.encode(data) }))
                })
                .collect::<Result<Vec<_>>>()?;
            eprintln!("zapper: starting firm {firm}…");
            let read = |p: &Option<std::path::PathBuf>| -> Result<Option<String>> {
                match p {
                    None => Ok(None),
                    Some(p) => std::fs::read_to_string(p)
                        .map(Some)
                        .map_err(|e| anyhow::anyhow!("{}: {e}", p.display())),
                }
            };
            let body = json!({
                "company": company, "ceo": ceo, "seed": seed, "ceo_prompt": read(&ceo_prompt)?, "articles": read(&articles)?,
                "security_share": security_share, "max_concurrent": max_concurrent, "daily_turns": daily_turns,
                "scenario": scenario, "task": read(&task)?, "attachments": attachments,
                "repos": (!repo.is_empty()).then_some(repo),
            });
            let v = c
                .json(
                    Method::POST,
                    &format!("/v1/firms/{firm}/up"),
                    Some(body.to_string()),
                )
                .await?;
            eprintln!("zapper: firm {firm} is running (run {}); write to the CEO with `zapper mail send {firm} --to …` or in the UI", s(&v["run"]));
            if json_out {
                print_json(&v);
            }
        }
        FirmCmd::Down { firm } => {
            let v = c
                .json(Method::POST, &format!("/v1/firms/{firm}/down"), None)
                .await?;
            println!(
                "firm {firm} stopped; run {} is in {}",
                s(&v["run"]),
                s(&v["dir"])
            );
        }
        FirmCmd::Pause { firm } => print_json(
            &c.json(
                Method::POST,
                &format!("/v1/firms/{firm}/pause"),
                Some(r#"{"paused": true}"#.into()),
            )
            .await?,
        ),
        FirmCmd::Resume { firm } => print_json(
            &c.json(
                Method::POST,
                &format!("/v1/firms/{firm}/pause"),
                Some(r#"{"paused": false}"#.into()),
            )
            .await?,
        ),
        FirmCmd::Status { firm } => {
            let v = c
                .json(Method::GET, &format!("/v1/firms/{firm}"), None)
                .await?;
            if json_out {
                print_json(&v);
                return Ok(0);
            }
            let run = &v["run"];
            if run.is_null() {
                println!("firm {firm}: not started");
                return Ok(0);
            }
            println!(
                "firm {firm}: run {} {}; turns today {} of {}, tokens {}",
                s(&run["run"]),
                s(&run["state"]),
                run["usage"]["turns"],
                run["budget"]["daily_turns"],
                run["usage"]["tokens"]
            );
            let rows = run["workers"]
                .as_array()
                .unwrap()
                .iter()
                .map(|w| {
                    vec![
                        s(&w["name"]),
                        s(&w["title"]),
                        s(&w["state"]),
                        s(&w["detail"]),
                        s(&w["unread"]),
                        format!("{}/{}", w["owed"], w["waiting"]),
                        s(&w["session"]),
                    ]
                })
                .collect();
            table(
                &[
                    "worker",
                    "title",
                    "state",
                    "doing",
                    "unread",
                    "owes/waits",
                    "session",
                ],
                rows,
            );
            let f = &run["funds"];
            if f["enabled"] == json!(true) {
                println!(
                    "\nbudget: {} tokens (approved {}; seed {} included), spent {}",
                    f["total"], f["approved"], f["seed"], f["spent"]
                );
            }
        }
        FirmCmd::Credentials {
            firm,
            action,
            args,
            level,
            expires,
            note,
        } => {
            let base = format!("/v1/firms/{firm}/credentials");
            let arg = |i: usize, what: &str| {
                args.get(i)
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("give the {what}"))
            };
            match action.as_deref() {
                None => {
                    let v = c.json(Method::GET, &base, None).await?;
                    if json_out {
                        print_json(&v);
                        return Ok(0);
                    }
                    let now = crate::util::now();
                    println!(
                        "{:<12} {:<20} {:<7} {:<10} EXPIRES",
                        "WORKER", "RESOURCE", "LEVEL", "BY"
                    );
                    for g in v["grants"].as_array().into_iter().flatten() {
                        let exp = g["expires"]
                            .as_f64()
                            .map(|e| format!("in {} min", ((e - now) / 60.0).max(0.0) as u64))
                            .unwrap_or_else(|| "while they hold the role".into());
                        println!(
                            "{:<12} {:<20} {:<7} {:<10} {exp}",
                            s(&g["worker"]),
                            s(&g["resource"]),
                            s(&g["level"]),
                            s(&g["by"])
                        );
                    }
                    let open: Vec<&Value> = v["requests"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|q| matches!(q["state"].as_str(), Some("pending" | "escalated")))
                        .collect();
                    if !open.is_empty() {
                        println!(
                            "\n{:<6} {:<12} {:<20} {:<7} {:<10} REASON",
                            "ID", "WORKER", "RESOURCE", "LEVEL", "DECIDER"
                        );
                        for q in open {
                            println!(
                                "{:<6} {:<12} {:<20} {:<7} {:<10} {}",
                                s(&q["id"]),
                                s(&q["worker"]),
                                s(&q["resource"]),
                                s(&q["level"]),
                                q["decider"].as_str().unwrap_or("board"),
                                s(&q["reason"]).chars().take(60).collect::<String>()
                            );
                        }
                    }
                }
                Some("grant") => {
                    let body = json!({ "worker": arg(0, "worker")?, "resource": arg(1, "resource")?, "level": arg(2, "level")?, "expires_in": expires, "note": note });
                    print_json(
                        &c.json(
                            Method::POST,
                            &format!("{base}/grants"),
                            Some(body.to_string()),
                        )
                        .await?,
                    );
                }
                Some(a @ ("approve" | "deny")) => {
                    let body = json!({ "grant": a == "approve", "level": level, "expires_in": expires, "note": note });
                    print_json(
                        &c.json(
                            Method::POST,
                            &format!("{base}/requests/{}", arg(0, "request")?),
                            Some(body.to_string()),
                        )
                        .await?,
                    );
                }
                Some("revoke") => {
                    let reason = if note.is_empty() {
                        "revoked by the board".to_string()
                    } else {
                        note
                    };
                    let q: String = reason
                        .bytes()
                        .map(|b| {
                            if b.is_ascii_alphanumeric() {
                                (b as char).to_string()
                            } else {
                                format!("%{b:02X}")
                            }
                        })
                        .collect();
                    print_json(
                        &c.json(
                            Method::DELETE,
                            &format!(
                                "{base}/grants/{}/{}?reason={q}",
                                arg(0, "worker")?,
                                arg(1, "resource")?
                            ),
                            None,
                        )
                        .await?,
                    );
                }
                Some(other) => bail!("{other}: grant, approve, deny or revoke"),
            }
        }
        FirmCmd::Contracts { firm } => {
            let v = c
                .json(Method::GET, &format!("/v1/firms/{firm}"), None)
                .await?;
            let list = v["run"]["contracts"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            if json_out {
                print_json(&json!(list));
                return Ok(0);
            }
            let tok = |t: &Value| {
                t["budget"]
                    .as_i64()
                    .map(|b| b.to_string())
                    .unwrap_or_else(|| "-".into())
            };
            println!(
                "{:<6} {:<10} {:<10} {:<28} {:<10} {:>10} {:>10} {:>10}  ENDED",
                "ID", "HIRER", "WORKER", "TITLE", "STATE", "OFFERED", "QUOTED", "AGREED"
            );
            for k in &list {
                println!(
                    "{:<6} {:<10} {:<10} {:<28} {:<10} {:>10} {:>10} {:>10}  {}",
                    s(&k["id"]),
                    s(&k["by"]),
                    s(&k["worker"]),
                    s(&k["title"]).chars().take(28).collect::<String>(),
                    s(&k["state"]),
                    tok(&k["offer"]),
                    tok(&k["quote"]),
                    tok(&k["agreed"]),
                    k["ended"].as_str().unwrap_or("")
                );
            }
        }
        FirmCmd::Fund {
            firm,
            worker,
            tokens,
            reason,
        } => {
            let v = c
                .json(
                    Method::POST,
                    &format!("/v1/firms/{firm}/fund"),
                    Some(
                        json!({ "worker": worker, "tokens": tokens, "reason": reason }).to_string(),
                    ),
                )
                .await?;
            if json_out {
                print_json(&v);
            } else {
                eprintln!("zapper: {worker} has {} tokens now", v["their_balance"]);
            }
        }
        FirmCmd::Release {
            firm,
            worker,
            reason,
        } => {
            let v = c
                .json(
                    Method::POST,
                    &format!("/v1/firms/{firm}/release"),
                    Some(json!({ "worker": worker, "reason": reason }).to_string()),
                )
                .await?;
            if json_out {
                print_json(&v);
            } else {
                eprintln!("zapper: {worker} left the firm");
            }
        }
        FirmCmd::Budget {
            firm,
            action,
            request,
            note,
        } => {
            if let Some(a) = action {
                let approve = match a.as_str() {
                    "approve" => true,
                    "decline" => false,
                    _ => bail!("approve or decline"),
                };
                let Some(id) = request else {
                    bail!("which request? e.g. b-1")
                };
                print_json(
                    &c.json(
                        Method::POST,
                        &format!("/v1/firms/{firm}/budget/{id}"),
                        Some(json!({ "approve": approve, "note": note }).to_string()),
                    )
                    .await?,
                );
                return Ok(0);
            }
            let v = c
                .json(Method::GET, &format!("/v1/firms/{firm}"), None)
                .await?;
            let f = &v["run"]["funds"];
            if json_out {
                print_json(f);
                return Ok(0);
            }
            if f["enabled"] != json!(true) {
                println!("firm {firm}: no run budget");
                return Ok(0);
            }
            println!(
                "budget: {} tokens (approved {}; seed {} included), spent {}",
                f["total"], f["approved"], f["seed"], f["spent"]
            );
            let rows = f["requests"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|r| {
                    vec![
                        s(&r["id"]),
                        s(&r["by"]),
                        s(&r["total"]),
                        s(&r["state"]),
                        s(&r["note"]),
                    ]
                })
                .collect();
            table(&["request", "by", "tokens", "state", "note"], rows);
            println!();
            let rows = f["workers"]
                .as_object()
                .into_iter()
                .flatten()
                .map(|(n, w)| {
                    vec![
                        n.clone(),
                        s(&w["balance"]),
                        s(&w["spent"]),
                        s(&w["received"]),
                    ]
                })
                .collect();
            table(&["worker", "balance", "spent", "received"], rows);
        }
        FirmCmd::Runs { firm } => {
            let v = c
                .json(Method::GET, &format!("/v1/firms/{firm}/runs"), None)
                .await?;
            if json_out {
                print_json(&v);
            } else {
                let rows = v
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|r| {
                        vec![
                            s(&r["run"]),
                            s(&r["state"]),
                            format!("{} KiB", r["journal_bytes"].as_u64().unwrap_or(0) / 1024),
                            s(&r["note"])
                                .lines()
                                .next()
                                .unwrap_or("")
                                .chars()
                                .take(60)
                                .collect(),
                        ]
                    })
                    .collect();
                table(&["run", "state", "journal", "note"], rows);
            }
        }
        FirmCmd::Note { firm, run, note } => print_json(
            &c.json(
                Method::PUT,
                &format!("/v1/firms/{firm}/runs/{run}/note"),
                Some(json!({ "note": note }).to_string()),
            )
            .await?,
        ),
        FirmCmd::Prune { firm, run } => print_json(
            &c.json(
                Method::DELETE,
                &format!("/v1/firms/{firm}/runs/{run}"),
                None,
            )
            .await?,
        ),
        FirmCmd::Journal {
            firm,
            run,
            follow: f,
        } => {
            let path = format!("/v1/firms/{firm}/runs/{run}/journal");
            if f {
                follow(c, &format!("{path}?follow=true"), journal_line, json_out).await?;
            } else {
                let text = c.text(&path).await?;
                for l in text.lines() {
                    match (json_out, serde_json::from_str::<Value>(l)) {
                        (false, Ok(v)) => println!("{}", journal_line(&v)),
                        _ => println!("{l}"),
                    }
                }
            }
        }
        FirmCmd::Transcript {
            firm,
            worker,
            run,
            follow: f,
        } => {
            let path = format!("/v1/firms/{firm}/runs/{run}/transcripts/{worker}");
            if f {
                follow(c, &format!("{path}?follow=true"), transcript_line, json_out).await?;
            } else {
                for l in c.text(&path).await?.lines() {
                    match (json_out, serde_json::from_str::<Value>(l)) {
                        (false, Ok(v)) => {
                            let line = transcript_line(&v);
                            if !line.is_empty() {
                                println!("{line}");
                            }
                        }
                        _ => println!("{l}"),
                    }
                }
            }
        }
        FirmCmd::Replay { firm, run } => {
            c.json(Method::GET, &format!("/v1/firms/{firm}/runs"), None)
                .await?;
            let state = super::daemon_state_dir()?;
            let url = std::fs::read_to_string(state.join("ui-url"))
                .context("the daemon's UI address (is the UI enabled?)")?;
            println!("{}#firm={firm}&replay={run}", url.trim());
        }
        FirmCmd::Security { firm } => {
            let v = c
                .json(Method::GET, &format!("/v1/firms/{firm}/security"), None)
                .await?;
            if json_out {
                print_json(&v);
                return Ok(0);
            }
            let rows = v["requests"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| {
                    vec![
                        s(&r["id"]),
                        s(&r["worker"]),
                        format!(
                            "{} {}:{}",
                            r["method"].as_str().unwrap_or("CONNECT"),
                            s(&r["host"]),
                            r["port"]
                        ),
                        s(&r["state"]),
                        s(&r["officer"]),
                        s(&r["decision"]["reason"]),
                    ]
                })
                .collect();
            table(
                &["request", "worker", "target", "state", "officer", "reason"],
                rows,
            );
            println!();
            let rows = v["standing"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| {
                    vec![
                        s(&r["id"]),
                        s(&r["scope"]),
                        format!(
                            "{} {}:{}",
                            s(&r["rule"]["action"]),
                            s(&r["rule"]["host"]),
                            r["rule"]["ports"]
                        ),
                        s(&r["by"]),
                        s(&r["uses"]),
                        s(&r["reason"]),
                    ]
                })
                .collect();
            table(&["rule", "scope", "rule", "by", "uses", "reason"], rows);
            let rows: Vec<Vec<String>> = v["screenings"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|x| {
                    vec![
                        s(&x["id"]),
                        s(&x["worker"]),
                        s(&x["title"]),
                        s(&x["by"]),
                        s(&x["state"]),
                        s(&x["decided_by"]),
                        s(&x["note"]),
                    ]
                })
                .collect();
            if !rows.is_empty() {
                println!();
                table(
                    &["check", "hire", "title", "hired by", "state", "by", "note"],
                    rows,
                );
            }
        }
        FirmCmd::Decide {
            firm,
            request,
            allow,
            deny,
            scope,
            reason,
        } => {
            if allow == deny {
                bail!("give --allow or --deny");
            }
            let body = json!({ "allow": allow, "scope": scope, "reason": reason });
            print_json(
                &c.json(
                    Method::POST,
                    &format!("/v1/firms/{firm}/security/requests/{request}"),
                    Some(body.to_string()),
                )
                .await?,
            );
        }
        FirmCmd::Revoke { firm, rule } => print_json(
            &c.json(
                Method::DELETE,
                &format!("/v1/firms/{firm}/security/rules/{rule}"),
                None,
            )
            .await?,
        ),
        FirmCmd::Screen {
            firm,
            screening,
            approve,
            object,
            note,
        } => {
            if approve == object {
                bail!("give --approve or --object");
            }
            print_json(
                &c.json(
                    Method::POST,
                    &format!("/v1/firms/{firm}/security/screenings/{screening}"),
                    Some(json!({ "clear": approve, "note": note }).to_string()),
                )
                .await?,
            );
        }
    }
    Ok(0)
}

pub async fn mail(c: &Client, cmd: MailCmd, json_out: bool) -> Result<i32> {
    match cmd {
        MailCmd::Send {
            firm,
            to,
            cc,
            subject,
            body,
            kind,
            due,
            reply_to,
        } => {
            let body = match body {
                Some(b) => b,
                None => {
                    use std::io::Read;
                    eprintln!("zapper: reading the body from stdin (end with Ctrl-D)…");
                    let mut b = String::new();
                    std::io::stdin().read_to_string(&mut b)?;
                    b
                }
            };
            let v = json!({ "to": to, "cc": cc, "subject": subject, "body": body, "kind": kind, "due": due, "in_reply_to": reply_to });
            let sent = c
                .json(
                    Method::POST,
                    &format!("/v1/firms/{firm}/mail"),
                    Some(v.to_string()),
                )
                .await?;
            println!("sent {} in thread {}", s(&sent["id"]), s(&sent["thread"]));
        }
        MailCmd::Ls { firm, who, unread } => {
            let mut path = format!("/v1/firms/{firm}/mail?unread={unread}");
            if let Some(w) = who {
                path.push_str(&format!("&for={}", url_encode(&w)));
            }
            let v = c.json(Method::GET, &path, None).await?;
            if json_out {
                print_json(&v);
            } else {
                let rows = v
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|m| {
                        vec![
                            s(&m["id"]),
                            s(&m["thread"]),
                            s(&m["from"]),
                            s(&m["kind"]),
                            if m["unread"] == json!(true) {
                                "•".into()
                            } else {
                                String::new()
                            },
                            s(&m["subject"]),
                            s(&m["preview"]),
                        ]
                    })
                    .collect();
                table(
                    &["id", "thread", "from", "kind", "new", "subject", "preview"],
                    rows,
                );
            }
        }
        MailCmd::Thread { firm, thread } => {
            let v = c
                .json(
                    Method::GET,
                    &format!("/v1/firms/{firm}/threads/{thread}"),
                    None,
                )
                .await?;
            if json_out {
                print_json(&v);
            } else {
                println!(
                    "{} — {}\n",
                    s(&v["thread"]["subject"]),
                    s(&v["thread"]["id"])
                );
                for m in v["messages"].as_array().into_iter().flatten() {
                    println!(
                        "── {} {} → {} [{}]{}\n{}\n",
                        s(&m["id"]),
                        s(&m["from"]),
                        m["to"]
                            .as_array()
                            .map(|a| a.iter().map(s).collect::<Vec<_>>().join(", "))
                            .unwrap_or_default(),
                        s(&m["kind"]),
                        if m["automatic"] == json!(true) {
                            " automatic"
                        } else {
                            ""
                        },
                        s(&m["body"])
                    );
                }
            }
        }
    }
    Ok(0)
}

pub async fn drive(c: &Client, cmd: DriveCmd, json_out: bool) -> Result<i32> {
    match cmd {
        DriveCmd::Ls { firm } => {
            let v = c
                .json(Method::GET, &format!("/v1/firms/{firm}/drive"), None)
                .await?;
            if json_out {
                print_json(&v);
            } else {
                let rows = v
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|a| {
                        vec![
                            format!("{}@v{}", s(&a["id"]), a["latest"]),
                            s(&a["kind"]),
                            s(&a["author"]),
                            s(&a["title"]),
                            s(&a["files"]),
                        ]
                    })
                    .collect();
                table(&["artifact", "kind", "author", "title", "files"], rows);
            }
        }
        DriveCmd::Get {
            firm,
            artifact,
            dir,
            run,
        } => {
            let journal = c
                .text(&format!("/v1/firms/{firm}/runs/{run}/journal"))
                .await?;
            let r = artifact.trim_start_matches("drive:");
            let (id, ver) = match r.split_once('@') {
                Some((i, v)) => (
                    i.to_string(),
                    Some(v.trim_start_matches('v').parse::<u64>()?),
                ),
                None => (r.to_string(), None),
            };
            let published: Vec<Value> = journal
                .lines()
                .filter_map(|l| serde_json::from_str::<Value>(l).ok())
                .filter(|e| e["type"] == "published" && e["artifact"] == json!(id))
                .collect();
            let entry = match ver {
                Some(v) => published.iter().find(|e| e["version"] == json!(v)),
                None => published.last(),
            };
            let Some(entry) = entry else {
                bail!("no artifact {artifact} in run {run}")
            };
            let version = entry["version"].as_u64().unwrap_or(1);
            std::fs::create_dir_all(&dir)?;
            for f in entry["files"].as_array().into_iter().flatten() {
                let path = s(&f["path"]);
                if path.split('/').any(|p| p == ".." || p.is_empty()) {
                    continue;
                }
                let url = format!(
                    "/v1/firms/{firm}/runs/{run}/drive/{id}/{version}/{}",
                    path.split('/')
                        .map(url_encode)
                        .collect::<Vec<_>>()
                        .join("/")
                );
                let (status, bytes) = c.call(Method::GET, &url, None).await?;
                if !status.is_success() {
                    bail!("{path}: {status}");
                }
                let to = dir.join(&path);
                std::fs::create_dir_all(to.parent().unwrap())?;
                std::fs::write(&to, &bytes)?;
            }
            println!(
                "{id}@v{version} \"{}\" → {}",
                s(&entry["title"]),
                dir.display()
            );
            if json_out {
                print_json(entry);
            }
        }
    }
    Ok(0)
}
