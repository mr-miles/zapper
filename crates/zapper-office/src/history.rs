//! Past runs on disk: their list and summaries, notes, configs, pruning, and
//! ending the journals of runs a previous daemon left running.

use std::path::PathBuf;

use anyhow::{bail, Result};
use serde_json::{json, Value};

use crate::journal::Journal;
use crate::{is_run_id, Office};

/// Ends the journals of runs a previous daemon left running: they can't be
/// resumed, and a replay should say how they ended.
pub fn close_orphans(state: &std::path::Path) {
    let Ok(firms) = std::fs::read_dir(state.join("firms")) else {
        return;
    };
    for firm in firms.flatten() {
        let Ok(runs) = std::fs::read_dir(firm.path().join("runs")) else {
            continue;
        };
        for run in runs.flatten() {
            let path = run.path().join(zapper_runs::JOURNAL);
            let Ok(text) = std::fs::read(zapper_core::record::view(&path)) else {
                continue;
            };
            let tail = &text[text.len().saturating_sub(64 * 1024)..];
            let ended = String::from_utf8_lossy(tail)
                .lines()
                .rev()
                .filter_map(|l| serde_json::from_str::<Value>(l).ok())
                .find(|v| v["type"] == "run")
                .is_some_and(|v| v["state"] == "ended" || v["state"] == "failed");
            if !ended {
                if let Ok(j) = Journal::create(&path) {
                    j.append(
                        "run",
                        json!({ "state": "ended", "reason": "the daemon stopped" }),
                    );
                }
            }
        }
    }
}

/// Past runs of a firm, newest first.
pub fn list_runs(office: &Office, firm: &str) -> Vec<Value> {
    let dir = office.firm_dir(firm).join("runs");
    let mut out: Vec<Value> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if !is_run_id(&name) {
                return None;
            }
            let info: Value = std::fs::read(e.path().join(zapper_runs::INFO)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(json!({}));
            let journal = zapper_core::record::view(&e.path().join(zapper_runs::JOURNAL));
            let size = std::fs::metadata(&journal).map(|m| m.len()).unwrap_or(0);
            let sum = zapper_runs::Summary::read(&journal);
            let note = std::fs::read_to_string(e.path().join(RUN_NOTE)).ok();
            Some(json!({
                "run": name, "started": info["started"], "journal_bytes": size, "note": note,
                "kickoff": sum.kickoff.as_ref().map(kickoff), "messages": sum.messages, "last": sum.last, "ended": sum.ended(),
            }))
        })
        .collect();
    out.sort_by(|a, b| b["run"].as_str().cmp(&a["run"].as_str()));
    if let Ok(r) = office.run(firm) {
        for v in out.iter_mut() {
            if v["run"] == json!(r.id) {
                v["state"] = json!(*r.state.lock().unwrap());
            }
        }
    }
    out
}

/// The config a run started with, in its directory.
pub const RUN_CONFIG: &str = "config.yaml";

/// The board's note on a run, in its directory: free text to find the run
/// again later. Not part of the record.
pub const RUN_NOTE: &str = "note.txt";
/// The longest note, in bytes.
const MAX_NOTE: usize = 4000;

/// Sets the board's note on a run; empty removes it.
pub fn set_note(office: &Office, firm: &str, run: &str, note: &str) -> Result<()> {
    let dir = run_dir(office, firm, run)?;
    let note = note.trim();
    if note.len() > MAX_NOTE {
        bail!("keep the note under {MAX_NOTE} bytes");
    }
    let path = dir.join(RUN_NOTE);
    if note.is_empty() {
        match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => return Ok(()),
        }
    }
    let tmp = dir.join(format!("{RUN_NOTE}.tmp"));
    zapper_util::write_private(&tmp, note.as_bytes())?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// A run's config and the skills its workers had: what it was run with.
pub fn run_config(dir: &std::path::Path) -> Value {
    let config = std::fs::read_to_string(dir.join(RUN_CONFIG)).ok();
    // skills/<role>/<skill>/…: SKILL.md as text, other files by name.
    let mut skills = Vec::new();
    let mut roles: Vec<_> = std::fs::read_dir(dir.join("skills"))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_dir())
        .collect();
    roles.sort_by_key(|e| e.file_name());
    for role in roles {
        let mut list: Vec<_> = std::fs::read_dir(role.path())
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().is_dir())
            .collect();
        list.sort_by_key(|e| e.file_name());
        for sk in list {
            let text = std::fs::read(sk.path().join("SKILL.md"))
                .ok()
                .map(|b| String::from_utf8_lossy(&b[..b.len().min(256 << 10)]).into_owned());
            let mut files: Vec<String> = std::fs::read_dir(sk.path())
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n != "SKILL.md")
                .collect();
            files.sort();
            skills.push(json!({ "role": role.file_name().to_string_lossy(), "skill": sk.file_name().to_string_lossy(), "text": text, "files": files }));
        }
    }
    // agents/<role>/AGENT.md: each role's brief, as the preamble had it.
    let mut agents: Vec<Value> = std::fs::read_dir(dir.join("agents"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let text = std::fs::read(e.path().join("AGENT.md")).ok()?;
            Some(json!({ "role": e.file_name().to_string_lossy(), "text": String::from_utf8_lossy(&text[..text.len().min(256 << 10)]) }))
        })
        .collect();
    agents.sort_by(|a, b| a["role"].as_str().cmp(&b["role"].as_str()));
    json!({ "config": config, "skills": skills, "agents": agents })
}

/// The board's first mail, for the list of runs: its subject and a preview,
/// and the whole message for the board to give the next run the same task.
fn kickoff(mail: &Value) -> Value {
    let body = mail["body"].as_str().unwrap_or("");
    let preview: String = body
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(240)
        .collect();
    json!({ "subject": mail["subject"], "preview": preview, "body": body, "to": mail["to"], "ts": mail["ts"] })
}

pub fn run_dir(office: &Office, firm: &str, run: &str) -> Result<PathBuf> {
    if !zapper_policy::valid_worker_name(firm) {
        bail!("{firm:?} is not a firm name");
    }
    let id = if run == "current" {
        office.run(firm)?.id.clone()
    } else {
        run.to_string()
    };
    if !is_run_id(&id) {
        bail!("{run:?} is not a run id");
    }
    let dir = office.firm_dir(firm).join("runs").join(&id);
    if !dir.is_dir() {
        bail!("no run {id} of firm {firm}");
    }
    Ok(dir)
}

pub fn prune(office: &Office, firm: &str, run: &str) -> Result<()> {
    let dir = run_dir(office, firm, run)?;
    if let Ok(r) = office.run(firm) {
        if r.dir == dir && !r.ended() {
            bail!("run {run} is running; stop the firm first");
        }
    }
    std::fs::remove_dir_all(&dir)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn run_summaries() {
        let dir = std::env::temp_dir().join(format!("zapper-runsum-{}", zapper_util::random_id(6)));
        std::fs::create_dir_all(&dir).unwrap();
        let j = dir.join("journal.jsonl");
        std::fs::write(&j, concat!(
            r#"{"seq":1,"ts":10.0,"type":"run","state":"running"}"#, "\n",
            r#"{"seq":2,"ts":11.5,"type":"mail","from":"ceo","to":["ada"],"subject":"x","body":"y"}"#, "\n",
            r#"{"seq":3,"ts":12.0,"type":"mail","from":"board","to":["ceo"],"subject":"Build a todo app","body":"An MVP,\n  please."}"#, "\n",
            r#"{"seq":4,"ts":20.25,"type":"worker","worker":"ceo","state":"working","detail":"thinking about the todo list app"}"#, "\n",
            r#"{"seq":5,"ts":30.0,"type":"run","state":"ended"}"#, "\n",
        )).unwrap();
        let s = zapper_runs::Summary::read(&j);
        assert_eq!(s.messages, 2);
        let k = super::kickoff(s.kickoff.as_ref().unwrap());
        assert_eq!(k["subject"], "Build a todo app");
        assert_eq!(k["preview"], "An MVP, please.");
        // The message as sent, to send again.
        assert_eq!(k["body"], "An MVP,\n  please.");
        assert_eq!(s.last, Some(30.0));
        assert!(s.ended());
        std::fs::write(&j, r#"{"seq":1,"ts":10.0,"type":"run","state":"running"}"#).unwrap();
        assert!(
            zapper_runs::Summary::read(&j).kickoff.is_none(),
            "a run the board never wrote to has no kickoff"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
