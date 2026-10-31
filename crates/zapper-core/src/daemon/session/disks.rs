//! A persistent session's disks in its record: hashing them, what the record
//! says they were when the session last stopped (what a start checks them
//! against), and the rules `zapper record session` applies to the whole
//! record. The record, not a file beside it, is what's trusted: with a
//! keeper, it is append-only, so the hashes of a stop stay as they were
//! written.

use std::collections::BTreeMap;
use std::path::PathBuf;

use std::sync::Arc;

use anyhow::{bail, Result};
use serde_json::{json, Value};

use super::persist::save;
use super::{Session, SessionDir};
use crate::daemon::{Daemon, EventBody};

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

/// What the record in `dir` says of the session's disks at its last stop
/// (an unreadable record says nothing: no hashes).
pub(super) fn last_stop_in(dir: &SessionDir) -> LastStop {
    let record =
        std::fs::read_to_string(crate::daemon::record::view(&dir.audit())).unwrap_or_default();
    last_stop(&events(&record))
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
    // Since the last `disks stopped`: a boot (`session starting`: made, or
    // started again; an adoption isn't one), and a check of the disks.
    // Hashes appended to cover a change have no boot before them, and a
    // start that skipped the check has no check.
    let mut stopped_once = false;
    let mut booted = false;
    let mut checked = false;
    for e in events {
        let mut note = |what: String| {
            out.push(
                json!({ "epoch": e["epoch"], "seq": e["seq"], "ts": e["ts"], "finding": what }),
            );
        };
        let (kind, state) = (str_of(e, "type"), str_of(e, "state"));
        match (kind, state) {
            ("session", "stopped" | "failed") => {
                just_stopped = true;
                continue;
            }
            ("session", "running") => last = LastStop::RanSince,
            ("session", "starting") => {
                if stopped_once && !checked {
                    note("a start without its disks checked against the stop before it".into());
                }
                booted = true;
            }
            ("disks", "stopped") => {
                if !just_stopped {
                    note("disks recorded as stopped without the session stopping before".into());
                }
                if stopped_once && !booted {
                    note("disks recorded as stopped with no run since the stop before".into());
                }
                last = LastStop::Hashed(hashes_of(e));
                (stopped_once, booted, checked) = (true, false, false);
            }
            ("disks", "checked") => {
                checked = true;
                match &last {
                    LastStop::Hashed(before) if *before == hashes_of(e) => {}
                    LastStop::Hashed(before) => note(format!(
                        "a start checked disks unlike those of the stop before it: {}",
                        changes(before, &hashes_of(e))
                    )),
                    LastStop::RanSince => note(
                        "a start checked disks that had run since their last recorded stop".into(),
                    ),
                    LastStop::None => {
                        note("a start checked disks with no stop recorded before it".into())
                    }
                }
            }
            ("disks", "changed" | "unverified" | "accepted") => {
                checked |= state == "accepted";
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

/// Hashes a stopped session's disks and compares them with what its record
/// says they were when it last stopped (the record, not a file beside it:
/// with a keeper it is append-only); records what it found (`disks`).
pub(super) async fn check_disks(
    daemon: &Arc<Daemon>,
    session: &Arc<Session>,
    accept: bool,
) -> Result<()> {
    let dir = session.dir.clone();
    let now = tokio::task::spawn_blocking(move || hash(&dir.disks())).await??;
    let (state, detail) = match last_stop_in(&session.dir) {
        LastStop::Hashed(before) if before == now => ("checked", None),
        LastStop::Hashed(before) => ("changed", Some(changes(&before, &now))),
        LastStop::RanSince => (
            "unverified",
            Some(
                "it ran after its disks were last hashed, without stopping: the daemon ended while it ran"
                    .to_string(),
            ),
        ),
        LastStop::None => (
            "unverified",
            Some("no hashes from its last stop to compare with".to_string()),
        ),
    };
    daemon.emit(
        Some(session),
        EventBody::Disks {
            state: state.into(),
            disks: now.clone(),
            detail: detail.clone(),
        },
    );
    if let Some(detail) = detail {
        if !accept {
            return Err(crate::errors::conflict(format!(
                    "session {}: {detail}; start it with --accept-changed-disks to boot its disks as they are",
                    session.id
                ),
            ));
        }
        daemon.emit(
            Some(session),
            EventBody::Disks {
                state: "accepted".into(),
                disks: now,
                detail: Some(format!("started as they are: {detail}")),
            },
        );
    }
    Ok(())
}

/// A persistent session's VM is off: its disks stay, the rest of the VMM's
/// directory (sockets) goes, and the disks' hashes are recorded, in its
/// record, to be checked against when it starts again.
pub(super) async fn keep_disks(daemon: &Arc<Daemon>, session: &Arc<Session>) {
    let sd = &session.dir;
    for e in std::fs::read_dir(sd.vm()).into_iter().flatten().flatten() {
        let p = e.path();
        if p.extension().is_some_and(|x| x == "ext4") {
            continue;
        }
        let _ = if p.is_dir() {
            std::fs::remove_dir_all(&p)
        } else {
            std::fs::remove_file(&p)
        };
    }
    if let Err(e) = save(session) {
        tracing::warn!(session = %session.id, "saving the session: {e:#}");
    }
    let kept = sd.disks();
    let hashed = tokio::task::spawn_blocking(move || hash(&kept)).await;
    let (hashes, detail) = match hashed {
        Ok(Ok(h)) => (h, None),
        Ok(Err(e)) => (BTreeMap::new(), Some(format!("hashing: {e:#}"))),
        Err(e) => (BTreeMap::new(), Some(format!("hashing: {e}"))),
    };
    // In the record, which the next start checks the disks against.
    daemon.emit(
        Some(session),
        EventBody::Disks {
            state: "stopped".into(),
            disks: hashes,
            detail,
        },
    );
}

/// Before a persistent session's disk is made from its image: the image's
/// hash, compared with the one recorded when it was built, in a `disks`
/// event (`image`). One changed since is refused unless
/// `accept_changed_image`; one built before images were hashed is recorded
/// as unverified, and used.
pub(super) async fn check_image(
    daemon: &Arc<Daemon>,
    session: &Arc<Session>,
    image: &std::path::Path,
) -> Result<()> {
    let name = session.resolved.engine.image.clone();
    let path = image.to_path_buf();
    let hash = tokio::task::spawn_blocking(move || crate::util::sha256_file(&path)).await??;
    let built = crate::images::built_hash(&daemon.paths.images, &name);
    let (changed, detail) = match &built {
        Some(b) if *b == hash => (false, format!("image {name} as it was built")),
        Some(_) => (true, format!("image {name} changed since it was built")),
        None => (
            false,
            format!("image {name} was built before images were hashed: unverified (rebuild it to have one)"),
        ),
    };
    let accepted = changed && session.accept_changed_image;
    daemon.emit(
        Some(session),
        EventBody::Disks {
            state: "image".into(),
            disks: [(format!("{name}.ext4"), hash)].into(),
            detail: Some(if accepted {
                format!("{detail}; used as it is, as asked")
            } else {
                detail.clone()
            }),
        },
    );
    if changed && !accepted {
        bail!("{detail}; give accept_changed_image (--accept-changed-image) to make the session from it as it is");
    }
    Ok(())
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
            ev(0, "session", "starting", &[]),
            ev(1, "session", "running", &[]),
            ev(2, "session", "stopped", &[]),
            ev(3, "disks", "stopped", &[("root.ext4", "a")]),
            ev(4, "disks", "checked", &[("root.ext4", "a")]),
            ev(5, "session", "starting", &[]),
            ev(6, "session", "running", &[]),
            ev(7, "session", "stopped", &[]),
            ev(8, "disks", "stopped", &[("root.ext4", "b")]),
            // Adopted after a daemon restart: not a boot, nothing to check.
            ev(9, "disks", "checked", &[("root.ext4", "b")]),
            ev(10, "session", "starting", &[]),
            ev(11, "session", "running", &[]),
            ev(12, "session", "running", &[]),
        ];
        assert_eq!(findings(&r), Vec::<Value>::new());
    }

    /// Two appended lines, a session stop then a disks stop with new hashes,
    /// covering a disk changed while stopped: there was no run since the
    /// stop before.
    #[test]
    fn a_stop_appended_to_cover_a_change_is_found() {
        let r = [
            ev(0, "session", "starting", &[]),
            ev(1, "session", "running", &[]),
            ev(2, "session", "stopped", &[]),
            ev(3, "disks", "stopped", &[("root.ext4", "a")]),
            ev(4, "session", "stopped", &[]),
            ev(5, "disks", "stopped", &[("root.ext4", "x")]),
            ev(6, "disks", "checked", &[("root.ext4", "x")]),
        ];
        let f = findings(&r);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0]["seq"], 5);
        assert!(f[0]["finding"].as_str().unwrap().contains("no run since"));
    }

    /// A finding names the daemon run of the line it is about, as well as
    /// its `seq`, which counts again in each run.
    #[test]
    fn a_finding_names_its_lines_run() {
        let mut r = vec![
            ev(1, "session", "stopped", &[]),
            ev(2, "disks", "stopped", &[("root.ext4", "a")]),
            ev(1, "disks", "stopped", &[("root.ext4", "x")]),
        ];
        r[2]["epoch"] = json!("run2");
        let f = findings(&r);
        assert_eq!(f.len(), 2, "{f:?}");
        assert_eq!((&f[1]["epoch"], &f[1]["seq"]), (&json!("run2"), &json!(1)));
    }

    /// A start whose disks were never checked against the stop before it.
    #[test]
    fn a_start_without_a_check_is_found() {
        let r = [
            ev(0, "session", "starting", &[]),
            ev(1, "session", "stopped", &[]),
            ev(2, "disks", "stopped", &[("root.ext4", "a")]),
            ev(3, "session", "starting", &[]),
            ev(4, "session", "running", &[]),
        ];
        let f = findings(&r);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0]["seq"], 3);
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
        // Found by both rules: no session stop before it, no run since.
        let f = findings(&r);
        assert!(f.iter().all(|x| x["seq"] == 3), "{f:?}");
        assert!(f.iter().any(|x| x["finding"]
            .as_str()
            .unwrap()
            .contains("without the session stopping")));
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

#[cfg(test)]
mod image_tests {
    use super::*;
    use crate::daemon::session::NewSession;
    use crate::testing::TestCore;

    /// An image `web` in the images directory, built (its hash recorded)
    /// with `built`, then holding `now`.
    fn image(t: &TestCore, built: Option<&[u8]>, now: &[u8]) -> std::path::PathBuf {
        let dir = &t.core.daemon().paths.images;
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join("web.ext4");
        if let Some(b) = built {
            std::fs::write(&path, b).unwrap();
            let hash = crate::util::sha256_file(&path).unwrap();
            std::fs::write(
                dir.join("web.json"),
                serde_json::json!({ "sha256": hash }).to_string(),
            )
            .unwrap();
        }
        std::fs::write(&path, now).unwrap();
        path
    }

    fn session(t: &TestCore, accept: bool) -> Arc<Session> {
        let mut new = NewSession::plain("web".into(), t.workdir(), Default::default());
        new.persistent = true;
        new.accept_changed_image = accept;
        t.core.test_session(new, true).unwrap()
    }

    fn image_event(t: &TestCore, s: &Session) -> String {
        t.core
            .daemon()
            .recent_events(Some(&s.id))
            .into_iter()
            .rev()
            .find_map(|e| match e.body {
                EventBody::Disks { state, detail, .. } if state == "image" => detail,
                _ => None,
            })
            .unwrap_or_default()
    }

    const CONFIG: &str = "profiles:\n  web: { engine: { image: web } }\n";

    #[tokio::test]
    async fn a_persistent_sessions_image_is_checked_against_its_build() {
        let t = TestCore::new(CONFIG);
        let path = image(&t, Some(b"as built"), b"as built");
        let s = session(&t, false);
        check_image(t.core.daemon(), &s, &path).await.unwrap();
        assert_eq!(image_event(&t, &s), "image web as it was built");

        let path = image(&t, Some(b"as built"), b"changed");
        let e = check_image(t.core.daemon(), &s, &path).await.unwrap_err();
        assert!(
            e.to_string()
                .contains("image web changed since it was built"),
            "{e}"
        );
        assert_eq!(image_event(&t, &s), "image web changed since it was built");

        let accepting = session(&t, true);
        check_image(t.core.daemon(), &accepting, &path)
            .await
            .unwrap();
        assert!(image_event(&t, &accepting).ends_with("used as it is, as asked"));
    }

    #[tokio::test]
    async fn an_image_built_before_hashing_is_unverified_and_used() {
        let t = TestCore::new(CONFIG);
        let path = image(&t, None, b"old");
        let s = session(&t, false);
        check_image(t.core.daemon(), &s, &path).await.unwrap();
        assert!(
            image_event(&t, &s).contains("unverified"),
            "{}",
            image_event(&t, &s)
        );
    }
}
