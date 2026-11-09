//! The workers' tools, at the edge's `/mcp`: mail, the desk, the directory,
//! the drive, and the Stop hook's check. The caller is the worker whose
//! sandbox the edge's token names, never what the call says.

use std::io::{Read, Write};

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use zapper_workbench::drive::files::{components, Found};

use crate::mail::{Draft, BOARD};
use crate::{foreman, now, Office, Worker};

/// The MCP server's name, as the workers' harnesses know it.
pub const SERVER: &str = zapper_openshell::OFFICE_MCP;
/// The most a `drive_publish` copies.
const MAX_PUBLISH: u64 = zapper_workbench::drive::MAX_PUT;

pub fn instructions() -> String {
    "The office: mail with the other workers and the board (the person who runs this office), your desk, the directory, and the shared drive.".to_string()
}

fn s(description: &str) -> Value {
    json!({ "type": "string", "description": description })
}

fn tool(name: &str, description: &str, props: Value, required: &[&str]) -> Value {
    json!({ "name": name, "description": description, "inputSchema": { "type": "object", "properties": props, "required": required } })
}

/// The tools, as `tools/list` gives them.
pub fn list() -> Value {
    let to = json!({ "type": "array", "items": { "type": "string" }, "description": "Who: workers' names, board, or a relay's address (wa:…)" });
    let due = s("When an answer is due: in 2h, in 30m (optional). Unanswered, the office reminds them, then tells you.");
    json!([
        tool("mail_send", "Start a new thread.", json!({ "to": to, "subject": s("Subject"), "body": s("The message"), "due": due }), &["to", "body"]),
        tool("mail_reply", "Answer a message, in its thread: to its sender, or to whom you name.", json!({ "message": s("The message's id (m-…)"), "body": s("The answer"), "to": to, "due": due }), &["message", "body"]),
        tool("mail_inbox", "Your mail, newest first.", json!({ "unread": { "type": "boolean", "description": "Only unread" }, "limit": { "type": "integer" } }), &[]),
        tool("mail_read", "One message, in full.", json!({ "message": s("Its id (m-…)") }), &["message"]),
        tool("mail_thread", "A whole thread.", json!({ "thread": s("Its id (t-…)") }), &["thread"]),
        tool("desk", "What you wait for, and what is awaited of you.", json!({}), &[]),
        tool("directory", "Who works here, and what they do.", json!({}), &[]),
        tool("drive_publish", "Publish a file you uploaded to your outbox (PUT /drive/outbox/<you>/<path> at the edge) to the shared drive, where everyone can fetch it.", json!({ "path": s("Its path in your outbox"), "as": s("Its path on the shared drive (default: the same)") }), &["path"]),
        tool("drive_list", "The shared drive's files.", json!({ "path": s("A directory (default: all)") }), &[]),
        tool("drive_fetch", "Where to fetch a file of the shared drive from.", json!({ "path": s("Its path, as drive_list shows it") }), &["path"]),
        tool("instructions", "Your instructions, and who else works here.", json!({}), &[]),
        tool("stop_check", "The Stop hook's check: mail that came during the turn.", json!({}), &[]),
    ])
}

fn opt(args: &Value, k: &str) -> Option<String> {
    args[k]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn req(args: &Value, k: &str) -> Result<String> {
    opt(args, k).ok_or_else(|| anyhow!("{k} is required"))
}

/// A list given as an array or a comma-separated string.
fn names(v: &Value) -> Vec<String> {
    match v {
        Value::Array(a) => a
            .iter()
            .filter_map(|x| x.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        Value::String(s) => s
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        _ => vec![],
    }
}

/// `in 2h`, `2h`, or seconds since the epoch.
fn due(args: &Value) -> Result<Option<f64>> {
    match &args["due"] {
        Value::Null => Ok(None),
        Value::Number(n) => Ok(n.as_f64()),
        Value::String(s) if s.trim().is_empty() => Ok(None),
        Value::String(s) => {
            let span = s.trim().trim_start_matches("in ").trim();
            let d = crate::config::parse_duration(span)?;
            Ok(Some(now() + d.as_secs_f64()))
        }
        _ => bail!("due is like \"in 2h\""),
    }
}

/// Answers `op` for worker `w`.
pub async fn answer(office: &Office, w: &Worker, op: &str, args: Value) -> Result<Value> {
    let me = w.name.as_str();
    let r = match op {
        "tools" => Ok(list()),
        "mail_send" => {
            let d = Draft {
                from: me.into(),
                to: names(&args["to"]),
                subject: opt(&args, "subject").unwrap_or_default(),
                body: req(&args, "body")?,
                in_reply_to: None,
                due: due(&args)?,
            };
            office
                .send(d)
                .map(|s| json!({ "sent": s.id, "thread": s.thread }))
        }
        "mail_reply" => {
            let id = req(&args, "message")?;
            // A reply to one of the post office's own notices answers the
            // message it is about, to that message's sender.
            let (id, from, subject) = {
                let po = office.post.lock().unwrap();
                let mut m = po
                    .meta(&id)
                    .filter(|m| po.can_read(me, &m.thread))
                    .ok_or_else(|| anyhow!("no message {id}"))?;
                if m.from == crate::mail::OFFICE {
                    if let Some(about) = m.in_reply_to.as_ref().and_then(|a| po.meta(a)) {
                        m = about;
                    }
                }
                (m.id.clone(), m.from.clone(), m.subject.clone())
            };
            let mut to = names(&args["to"]);
            if to.is_empty() {
                to = vec![if from == crate::mail::OFFICE {
                    BOARD.into()
                } else {
                    from
                }];
            }
            let subject = if subject.starts_with("Re: ") {
                subject
            } else {
                format!("Re: {subject}")
            };
            let d = Draft {
                from: me.into(),
                to,
                subject: crate::mail::subject_from(&subject),
                body: req(&args, "body")?,
                in_reply_to: Some(id),
                due: due(&args)?,
            };
            office
                .send(d)
                .map(|s| json!({ "sent": s.id, "thread": s.thread }))
        }
        "mail_inbox" => {
            let unread = args["unread"].as_bool().unwrap_or(false);
            let limit = args["limit"].as_u64().unwrap_or(20).clamp(1, 200) as usize;
            let po = office.post.lock().unwrap();
            Ok(json!({ "unread": po.unread(me).len(), "messages": po.inbox(me, unread, limit) }))
        }
        "mail_read" => {
            let id = req(&args, "message")?;
            let mut po = office.post.lock().unwrap();
            let m = po.message(&id, me)?;
            po.mark_read(&office.journal, me, std::slice::from_ref(&id))?;
            w.queue.lock().unwrap().retain(|q| q != &id);
            Ok(m)
        }
        "mail_thread" => {
            let id = req(&args, "thread")?;
            let po = office.post.lock().unwrap();
            po.thread(&id, me)
        }
        "desk" => {
            let po = office.post.lock().unwrap();
            Ok(serde_json::to_value(po.desk(me))?)
        }
        "directory" => {
            let po = office.post.lock().unwrap();
            Ok(po.directory(&office.config.about()))
        }
        "drive_publish" => publish(office, me, &req(&args, "path")?, opt(&args, "as")),
        "drive_list" => drive_list(office, opt(&args, "path").as_deref()),
        "drive_fetch" => {
            let path = req(&args, "path")?;
            let parts = shared(&path)?;
            match office.files.find(&parts)? {
                Found::File { size, .. } => {
                    let url = format!("{}/drive/{}", edge_base(office), parts.join("/"));
                    Ok(
                        json!({ "url": url, "size": size, "how": format!("curl -fsS -o FILE {url}") }),
                    )
                }
                Found::Dir(_) => bail!("{path} is a directory; drive_list shows what is in it"),
            }
        }
        "stop_check" => Ok(foreman::stop_check(office, w)),
        "instructions" => Ok(json!({ "text": office.brief(me) })),
        _ => bail!("no tool {op}"),
    };
    if let Err(e) = &r {
        office.journal.append(
            "op_error",
            json!({ "worker": me, "op": op, "error": format!("{e:#}") }),
        );
    }
    r
}

/// The edge as the workers reach it (without `/mcp`).
fn edge_base(office: &Office) -> String {
    office.config.edge.url.trim_end_matches('/').to_string()
}

/// A path on the shared drive (its `inbox/`), as parts.
fn shared(path: &str) -> Result<Vec<String>> {
    let p = path.trim().trim_start_matches('/');
    let p = p.strip_prefix("drive/").unwrap_or(p);
    let p = p.strip_prefix("inbox/").unwrap_or(p);
    let mut parts = components(p)?;
    parts.insert(0, "inbox".into());
    Ok(parts)
}

/// Copies `path` of `me`'s outbox to the shared drive, under `me`'s name.
fn publish(office: &Office, me: &str, path: &str, to: Option<String>) -> Result<Value> {
    let p = path.trim().trim_start_matches('/');
    let p = p
        .strip_prefix(&format!("drive/outbox/{me}/"))
        .or_else(|| p.strip_prefix("drive/mine/"))
        .or_else(|| p.strip_prefix("mine/"))
        .or_else(|| p.strip_prefix(&format!("outbox/{me}/")))
        .unwrap_or(p);
    let src_rel = components(p)?;
    let mut src = vec!["outbox".to_string(), me.to_string()];
    src.extend(src_rel.iter().cloned());
    let dst_rel = match &to {
        Some(t) => components(t.trim().trim_start_matches('/'))?,
        None => src_rel,
    };
    let mut dst = vec!["inbox".to_string(), me.to_string()];
    dst.extend(dst_rel);
    let (mut file, size) = match office.files.find(&src) {
        Ok(Found::File { file, size }) => (file, size),
        Ok(Found::Dir(_)) => bail!("{path} is a directory: publish its files one by one"),
        Err(_) => bail!(
            "no {path} in your outbox: PUT it to {}/drive/outbox/{me}/{p} first",
            edge_base(office)
        ),
    };
    if size > MAX_PUBLISH {
        bail!("{path} is {size} bytes; at most {MAX_PUBLISH}");
    }
    let mut w = office.files.begin_write(&dst)?;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        w.file().write_all(&buf[..n])?;
    }
    w.finish()?;
    let shared = dst[1..].join("/");
    office.journal.append(
        "published",
        json!({ "worker": me, "path": shared, "bytes": size }),
    );
    Ok(
        json!({ "published": shared, "url": format!("{}/drive/{}", edge_base(office), dst.join("/")) }),
    )
}

fn drive_list(office: &Office, path: Option<&str>) -> Result<Value> {
    let parts = shared(path.unwrap_or(""))?;
    match office.files.find(&parts)? {
        Found::Dir(entries) => Ok(json!({ "path": parts[1..].join("/"), "entries": entries })),
        Found::File { size, .. } => Ok(json!({ "path": parts[1..].join("/"), "size": size })),
    }
}
