//! A persistent session's disks in its record: hashing them, what the record
//! says they were when the session last stopped (what a start checks them
//! against), and the rules `zapper record session` applies to the whole
//! record. The record, not a file beside it, is what's trusted: with a
//! keeper, it is append-only, so the hashes of a stop stay as they were
//! written.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::Result;
use serde_json::{json, Value};

pub(crate) type Hashes = BTreeMap<String, String>;

/// Each disk's SHA-256, by file name.
pub(crate) fn hash(disks: &[PathBuf]) -> Result<Hashes> {
    let mut out = BTreeMap::new();
    for path in disks {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        out.insert(name, crate::util::sha256_file(path)?);
    }
    Ok(out)
}

/// What differs between two sets of disk hashes, in words.
pub(crate) fn changes(before: &Hashes, now: &Hashes) -> String {
    let mut v = Vec::new();
    for (name, hash) in before {
        match now.get(name) {
            Some(h) if h == hash => {}
            Some(_) => v.push(format!("{name} changed while stopped")),
            None => v.push(format!("{name} is gone")),
        }
    }
    for name in now.keys().filter(|n| !before.contains_key(*n)) {
        v.push(format!("{name} is new"));
    }
    v.join("; ")
}

/// What a session's record says its disks were when it last stopped.
#[derive(Debug, PartialEq)]
pub(crate) enum LastStop {
    /// Their hashes, from the last `disks stopped` event.
    Hashed(Hashes),
    /// It ran after that (or never stopped since it was made) without its
    /// disks being hashed: the daemon ended while it ran.
    RanSince,
    /// No stop recorded.
    None,
}

fn str_of<'a>(e: &'a Value, k: &str) -> &'a str {
    e[k].as_str().unwrap_or("")
}

fn hashes_of(e: &Value) -> Hashes {
    e["disks"]
        .as_object()
        .map(|m| {
            m.iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
                .collect()
        })
        .unwrap_or_default()
}

/// The events of a record, a JSON line each; lines that aren't are skipped.
pub(crate) fn events(text: &str) -> Vec<Value> {
    text.lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// From a session's record, oldest first: its disks at its last stop.
pub(crate) fn last_stop(events: &[Value]) -> LastStop {
    let mut last = LastStop::None;
    for e in events {
        match (str_of(e, "type"), str_of(e, "state")) {
            ("disks", "stopped") => last = LastStop::Hashed(hashes_of(e)),
            ("session", "running") => last = LastStop::RanSince,
            _ => {}
        }
    }
    last
}

/// What `zapper record session` finds in a session's record about its
/// disks, oldest first: each `{seq, ts, finding}`. A disk changed while
/// stopped shows here even when the start said `checked` (a daemon that
/// didn't check), as the stop's hashes were recorded before.
pub(crate) fn findings(events: &[Value]) -> Vec<Value> {
    let mut out = Vec::new();
    let mut last = LastStop::None;
    // The event before, for a `disks stopped` event, which follows the
    // session's stop or a failed boot.
    let mut just_stopped = false;
    for e in events {
        let mut note = |what: String| {
            out.push(json!({ "seq": e["seq"], "ts": e["ts"], "finding": what }));
        };
        let (kind, state) = (str_of(e, "type"), str_of(e, "state"));
        match (kind, state) {
            ("session", "stopped" | "failed") => {
                just_stopped = true;
                continue;
            }
            ("session", "running") => last = LastStop::RanSince,
            ("disks", "stopped") => {
                if !just_stopped {
                    note("disks recorded as stopped without the session stopping before".into());
                }
                last = LastStop::Hashed(hashes_of(e));
            }
            ("disks", "checked") => match &last {
                LastStop::Hashed(before) if *before == hashes_of(e) => {}
                LastStop::Hashed(before) => note(format!(
                    "a start checked disks unlike those of the stop before it: {}",
                    changes(before, &hashes_of(e))
                )),
                LastStop::RanSince => {
                    note("a start checked disks that had run since their last recorded stop".into())
                }
                LastStop::None => {
                    note("a start checked disks with no stop recorded before it".into())
                }
            },
            ("disks", "changed" | "unverified" | "accepted") => {
                note(format!("disks {state}: {}", str_of(e, "detail")))
            }
            ("disks", "image") => {
                let d = str_of(e, "detail");
                if !d.ends_with("as it was built") {
                    note(d.to_string());
                }
            }
            _ => {}
        }
        just_stopped = false;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(seq: u64, kind: &str, state: &str, disks: &[(&str, &str)]) -> Value {
        let mut e = json!({ "seq": seq, "ts": seq, "type": kind, "state": state });
        if kind == "disks" {
            e["disks"] = json!(disks
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<Hashes>());
        }
        e
    }

    #[test]
    fn changes_are_named() {
        let m = |p: &[(&str, &str)]| -> Hashes {
            p.iter()
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect()
        };
        assert_eq!(
            changes(
                &m(&[("root.ext4", "a"), ("disk0.ext4", "b")]),
                &m(&[("root.ext4", "x"), ("disk1.ext4", "c")])
            ),
            "disk0.ext4 is gone; root.ext4 changed while stopped; disk1.ext4 is new"
        );
    }

    #[test]
    fn the_last_stop_is_the_records() {
        let run = [
            ev(1, "session", "running", &[]),
            ev(2, "session", "stopped", &[]),
            ev(3, "disks", "stopped", &[("root.ext4", "a")]),
        ];
        assert_eq!(
            last_stop(&run),
            LastStop::Hashed([("root.ext4".to_string(), "a".to_string())].into())
        );
        let mut ran = run.to_vec();
        ran.push(ev(4, "disks", "checked", &[("root.ext4", "a")]));
        ran.push(ev(5, "session", "running", &[]));
        assert_eq!(last_stop(&ran), LastStop::RanSince);
        assert_eq!(last_stop(&[]), LastStop::None);
    }

    #[test]
    fn an_honest_record_has_no_findings() {
        let r = [
            ev(1, "session", "running", &[]),
            ev(2, "session", "stopped", &[]),
            ev(3, "disks", "stopped", &[("root.ext4", "a")]),
            ev(4, "disks", "checked", &[("root.ext4", "a")]),
            ev(5, "session", "running", &[]),
            ev(6, "session", "stopped", &[]),
            ev(7, "disks", "stopped", &[("root.ext4", "b")]),
        ];
        assert_eq!(findings(&r), Vec::<Value>::new());
    }

    /// A disk changed while stopped, and a start that said `checked`
    /// anyway: the record's stop says otherwise.
    #[test]
    fn a_start_unlike_its_stop_is_found() {
        let r = [
            ev(1, "session", "stopped", &[]),
            ev(2, "disks", "stopped", &[("root.ext4", "a")]),
            ev(3, "disks", "checked", &[("root.ext4", "x")]),
        ];
        let f = findings(&r);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0]["seq"], 3);
        assert!(
            f[0]["finding"]
                .as_str()
                .unwrap()
                .contains("root.ext4 changed while stopped"),
            "{f:?}"
        );
    }

    /// A stop's hashes appended later, to cover a change: no stop before it.
    #[test]
    fn a_stop_without_the_session_stopping_is_found() {
        let r = [
            ev(1, "session", "stopped", &[]),
            ev(2, "disks", "stopped", &[("root.ext4", "a")]),
            ev(3, "disks", "stopped", &[("root.ext4", "x")]),
            ev(4, "disks", "checked", &[("root.ext4", "x")]),
        ];
        let f = findings(&r);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0]["finding"]
            .as_str()
            .unwrap()
            .contains("without the session stopping"));
    }

    #[test]
    fn accepted_and_unverified_starts_are_found() {
        let mut changed = ev(3, "disks", "changed", &[("root.ext4", "x")]);
        changed["detail"] = json!("root.ext4 changed while stopped");
        let r = [
            ev(1, "session", "stopped", &[]),
            ev(2, "disks", "stopped", &[("root.ext4", "a")]),
            changed,
            ev(4, "disks", "accepted", &[("root.ext4", "x")]),
        ];
        assert_eq!(findings(&r).len(), 2);
    }
}
