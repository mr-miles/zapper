//! Subordinate ids, for running each session's Firecracker as a uid of its
//! own instead of the daemon user's.
//!
//! With ranges for the daemon user in `/etc/subuid` and `/etc/subgid` and
//! the `newuidmap` and `newgidmap` helpers (package `uidmap`), the daemon
//! maps two ids into each VMM's user namespace: 0, the daemon user, who
//! sets the namespaces up and owns the session's files; and 1, a
//! subordinate uid no other session uses, which Firecracker runs as, with
//! no capabilities. A VMM that escaped Firecracker then holds a uid that
//! owns nothing on the host, can't signal or trace the daemon, and can't
//! reach another session's VMM. Without the ranges or the helpers, the VMM
//! keeps the daemon user's uid, as before.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{bail, Context, Result};

/// A range of subordinate ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Range {
    pub start: u32,
    pub count: u32,
}

/// The first range for `user` (by name or numeric id) in a subuid or
/// subgid file's text: `name:start:count` lines.
fn range_in(text: &str, user: &str, id: u32) -> Option<Range> {
    text.lines().find_map(|l| {
        let mut f = l.trim().split(':');
        let (who, start, count) = (f.next()?, f.next()?, f.next()?);
        if who != user && who != id.to_string() {
            return None;
        }
        let r = Range {
            start: start.parse().ok()?,
            count: count.parse().ok()?,
        };
        (r.count > 0).then_some(r)
    })
}

/// What a host offers for mapping: the daemon user's ranges and helpers.
#[derive(Debug, Clone)]
pub struct SubIds {
    pub uid: u32,
    pub gid: u32,
    pub uids: Range,
    pub gids: Range,
    pub newuidmap: PathBuf,
    pub newgidmap: PathBuf,
}

/// Sessions with a subordinate uid at once, at most.
const MAX: u32 = 4096;

fn helper(name: &str) -> Option<PathBuf> {
    crate::util::find_exe(name, &["/usr/bin", "/bin", "/usr/sbin", "/sbin"])
}

fn user_name(uid: u32) -> Option<String> {
    let text = std::fs::read_to_string("/etc/passwd").ok()?;
    text.lines().find_map(|l| {
        let f: Vec<&str> = l.split(':').collect();
        (f.len() > 2 && f[2] == uid.to_string()).then(|| f[0].to_string())
    })
}

/// The daemon user's subordinate ids, or why there are none.
fn detect() -> Result<SubIds, String> {
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let name = user_name(uid).unwrap_or_else(|| uid.to_string());
    let range = |file: &str, id: u32| {
        std::fs::read_to_string(file)
            .ok()
            .and_then(|t| range_in(&t, &name, id))
            .ok_or_else(|| format!("no range for {name} in {file}"))
    };
    let uids = range("/etc/subuid", uid)?;
    let gids = range("/etc/subgid", gid)?;
    let newuidmap = helper("newuidmap").ok_or("no newuidmap (package uidmap)")?;
    let newgidmap = helper("newgidmap").ok_or("no newgidmap (package uidmap)")?;
    Ok(SubIds {
        uid,
        gid,
        uids,
        gids,
        newuidmap,
        newgidmap,
    })
}

/// The detected ids, once per daemon, or why there are none.
pub fn host() -> Result<&'static SubIds, &'static str> {
    static HOST: std::sync::OnceLock<Result<SubIds, String>> = std::sync::OnceLock::new();
    HOST.get_or_init(detect).as_ref().map_err(String::as_str)
}

/// The first and last uid a session's Firecracker can take — the uid range
/// `zapper check` reports, clamped to the sessions cap (`MAX`) — or why the
/// host offers none.
pub fn host_range() -> Result<(u32, u32), String> {
    let ids = host().map_err(str::to_string)?;
    let start = ids.uids.start;
    Ok((start, start + ids.uids.count.min(MAX) - 1))
}

/// Each session's offset into the ranges, while its VM runs.
static LEASES: Mutex<BTreeMap<String, u32>> = Mutex::new(BTreeMap::new());

/// The lowest offset no other session holds: `session`'s until `release`.
pub fn lease(ids: &SubIds, session: &str) -> Result<u32> {
    let mut leases = LEASES.lock().unwrap();
    if let Some(n) = leases.get(session) {
        return Ok(*n);
    }
    let limit = ids.uids.count.min(ids.gids.count).min(MAX);
    let taken: std::collections::BTreeSet<u32> = leases.values().copied().collect();
    let Some(n) = (0..limit).find(|n| !taken.contains(n)) else {
        bail!("all {limit} subordinate ids are in use");
    };
    leases.insert(session.to_string(), n);
    Ok(n)
}

pub fn release(session: &str) {
    LEASES.lock().unwrap().remove(session);
}

/// The `newuidmap` and `newgidmap` arguments for a VMM at `pid` with
/// offset `n`: 0 the daemon user, 1 its own subordinate id.
fn map_args(ids: &SubIds, pid: u32, n: u32) -> [Vec<String>; 2] {
    let pid = pid.to_string();
    [
        vec![
            pid.clone(),
            "0".into(),
            ids.uid.to_string(),
            "1".into(),
            "1".into(),
            (ids.uids.start + n).to_string(),
            "1".into(),
        ],
        vec![
            pid,
            "0".into(),
            ids.gid.to_string(),
            "1".into(),
            "1".into(),
            (ids.gids.start + n).to_string(),
            "1".into(),
        ],
    ]
}

/// Maps the ids into the user namespace of the process `pid`, once it has
/// entered one of its own (it waits for its map: `crate::vmm`).
pub async fn map(ids: &SubIds, pid: u32, n: u32) -> Result<()> {
    let own = std::fs::read_link("/proc/self/ns/user")?;
    let theirs = format!("/proc/{pid}/ns/user");
    let mut entered = false;
    for _ in 0..1000 {
        match std::fs::read_link(&theirs) {
            Ok(ns) if ns != own => {
                entered = true;
                break;
            }
            Ok(_) => {}
            Err(e) => bail!("the VMM exited before its ids were mapped: {e}"),
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    if !entered {
        bail!("the VMM didn't enter its user namespace");
    }
    let [uid_args, gid_args] = map_args(ids, pid, n);
    for (tool, args) in [(&ids.newuidmap, uid_args), (&ids.newgidmap, gid_args)] {
        let out = tokio::process::Command::new(tool)
            .args(&args)
            .output()
            .await
            .with_context(|| format!("running {}", tool.display()))?;
        if !out.status.success() {
            bail!(
                "{} {}: {}",
                tool.display(),
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_by_name_or_id() {
        let text = "root:100000:65536\nmiles:165536:65536\n1000:300000:10\nempty:1:0\n";
        assert_eq!(
            range_in(text, "miles", 1000),
            Some(Range {
                start: 165536,
                count: 65536
            })
        );
        assert_eq!(
            range_in(text, "other", 1000),
            Some(Range {
                start: 300000,
                count: 10
            })
        );
        assert_eq!(range_in(text, "empty", 7), None, "an empty range is none");
        assert_eq!(range_in(text, "nobody", 7), None);
        assert_eq!(range_in("garbage\n:::\n", "x", 1), None);
    }

    fn ids() -> SubIds {
        SubIds {
            uid: 1000,
            gid: 1000,
            uids: Range {
                start: 100000,
                count: 3,
            },
            gids: Range {
                start: 200000,
                count: 2,
            },
            newuidmap: "/usr/bin/newuidmap".into(),
            newgidmap: "/usr/bin/newgidmap".into(),
        }
    }

    /// Each session holds its own offset until released; the smaller range
    /// bounds how many run at once.
    #[test]
    fn each_session_has_its_own_id() {
        let ids = ids();
        let (a, b) = (
            format!("a-{}", std::process::id()),
            format!("b-{}", std::process::id()),
        );
        let na = lease(&ids, &a).unwrap();
        let nb = lease(&ids, &b).unwrap();
        assert_ne!(na, nb);
        assert_eq!(
            lease(&ids, &a).unwrap(),
            na,
            "the same session, the same id"
        );
        let c = format!("c-{}", std::process::id());
        assert!(lease(&ids, &c).is_err(), "two gids: a third is refused");
        release(&a);
        assert_eq!(lease(&ids, &c).unwrap(), na, "a released id is reused");
        release(&b);
        release(&c);
    }

    #[test]
    fn the_daemon_user_is_0_and_the_session_1() {
        let [u, g] = map_args(&ids(), 42, 2);
        assert_eq!(u.join(" "), "42 0 1000 1 1 100002 1");
        assert_eq!(g.join(" "), "42 0 1000 1 1 200002 1");
    }
}
