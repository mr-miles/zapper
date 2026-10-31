//! Tests of a session's lifecycle across modules: made, stopped, started
//! again, listed again after a restart, adopted.

use super::adopt::*;
use super::persist::kept_files;
use super::run::*;
use super::*;
use crate::prelude::*;
use crate::testing::TestCore;
use std::time::Duration;

const CONFIG: &str = "profiles:\n  web: {}\n";

fn persistent(t: &TestCore) -> Arc<Session> {
    let mut new = NewSession::plain("web".into(), t.workdir(), Default::default());
    new.persistent = true;
    t.core.test_session(new, true).unwrap()
}

/// The `disks` events of `s`, oldest first: each `(state, disks, detail)`.
fn disk_events(t: &TestCore, s: &Session) -> Vec<(String, serde_json::Value, String)> {
    t.core
        .recent_events(Some(&s.id))
        .into_iter()
        .filter_map(|e| match e.body {
            EventBody::Disks {
                state,
                disks,
                detail,
            } => Some((state, serde_json::json!(disks), detail.unwrap_or_default())),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_persistent_sessions_disks_stay_and_are_checked_at_its_start() {
    let t = TestCore::new(CONFIG);
    let s = persistent(&t);
    std::fs::create_dir_all(s.dir.vm()).unwrap();
    std::fs::write(s.dir.root_disk(), b"root").unwrap();
    std::fs::write(s.dir.data_disk(0), b"work").unwrap();
    std::fs::write(s.dir.vm().join("v.sock_1000"), b"").unwrap();

    stop(t.core.daemon(), &s, "stopped by operator").await;
    // The disks stay, the sockets go, and the hashes are recorded.
    assert_eq!(s.dir.disks().len(), 2);
    assert!(!s.dir.vm().join("v.sock_1000").exists());
    let events = disk_events(&t, &s);
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].0, "stopped");
    assert_eq!(
        events[0].1["root.ext4"],
        "4813494d137e1631bba301d5acab6e7bb7aa74ce1185d456565ef51d737677b2"
    );

    // Changed on the host while stopped: recorded, and refused.
    std::fs::write(s.dir.root_disk(), b"tampered").unwrap();
    let e = start(t.core.daemon(), &s, false).await.unwrap_err();
    let e = format!("{e:#}");
    assert!(e.contains("root.ext4 changed while stopped"), "{e}");
    assert!(e.contains("--accept-changed-disks"), "{e}");
    let events = disk_events(&t, &s);
    assert_eq!(events.last().unwrap().0, "changed", "{events:?}");
    assert!(matches!(*s.runtime.status().borrow(), Status::Stopped(_)));

    // Accepted: recorded too, then it boots, which, without a VM here,
    // fails past the check and stops it again, its disks hashed again.
    let _ = start(t.core.daemon(), &s, true).await;
    let states: Vec<String> = disk_events(&t, &s).into_iter().map(|e| e.0).collect();
    assert_eq!(
        states,
        ["stopped", "changed", "changed", "accepted", "stopped"]
    );
}

#[tokio::test]
async fn unchanged_disks_pass_and_forgetting_removes_them() {
    let t = TestCore::new(CONFIG);
    let s = persistent(&t);
    std::fs::create_dir_all(s.dir.vm()).unwrap();
    std::fs::write(s.dir.root_disk(), b"root").unwrap();
    stop(t.core.daemon(), &s, "stopped").await;
    // Without a VM here, the boot fails past the check, and stops it.
    let _ = start(t.core.daemon(), &s, false).await;
    let states: Vec<String> = disk_events(&t, &s).into_iter().map(|e| e.0).collect();
    assert_eq!(states, ["stopped", "checked", "stopped"]);
    t.core.forget_session(&s.id).unwrap();
    assert!(!s.dir.vm().exists());
    assert!(s.dir.audit().exists(), "the record stays");
}

#[tokio::test]
async fn a_start_is_admitted_like_a_new_session() {
    let t = TestCore::new(CONFIG);
    let s = persistent(&t);
    stop(t.core.daemon(), &s, "stopped").await;
    if crate::util::host_memory_mib("MemTotal").is_none() {
        return;
    }
    // No host has a millionth of the memory of a 2 GiB VM to give.
    t.core
        .daemon()
        .config
        .write()
        .unwrap()
        .daemon
        .memory_overcommit = Some(1e-6);
    let e = start(t.core.daemon(), &s, false).await.unwrap_err();
    assert!(e.to_string().contains("memory_overcommit"), "{e}");
    assert!(matches!(*s.runtime.status().borrow(), Status::Stopped(_)));
}

#[tokio::test]
async fn only_a_persistent_session_starts_again() {
    let t = TestCore::new(CONFIG);
    let plain = t
        .core
        .test_session(
            NewSession::plain("web".into(), t.workdir(), Default::default()),
            true,
        )
        .unwrap();
    stop(t.core.daemon(), &plain, "stopped").await;
    let e = start(t.core.daemon(), &plain, false).await.unwrap_err();
    assert!(e.to_string().contains("isn't persistent"), "{e}");

    let running = persistent(&t);
    let e = start(t.core.daemon(), &running, false).await.unwrap_err();
    assert!(e.to_string().contains("already running"), "{e}");

    let mut shared = NewSession::plain("web".into(), t.workdir(), Default::default());
    shared.persistent = true;
    shared.shared_layers = vec![(
        zapper_policy::Scope::Role("acme/eng".into()),
        Default::default(),
    )];
    let e = t.core.test_session(shared, true).err().unwrap();
    assert!(e.to_string().contains("can't share policy layers"), "{e}");
}

/// A process standing in for session `id`'s VMM (its command line
/// `__vmm` and the id), once it has exec'd: before, its command line is
/// still this test's.
fn stand_in(id: &str) -> std::process::Child {
    let mut child = std::process::Command::new("sh")
        .args(["-c", "sleep 30", "__vmm", id])
        .spawn()
        .unwrap();
    let cmdline = format!("/proc/{}/cmdline", child.id());
    for _ in 0..500 {
        if std::fs::read(&cmdline).is_ok_and(|c| c.windows(5).any(|w| w == b"__vmm")) {
            return child;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("the stand-in didn't start");
}

/// As a daemon that starts again: the session gone from memory, then
/// what its directory holds read back.
fn restart(t: &TestCore, s: &Session) -> Arc<Session> {
    t.core.daemon().sessions.0.write().unwrap().remove(&s.id);
    restore(t.core.daemon());
    t.core.daemon().sessions.get(&s.id).expect("restored")
}

#[tokio::test]
async fn a_persistent_session_is_listed_again_after_a_restart() {
    let t = TestCore::new(CONFIG);
    let mut new = NewSession::plain("web".into(), t.workdir(), Default::default());
    new.persistent = true;
    new.labels = [("project".to_string(), "x".to_string())].into();
    new.policy.insert(
        serde_yaml::from_str("{ kind: net, action: allow, host: example.com }").unwrap(),
        None,
    );
    let s = t.core.test_session(new, true).unwrap();
    std::fs::create_dir_all(s.dir.vm()).unwrap();
    std::fs::write(s.dir.root_disk(), b"root").unwrap();
    stop(t.core.daemon(), &s, "stopped").await;

    let back = restart(&t, &s);
    assert!(back.persistent);
    assert_eq!(back.labels["project"], "x");
    assert_eq!(back.own_policy().net.len(), 1, "its own rules");
    assert_eq!(
        *back.runtime.status().borrow(),
        Status::Stopped("kept while the daemon restarted".into())
    );
    assert!(back.dir.root_disk().exists(), "its disks");
    // Its disks are checked as before the restart.
    let _ = start(t.core.daemon(), &back, false).await;
    let states: Vec<String> = disk_events(&t, &back).into_iter().map(|e| e.0).collect();
    assert!(states.contains(&"checked".to_string()), "{states:?}");
}

/// What a persistent session keeps to be adopted: its CA, the same after
/// a restart; its synthetic names, given out again as before; its rules
/// as they change. Removing it removes all of it, so no daemon lists it
/// again.
#[tokio::test]
async fn what_a_persistent_session_keeps_is_kept_and_removed_with_it() {
    let t = TestCore::new(CONFIG);
    let s = persistent(&t);
    let a = s.dns.names().address("a.example").unwrap();
    let b = s.dns.names().address("b.example").unwrap();
    s.put_rule(serde_yaml::from_str("{ kind: net, action: allow, host: x.example }").unwrap());
    stop(t.core.daemon(), &s, "stopped").await;
    let key = s.tls.public_key();

    let back = restart(&t, &s);
    assert_eq!(back.dns.names().address("b.example"), Some(b));
    assert_eq!(back.dns.names().address("a.example"), Some(a));
    assert_ne!(
        back.dns.names().address("c.example"),
        Some(b),
        "new names go on after"
    );
    assert_eq!(
        back.own_policy().net.len(),
        1,
        "the rule added after creation"
    );
    // The same CA: its key, so what it signs is trusted as before.
    assert_eq!(back.tls.public_key(), key);
    let kept = std::fs::read_to_string(back.dir.ca()).unwrap();
    assert_eq!(
        std::fs::read_to_string(s.dir.ca()).unwrap(),
        kept,
        "one CA, kept once"
    );
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(back.dir.ca())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );

    // Each of what a daemon adopting it would need, there to be removed.
    crate::util::write_private(&back.dir.resume_token(), b"tok").unwrap();
    crate::util::write_private(&back.dir.vmm(), b"{}").unwrap();
    crate::util::write_private(&back.dir.conversation(), b"{}").unwrap();
    t.core.forget_session(&back.id).unwrap();
    for f in kept_files(&back.dir) {
        assert!(!f.exists(), "{}", f.display());
    }
    // Only its record stays: anything else a persistent session wrote
    // beside it is kept state that `kept_files` doesn't list.
    let record = [
        "audit.jsonl",
        "console.log",
        "driver.jsonl",
        "vm.json",
        "metrics.json",
    ];
    for e in std::fs::read_dir(back.dir.root()).unwrap().flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        assert!(
            record.contains(&name.as_str()) || name.ends_with(".cast"),
            "{name} left after the session was removed"
        );
    }
    assert!(back.dir.audit().exists(), "the record stays");
    restore(t.core.daemon());
    assert!(
        t.core.daemon().sessions.get(&back.id).is_none(),
        "not listed again"
    );
}

/// A persistent session's VMM, left running by an earlier daemon, is
/// adopted: listed as running, its guest's sockets bound again (the
/// control socket waiting for its agent's resumption), its VM watched
/// by pid, so its end stops the session. A process standing in for the
/// VMM: what the check looks at is its command line.
#[tokio::test]
async fn a_running_vm_left_by_an_earlier_daemon_is_adopted() {
    let t = TestCore::new(CONFIG);
    let s = persistent(&t);
    std::fs::create_dir_all(s.dir.vm()).unwrap();
    std::fs::write(s.dir.root_disk(), b"root").unwrap();
    crate::util::write_private(&s.dir.resume_token(), b"tok").unwrap();
    let mut vmm = stand_in(&s.id);
    let record = crate::daemon::vm::Vmm::of(vmm.id(), None).unwrap();
    std::fs::write(s.dir.vmm(), serde_json::to_vec(&record).unwrap()).unwrap();

    let back = restart(&t, &s);
    assert_eq!(*back.runtime.status().borrow(), Status::Running);
    let adopted = t
        .core
        .recent_events(Some(&back.id))
        .into_iter()
        .any(|e| matches!(e.body, EventBody::Session { detail: Some(ref d), .. } if d.starts_with("adopted")));
    assert!(adopted);
    // Its guest's sockets, bound again by the adopting daemon.
    let control = back.dir.vsock_port(zapper_policy::proto::CONTROL_PORT);
    let proxy = back.dir.vsock_port(zapper_policy::proto::PROXY_PORT);
    for _ in 0..100 {
        if control.exists() && proxy.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(control.exists() && proxy.exists());

    // Its VM ends: the session stops, its disks kept and hashed.
    vmm.kill().unwrap();
    vmm.wait().unwrap();
    for _ in 0..100 {
        if !back.is_running() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(matches!(
        *back.runtime.status().borrow(),
        Status::Stopped(_)
    ));
    assert!(back.dir.root_disk().exists());
}

/// A pid in `vmm.json` that isn't that session's VMM (reused since) is
/// neither adopted nor killed.
#[tokio::test]
async fn a_reused_pid_is_left_alone() {
    let t = TestCore::new(CONFIG);
    let s = persistent(&t);
    std::fs::create_dir_all(s.dir.vm()).unwrap();
    stop(t.core.daemon(), &s, "stopped").await;
    let mut other = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let record = crate::daemon::vm::Vmm::of(other.id(), None).unwrap();
    std::fs::write(s.dir.vmm(), serde_json::to_vec(&record).unwrap()).unwrap();
    let back = restart(&t, &s);
    assert!(!back.is_running());
    assert!(other.try_wait().unwrap().is_none(), "still running");
    other.kill().unwrap();
    let _ = other.wait();

    // Its command line, on a pid that started at another time.
    let mut look_alike = stand_in(&s.id);
    let mut record = crate::daemon::vm::Vmm::of(look_alike.id(), None).unwrap();
    record.start += 1;
    std::fs::write(back.dir.vmm(), serde_json::to_vec(&record).unwrap()).unwrap();
    let again = restart(&t, &back);
    assert!(!again.is_running(), "not adopted");
    assert!(look_alike.try_wait().unwrap().is_none(), "not killed");
    look_alike.kill().unwrap();
    let _ = look_alike.wait();
}

/// A persistent session's rules keep their ids over a restart: a
/// client's id (a grant), and the session's own, gaps and all, so a
/// rule is revoked by the id it was given.
#[tokio::test]
async fn rule_ids_are_kept_over_a_restart() {
    let t = TestCore::new(CONFIG);
    let s = persistent(&t);
    let rule = |yaml: &str| serde_yaml::from_str(yaml).unwrap();
    s.put_rule(rule(
        "{ kind: net, id: grant-x, action: allow, host: a.example }",
    ));
    s.add_rule(rule("{ kind: net, action: allow, host: b.example }"), None);
    s.add_rule(rule("{ kind: net, action: allow, host: c.example }"), None);
    s.remove_rule(&format!("s-{}-net-1", s.id)).unwrap();
    stop(t.core.daemon(), &s, "stopped").await;
    let back = restart(&t, &s);
    let mut ids: Vec<String> = back
        .own_policy()
        .rules()
        .iter()
        .filter_map(|r| r.id().map(str::to_string))
        .collect();
    ids.sort();
    assert_eq!(ids, ["grant-x".to_string(), format!("s-{}-net-2", s.id)]);
    assert!(back.remove_rule("grant-x").is_some(), "revoked by its id");
}

/// A session.json that can't be read: the session isn't listed, and a
/// VM it left running is stopped rather than left unwatched.
#[tokio::test]
async fn an_unreadable_session_stops_its_running_vm() {
    let t = TestCore::new(CONFIG);
    let s = persistent(&t);
    std::fs::create_dir_all(s.dir.vm()).unwrap();
    let mut vmm = stand_in(&s.id);
    let record = crate::daemon::vm::Vmm::of(vmm.id(), None).unwrap();
    std::fs::write(s.dir.vmm(), serde_json::to_vec(&record).unwrap()).unwrap();
    std::fs::write(s.dir.saved(), b"{ torn").unwrap();
    t.core.daemon().sessions.0.write().unwrap().remove(&s.id);
    restore(t.core.daemon());
    assert!(t.core.daemon().sessions.get(&s.id).is_none());
    let status = vmm.wait().unwrap();
    assert!(!status.success(), "stopped: {status}");
}

/// Saves at once from many threads leave a whole session.json, with
/// every rule.
#[tokio::test]
async fn saves_at_once_leave_a_whole_file() {
    let t = TestCore::new(CONFIG);
    let s = persistent(&t);
    let threads: Vec<_> = (0..8)
        .map(|i| {
            let s = s.clone();
            std::thread::spawn(move || {
                let rule = format!("{{ kind: net, id: r{i}, action: allow, host: h{i}.example }}");
                s.put_rule(serde_yaml::from_str(&rule).unwrap());
            })
        })
        .collect();
    for th in threads {
        th.join().unwrap();
    }
    let saved: Saved = serde_json::from_slice(&std::fs::read(s.dir.saved()).unwrap()).unwrap();
    assert_eq!(saved.rules.net.len(), 8);
    let left: Vec<_> = std::fs::read_dir(s.dir.root())
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
        .collect();
    assert!(left.is_empty(), "no temporary files left");
}

/// A VMM is known by its command line and by when it started: the
/// session's stand-in is; another session's, a process that isn't a
/// VMM, the same command line on a pid started at another time (reused
/// since), and a pid that doesn't run, aren't.
#[test]
fn a_vmm_is_known_by_its_command_line_and_start() {
    use crate::daemon::vm::Vmm;
    let mut stand_in = stand_in("abc234");
    let vmm = Vmm::of(stand_in.id(), None).unwrap();
    assert!(vmm.runs_for("abc234"));
    assert!(!vmm.runs_for("other1"), "another session's");
    let reused = Vmm {
        start: vmm.start + 1,
        ..vmm.clone()
    };
    assert!(!reused.runs_for("abc234"), "started at another time");
    let me = Vmm::of(std::process::id(), None).unwrap();
    assert!(!me.runs_for("abc234"), "not a VMM");
    assert!(Vmm::of(u32::MAX - 1, None).is_none());
    stand_in.kill().unwrap();
    stand_in.wait().unwrap();
    assert!(!vmm.runs_for("abc234"), "ended");
}

#[tokio::test]
async fn a_session_running_when_the_daemon_ended_is_unverified() {
    let t = TestCore::new(CONFIG);
    let s = persistent(&t);
    std::fs::create_dir_all(s.dir.vm()).unwrap();
    std::fs::write(s.dir.root_disk(), b"root").unwrap();
    stop(t.core.daemon(), &s, "stopped").await;
    // It ran again, and the daemon ended without stopping it: its record
    // shows it running after its last stop.
    use std::io::Write;
    std::fs::OpenOptions::new()
        .append(true)
        .open(s.dir.audit())
        .unwrap()
        .write_all(b"{\"type\":\"session\",\"state\":\"running\"}\n")
        .unwrap();
    std::fs::write(s.dir.root_disk(), b"written while it ran").unwrap();

    let back = restart(&t, &s);
    assert_eq!(
        *back.runtime.status().borrow(),
        Status::Stopped("the daemon ended while it ran; its disks weren't hashed".into())
    );
    let e = start(t.core.daemon(), &back, false).await.unwrap_err();
    assert!(
        format!("{e:#}").contains("it ran after its disks were last hashed"),
        "{e:#}"
    );
}

/// A run's lifetime ends it, and only that run: a session started again
/// since isn't stopped by the earlier run's.
#[tokio::test(start_paused = true)]
async fn a_ttl_stops_its_own_run_only() {
    let t = TestCore::new(CONFIG);
    let s = t
        .core
        .test_session(
            NewSession::plain("web".into(), t.workdir(), Default::default()),
            true,
        )
        .unwrap();
    let run = s.boots.fetch_add(1, Ordering::Relaxed) + 1;
    arm_ttl(t.core.daemon(), &s, run, crate::daemon::now() + 30.0);
    // Started again before it ends: the earlier run's lifetime is no longer
    // this one's.
    s.boots.fetch_add(1, Ordering::Relaxed);
    tokio::time::sleep(Duration::from_secs(31)).await;
    assert!(s.is_running(), "the later run goes on");

    let run = s.boots.load(Ordering::Relaxed);
    arm_ttl(t.core.daemon(), &s, run, crate::daemon::now() + 30.0);
    tokio::time::sleep(Duration::from_secs(31)).await;
    assert_eq!(
        *s.runtime.status().borrow(),
        Status::Stopped("ttl expired".into())
    );
}
