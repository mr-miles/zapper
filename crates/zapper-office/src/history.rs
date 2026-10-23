//! Past runs on disk: their list and summaries, notes, configs, pruning, and
//! ending the journals of runs a previous daemon left running.

use std::path::PathBuf;

use anyhow::{bail, Result};
use serde_json::{json, Value};
use zapper_core::Core;

use crate::firm::BOARD;
use crate::journal::Journal;
use crate::{firm_dir, is_run_id, OfficeOf};

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
            let path = run.path().join("journal.jsonl");
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
pub fn list_runs(d: &Core, firm: &str) -> Vec<Value> {
    let dir = firm_dir(d, firm).join("runs");
    let mut out: Vec<Value> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if !is_run_id(&name) {
                return None;
            }
            let info: Value = std::fs::read(e.path().join("run.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(json!({}));
            let journal = zapper_core::record::view(&e.path().join("journal.jsonl"));
            let size = std::fs::metadata(&journal).map(|m| m.len()).unwrap_or(0);
            let sum = run_summary(&journal);
            let note = std::fs::read_to_string(e.path().join(RUN_NOTE)).ok();
            Some(json!({
                "run": name, "started": info["started"], "journal_bytes": size, "note": note,
                "kickoff": sum.kickoff, "messages": sum.messages, "last": sum.last, "ended": sum.ended,
            }))
        })
        .collect();
    out.sort_by(|a, b| b["run"].as_str().cmp(&a["run"].as_str()));
    if let Ok(r) = d.office().run(firm) {
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
pub fn set_note(d: &Core, firm: &str, run: &str, note: &str) -> Result<()> {
    let dir = run_dir(d, firm, run)?;
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
    zapper_core::util::write_private(&tmp, note.as_bytes())?;
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
    json!({ "config": config, "skills": skills })
}

/// What a run's journal says about it, for the list of runs.
struct RunSummary {
    /// The board's first message: what the firm was asked to build. A run
    /// without one was never kicked off.
    kickoff: Option<Value>,
    messages: u64,
    last: Option<f64>,
    ended: bool,
}

fn run_summary(journal: &std::path::Path) -> RunSummary {
    use std::io::BufRead;
    let mut sum = RunSummary {
        kickoff: None,
        messages: 0,
        last: None,
        ended: false,
    };
    let Ok(f) = std::fs::File::open(journal) else {
        return sum;
    };
    for line in std::io::BufReader::new(f)
        .split(b'\n')
        .map_while(Result::ok)
    {
        // Most lines are not needed whole: look before parsing.
        let mail = has(&line, br#""type":"mail""#);
        let run = has(&line, br#""type":"run""#);
        if !mail && !run && sum.last.is_some() && line.len() > 64 {
            if let Some(ts) = ts_of(&line) {
                sum.last = Some(ts);
            }
            continue;
        }
        let Ok(v) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        if let Some(ts) = v["ts"].as_f64() {
            sum.last = Some(ts);
        }
        if mail {
            sum.messages += 1;
            if sum.kickoff.is_none() && v["from"] == BOARD {
                let body = v["body"].as_str().unwrap_or("");
                let preview: String = body
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .chars()
                    .take(240)
                    .collect();
                sum.kickoff = Some(
                    // The whole message too, for the board to give the
                    // next run the same task.
                    json!({ "subject": v["subject"], "preview": preview, "body": body, "to": v["to"], "ts": v["ts"] }),
                );
            }
        } else if run {
            sum.ended = matches!(v["state"].as_str(), Some("ended" | "failed"));
        }
    }
    sum
}

fn has(line: &[u8], pat: &[u8]) -> bool {
    line.windows(pat.len()).any(|w| w == pat)
}

/// `"ts":<number>` from a journal line, without parsing it all.
fn ts_of(line: &[u8]) -> Option<f64> {
    let key = br#""ts":"#;
    let at = line.windows(key.len()).position(|w| w == key)? + key.len();
    let rest = &line[at..];
    let end = rest
        .iter()
        .position(|b| {
            !(b.is_ascii_digit()
                || *b == b'.'
                || *b == b'-'
                || *b == b'e'
                || *b == b'E'
                || *b == b'+')
        })
        .unwrap_or(rest.len());
    std::str::from_utf8(&rest[..end]).ok()?.parse().ok()
}

/// A run's directory; `current` is the firm's current or last run.
pub fn run_dir(d: &Core, firm: &str, run: &str) -> Result<PathBuf> {
    if !zapper_policy::valid_worker_name(firm) {
        bail!("{firm:?} is not a firm name");
    }
    let id = if run == "current" {
        d.office().run(firm)?.id.clone()
    } else {
        run.to_string()
    };
    if !is_run_id(&id) {
        bail!("{run:?} is not a run id");
    }
    let dir = firm_dir(d, firm).join("runs").join(&id);
    if !dir.is_dir() {
        bail!("no run {id} of firm {firm}");
    }
    Ok(dir)
}

pub fn prune(d: &Core, firm: &str, run: &str) -> Result<()> {
    let dir = run_dir(d, firm, run)?;
    if let Ok(r) = d.office().run(firm) {
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
        let dir =
            std::env::temp_dir().join(format!("zapper-runsum-{}", zapper_core::util::random_id(6)));
        std::fs::create_dir_all(&dir).unwrap();
        let j = dir.join("journal.jsonl");
        std::fs::write(&j, concat!(
            r#"{"seq":1,"ts":10.0,"type":"run","state":"running"}"#, "\n",
            r#"{"seq":2,"ts":11.5,"type":"mail","from":"ceo","to":["ada"],"subject":"x","body":"y"}"#, "\n",
            r#"{"seq":3,"ts":12.0,"type":"mail","from":"board","to":["ceo"],"subject":"Build a todo app","body":"An MVP,\n  please."}"#, "\n",
            r#"{"seq":4,"ts":20.25,"type":"worker","worker":"ceo","state":"working","detail":"thinking about the todo list app"}"#, "\n",
            r#"{"seq":5,"ts":30.0,"type":"run","state":"ended"}"#, "\n",
        )).unwrap();
        let s = super::run_summary(&j);
        assert_eq!(s.messages, 2);
        assert_eq!(s.kickoff.as_ref().unwrap()["subject"], "Build a todo app");
        assert_eq!(s.kickoff.as_ref().unwrap()["preview"], "An MVP, please.");
        // The message as sent, to send again.
        assert_eq!(s.kickoff.as_ref().unwrap()["body"], "An MVP,\n  please.");
        assert_eq!(s.last, Some(30.0));
        assert!(s.ended);
        std::fs::write(&j, r#"{"seq":1,"ts":10.0,"type":"run","state":"running"}"#).unwrap();
        assert!(
            super::run_summary(&j).kickoff.is_none(),
            "a run the board never wrote to has no kickoff"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
