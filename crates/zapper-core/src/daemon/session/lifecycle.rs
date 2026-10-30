//! A session's life: resolving and registering it, then stopping and tearing
//! it down, and, for a persistent one, starting it again from its kept disks,
//! checked against the hashes recorded when it stopped. Booting its VM and
//! running its guest control channel is `boot`.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use zapper_policy::proto::HostMsg;

use super::{disks, Counters, GuestRuntime, NewSession, Rules, Saved, Session, SessionDir, Status};
use crate::daemon::{dns, driver, harness, now, random_id, record, tls, Daemon, EventBody};

/// Creates a session without booting it: its profile resolved, its
/// directory and audit log, its rules; listed, as starting.
pub(crate) fn register(daemon: &Arc<Daemon>, new: &NewSession) -> Result<Arc<Session>> {
    let (mut resolved, max, overcommit) = {
        let c = daemon.config.read().unwrap();
        (
            c.resolve(&new.profile)?,
            c.daemon.max_sessions,
            c.daemon.memory_overcommit,
        )
    };
    if let Some(overlay) = &new.agent_overlay {
        let mut agent = serde_json::to_value(&resolved.agent)?;
        zapper_policy::merge(&mut agent, overlay);
        resolved.agent = serde_json::from_value(agent).context("agent overlay")?;
    }
    if new.persistent && !new.shared_layers.is_empty() {
        bail!("a persistent session can't share policy layers with other sessions");
    }
    resolved.mounts.extend(new.mounts.iter().cloned());
    // The agent's skills and configuration directories, where its harness
    // reads them.
    if let Some(h) = harness::get(&resolved.agent.kind) {
        let home = format!("/home/{}", resolved.agent.user);
        let extra = h.mounts(&resolved.agent, &home);
        resolved.mounts.extend(extra);
    }
    let running = daemon.sessions.running();
    if running >= max {
        bail!("{running} sessions running; daemon.max_sessions is {max}");
    }
    let memory_warning = memory_admission(
        &Memory {
            given: daemon.sessions.memory_mib(),
            want: resolved.engine.memory_mib as u64,
            total: crate::util::host_memory_mib("MemTotal"),
            available: crate::util::host_memory_mib("MemAvailable"),
        },
        overcommit,
    )?;
    if let Some(w) = &memory_warning {
        tracing::warn!("profile {}: {w}", new.profile);
    }
    let id = random_id(6);
    let dir = SessionDir::new(daemon.paths.state.join("sessions").join(&id));
    std::fs::create_dir_all(dir.root())?;
    let audit = record::Writer::open(&dir.audit())?;
    // A firm's workers are driven by the office's foreman.
    let driven = resolved.agent.driven
        && !new.labels.contains_key("firm")
        && daemon.config.read().unwrap().daemon.driver;
    let saved = Saved {
        id,
        profile: new.profile.clone(),
        created: now(),
        workdir: new.workdir.clone(),
        resolved,
        labels: new.labels.clone(),
        rules: new.policy.clone(),
        files: new.files.clone(),
        forwards: new.forwards.clone(),
        driven,
    };
    let session = assemble(
        daemon,
        saved,
        new.shared_layers.clone(),
        new.persistent,
        new.accept_changed_image,
        Some(audit),
    )?;
    if session.persistent {
        save(&session)?;
    }
    daemon.sessions.insert(session.clone());
    daemon.emit(
        Some(&session),
        EventBody::Session {
            state: "starting".into(),
            detail: Some(match &memory_warning {
                Some(w) => format!("profile {}; {w}", new.profile),
                None => format!("profile {}", new.profile),
            }),
        },
    );
    Ok(session)
}

/// The host's memory against a new VM's, in MiB. `total` and `available`
/// are unknown when `/proc/meminfo` can't be read.
struct Memory {
    /// Given to the VMs starting or running.
    given: u64,
    /// The new VM's.
    want: u64,
    total: Option<u64>,
    available: Option<u64>,
}

/// Whether a new VM may start: refused past `daemon.memory_overcommit`
/// times the host's memory; a warning when the VMs would be given more
/// than the host has, or the host has less available than the VM's size.
/// Firecracker takes a guest's memory as the guest touches it, so a host
/// can run more than fits on paper, until the guests fill theirs.
fn memory_admission(m: &Memory, overcommit: Option<f64>) -> Result<Option<String>> {
    let Some(total) = m.total else {
        return Ok(None);
    };
    let given = m.given + m.want;
    if let Some(ratio) = overcommit {
        let limit = (total as f64 * ratio) as u64;
        if given > limit {
            bail!(
                "the VMs would be given {given} MiB of memory ({} MiB for this one), \
                 past daemon.memory_overcommit: {ratio} of the host's {total} MiB ({limit} MiB)",
                m.want
            );
        }
    }
    if given > total {
        return Ok(Some(format!(
            "the VMs are given {given} MiB of memory and the host has {total} MiB"
        )));
    }
    match m.available {
        Some(avail) if m.want > avail => Ok(Some(format!(
            "this VM is given {} MiB of memory and the host has {avail} MiB available",
            m.want
        ))),
        _ => Ok(None),
    }
}

pub(crate) async fn create(daemon: Arc<Daemon>, new: NewSession) -> Result<Arc<Session>> {
    let session = register(&daemon, &new)?;
    boot(&daemon, &session).await?;
    Ok(session)
}

/// The session `saved` describes, not yet listed: its rules, a CA of its
/// own, the driver if it is driven, and its lifetime from now.
fn assemble(
    daemon: &Arc<Daemon>,
    saved: Saved,
    shared_layers: Vec<(
        zapper_policy::Scope,
        Arc<std::sync::RwLock<zapper_policy::Policy>>,
    )>,
    persistent: bool,
    accept_changed_image: bool,
    audit: Option<record::Writer>,
) -> Result<Arc<Session>> {
    let dir = SessionDir::new(daemon.paths.state.join("sessions").join(&saved.id));
    let ttl = saved.resolved.engine.ttl_secs;
    // A persistent session's CA and synthetic names are kept, to be the same
    // after a daemon restart.
    let (tls, dns) = if persistent {
        (
            tls::SessionTls::kept(&saved.id, &dir.ca())?,
            dns::SessionDns::kept(&dir.dns()),
        )
    } else {
        (tls::SessionTls::new(&saved.id)?, dns::SessionDns::new())
    };
    let session = Arc::new(Session {
        tls,
        id: saved.id,
        profile: saved.profile,
        created: saved.created,
        expires: Mutex::new((ttl > 0).then_some(now() + ttl as f64)),
        persistent,
        boots: AtomicU64::new(0),
        accept_changed_image,
        workdir: saved.workdir,
        rules: Rules::new(shared_layers),
        secrets: Default::default(),
        runtime: GuestRuntime::new(),
        dir,
        resolved: saved.resolved,
        counters: Counters::default(),
        usage: Mutex::new(None),
        dns,
        terminals: Mutex::new(Default::default()),
        requests: Default::default(),
        audit: Mutex::new(audit),
        labels: saved.labels,
        driver: saved.driven.then(driver::Driver::new),
        daemon: Arc::downgrade(daemon),
        extra_files: saved.files,
        forwards: saved.forwards,
    });
    for rule in saved.rules.rules() {
        session.add_rule(rule, None);
    }
    Ok(session)
}

/// Writes a persistent session's `session.json`, its own rules as they are.
pub(super) fn save(session: &Session) -> Result<()> {
    let saved = Saved {
        id: session.id.clone(),
        profile: session.profile.clone(),
        created: session.created,
        workdir: session.workdir.clone(),
        resolved: session.resolved.clone(),
        labels: session.labels.clone(),
        rules: session.own_policy(),
        files: session.extra_files.clone(),
        forwards: session.forwards.clone(),
        driven: session.driver.is_some(),
    };
    crate::util::write_private(&session.dir.saved(), &serde_json::to_vec_pretty(&saved)?)
}

/// At the daemon's start: lists the persistent sessions an earlier daemon
/// left, stopped, to be started again. One whose VM was running when that
/// daemon ended had its disks changed without their hashes recorded, which
/// its record shows, so its start says its disks are unverified.
pub(crate) fn restore(daemon: &Arc<Daemon>) {
    let Ok(dirs) = std::fs::read_dir(daemon.paths.state.join("sessions")) else {
        return;
    };
    for entry in dirs.flatten() {
        let dir = SessionDir::new(entry.path());
        let Some(saved) = std::fs::read(dir.saved())
            .ok()
            .and_then(|b| serde_json::from_slice::<Saved>(&b).ok())
        else {
            continue;
        };
        // Its VMM, still running, which an earlier daemon left: stopped, as
        // this daemon doesn't adopt it, so a start can't boot its disks
        // twice.
        let left_running = stop_left_vmm(&dir, &saved.id);
        let record =
            std::fs::read_to_string(crate::daemon::record::view(&dir.audit())).unwrap_or_default();
        let was_running = disks::last_stop(&disks::events(&record)) == disks::LastStop::RanSince;
        let session = match assemble(daemon, saved, vec![], true, false, None) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("restoring session in {}: {e:#}", dir.root().display());
                continue;
            }
        };
        let why = if left_running {
            "it ran on after the daemon ended, and was stopped; its disks weren't hashed"
        } else if was_running {
            "the daemon ended while it ran; its disks weren't hashed"
        } else {
            "kept while the daemon restarted"
        };
        session
            .runtime
            .status()
            .send_replace(Status::Stopped(why.into()));
        daemon.sessions.insert(session);
    }
}

/// Stops the durable VMM of the session `id` in `dir` if it still runs, after
/// checking the pid is that VMM's (not a pid reused since); whether one was.
/// Its Firecracker dies with it.
fn stop_left_vmm(dir: &SessionDir, id: &str) -> bool {
    let Some(vmm) = std::fs::read(dir.vmm())
        .ok()
        .and_then(|b| serde_json::from_slice::<crate::daemon::vm::Vmm>(&b).ok())
    else {
        return false;
    };
    let _ = std::fs::remove_file(dir.vmm());
    if !is_vmm_of(vmm.pid, id) {
        return false;
    }
    unsafe { libc::kill(vmm.pid as i32, libc::SIGKILL) };
    for _ in 0..50 {
        if !is_vmm_of(vmm.pid, id) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    true
}

/// Whether `pid` runs the VMM of session `id`: `zapper __vmm`, with the id
/// among its arguments.
pub(crate) fn is_vmm_of(pid: u32, id: &str) -> bool {
    let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
        return false;
    };
    let args: Vec<&[u8]> = cmdline.split(|b| *b == 0).collect();
    args.contains(&&b"__vmm"[..]) && args.contains(&id.as_bytes())
}

/// Boots a registered (or stopped, persistent) session's VM and waits for its
/// guest: running, with its driver and its `ttl_secs` for this run.
async fn boot(daemon: &Arc<Daemon>, session: &Arc<Session>) -> Result<()> {
    let daemon = daemon.clone();
    let session = session.clone();
    let id = session.id.clone();
    let ttl = session.resolved.engine.ttl_secs;
    let run = session.boots.fetch_add(1, Ordering::Relaxed) + 1;

    let started = Instant::now();
    if let Err(e) = super::boot::launch(&daemon, &session).await {
        let msg = format!("{e:#}");
        session
            .runtime
            .status()
            .send_replace(Status::Failed(msg.clone()));
        daemon.emit(
            Some(&session),
            EventBody::Session {
                state: "failed".into(),
                detail: Some(msg.clone()),
            },
        );
        teardown(&daemon, &session, None).await;
        return Err(anyhow!(msg));
    }
    let mut status = session.runtime.status().subscribe();
    let ready = tokio::time::timeout(
        Duration::from_secs(120),
        status.wait_for(|s| !matches!(s, Status::Starting)),
    )
    .await
    .is_ok();
    let state = session.runtime.status().borrow().clone();
    match (ready, state) {
        (true, Status::Running) => {
            let ms = started.elapsed().as_millis();
            daemon.emit(
                Some(&session),
                EventBody::Session {
                    state: "running".into(),
                    detail: Some(format!("ready in {ms} ms")),
                },
            );
            driver::drive(&daemon, &session);
        }
        (_, state) => {
            let msg = match state {
                Status::Failed(m) => m,
                _ => "guest did not become ready in 120s".into(),
            };
            stop(&daemon, &session, &msg).await;
            return Err(anyhow!(
                "session {id}: {msg}; see {}",
                session.dir.console().display()
            ));
        }
    }
    if ttl > 0 {
        let (d, s) = (daemon.clone(), session.clone());
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(ttl)).await;
            // Only this run's: a session stopped and started again since
            // has a lifetime of its own.
            if s.boots.load(Ordering::Relaxed) == run {
                stop(&d, &s, "ttl expired").await;
            }
        });
    }
    Ok(())
}

/// Starts a stopped persistent session again: its kept disks, checked against
/// the hashes recorded when it stopped, boot afresh. A disk that changed
/// while it was stopped is recorded, and the start refused unless
/// `accept_changed`, which is recorded too.
pub(crate) async fn start(
    daemon: &Arc<Daemon>,
    session: &Arc<Session>,
    accept_changed: bool,
) -> Result<()> {
    let id = &session.id;
    if !session.persistent {
        bail!("session {id} isn't persistent: its disks went when it stopped");
    }
    if matches!(
        *session.runtime.status().borrow(),
        Status::Starting | Status::Running
    ) {
        bail!("session {id} is already running");
    }
    let max = daemon.config.read().unwrap().daemon.max_sessions;
    let running = daemon.sessions.running();
    if running >= max {
        bail!("{running} sessions running; daemon.max_sessions is {max}");
    }
    *session.audit.lock().unwrap() = Some(record::Writer::open(&session.dir.audit())?);
    if let Err(e) = check_disks(daemon, session, accept_changed).await {
        *session.audit.lock().unwrap() = None;
        return Err(e);
    }
    let ttl = session.resolved.engine.ttl_secs;
    *session.expires.lock().unwrap() = (ttl > 0).then(|| now() + ttl as f64);
    session.runtime.status().send_replace(Status::Starting);
    daemon.emit(
        Some(session),
        EventBody::Session {
            state: "starting".into(),
            detail: Some("started again, from its kept disks".into()),
        },
    );
    if let Some(driver) = &session.driver {
        driver.revive();
    }
    boot(daemon, session).await
}

/// Hashes a stopped session's disks and compares them with what its record
/// says they were when it last stopped (the record, not a file beside it:
/// with a keeper it is append-only); records what it found (`disks`).
async fn check_disks(daemon: &Arc<Daemon>, session: &Arc<Session>, accept: bool) -> Result<()> {
    let dir = session.dir.clone();
    let now = tokio::task::spawn_blocking(move || disks::hash(&dir.disks())).await??;
    let record = std::fs::read_to_string(session.audit_path()).unwrap_or_default();
    let (state, detail) = match disks::last_stop(&disks::events(&record)) {
        disks::LastStop::Hashed(before) if before == now => ("checked", None),
        disks::LastStop::Hashed(before) => ("changed", Some(disks::changes(&before, &now))),
        disks::LastStop::RanSince => (
            "unverified",
            Some(
                "it ran after its disks were last hashed, without stopping: the daemon ended while it ran"
                    .to_string(),
            ),
        ),
        disks::LastStop::None => (
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
            bail!(
                "session {}: {detail}; start it with --accept-changed-disks to boot its disks as they are",
                session.id
            );
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

/// Stops a session's VM and removes its disks, or, for a persistent session,
/// keeps them and records their hashes; the audit log, console log and
/// recordings stay.
///
/// Status invariant: `Failed` is terminal and never overwritten with
/// `Stopped`; an already-`Stopped` session returns early. `teardown` is
/// idempotent and also reached from the launch-failure path (which sets
/// `Failed` directly, without sending `Shutdown`, since the guest never came
/// up).
pub(crate) async fn stop(daemon: &Arc<Daemon>, session: &Arc<Session>, reason: &str) {
    {
        let current = session.runtime.status().borrow().clone();
        if matches!(current, Status::Stopped(_)) {
            return;
        }
        if !matches!(current, Status::Failed(_)) {
            session
                .runtime
                .status()
                .send_replace(Status::Stopped(reason.into()));
        }
    }
    session.send(HostMsg::Shutdown);
    teardown(daemon, session, Some(reason)).await;
}

/// A persistent session's VM is off: its disks stay, the rest of the VMM's
/// directory (sockets) goes, and the disks' hashes are recorded, in its
/// record, to be checked against when it starts again.
async fn keep_disks(daemon: &Arc<Daemon>, session: &Arc<Session>) {
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
    let hashed = tokio::task::spawn_blocking(move || disks::hash(&kept)).await;
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

async fn teardown(daemon: &Arc<Daemon>, session: &Arc<Session>, reason: Option<&str>) {
    if let Some(driver) = &session.driver {
        driver.stop();
    }
    if let Some(mut child) = session.runtime.vm().lock().await.take() {
        if tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .is_err()
        {
            let _ = child.kill().await;
        }
    }
    crate::subids::release(&session.id);
    session.runtime.clear_guest();
    if let Some(reason) = reason {
        daemon.emit(
            Some(session),
            EventBody::Session {
                state: "stopped".into(),
                detail: Some(reason.into()),
            },
        );
    }
    // The VMM's directory: disks and sockets go; its config and metrics are
    // kept next to the audit log.
    let sd = &session.dir;
    let _ = std::fs::rename(sd.vm_config(), sd.root().join("vm.json"));
    let _ = std::fs::rename(sd.metrics(), sd.root().join("metrics.json"));
    if session.persistent {
        keep_disks(daemon, session).await;
    } else {
        let _ = std::fs::remove_dir_all(sd.vm());
    }
    *session.audit.lock().unwrap() = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prelude::*;
    use crate::testing::TestCore;

    const CONFIG: &str = "profiles:\n  web: {}\n";

    fn host(given: u64, want: u64) -> Memory {
        Memory {
            given,
            want,
            total: Some(16384),
            available: Some(12000),
        }
    }

    #[test]
    fn memory_fits() {
        assert_eq!(memory_admission(&host(8192, 4096), None).unwrap(), None);
        assert_eq!(
            memory_admission(&host(8192, 4096), Some(1.0)).unwrap(),
            None
        );
    }

    #[test]
    fn memory_past_the_host_warns_unless_a_limit_is_set() {
        let w = memory_admission(&host(16384, 4096), None).unwrap().unwrap();
        assert!(
            w.contains("given 20480 MiB") && w.contains("16384 MiB"),
            "{w}"
        );
        // 1.5 times the host's 16 GiB is 24 GiB: it fits, with the warning.
        assert!(memory_admission(&host(16384, 4096), Some(1.5))
            .unwrap()
            .is_some());
        let e = memory_admission(&host(16384, 4096), Some(1.0)).unwrap_err();
        assert!(format!("{e}").contains("memory_overcommit: 1"), "{e}");
    }

    #[test]
    fn little_available_warns() {
        let w = memory_admission(&host(0, 13000), None).unwrap().unwrap();
        assert!(w.contains("12000 MiB available"), "{w}");
    }

    #[test]
    fn unknown_host_memory_admits() {
        let m = Memory {
            total: None,
            available: None,
            ..host(1 << 30, 4096)
        };
        assert_eq!(memory_admission(&m, Some(1.0)).unwrap(), None);
    }

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

        t.core.forget_session(&back.id).unwrap();
        for f in [back.dir.saved(), back.dir.ca(), back.dir.dns()] {
            assert!(!f.exists(), "{}", f.display());
        }
        restore(t.core.daemon());
        assert!(
            t.core.daemon().sessions.get(&back.id).is_none(),
            "not listed again"
        );
    }

    #[test]
    fn a_vmm_is_known_by_its_command_line() {
        assert!(!is_vmm_of(std::process::id(), "abc234"));
        assert!(!is_vmm_of(u32::MAX - 1, "abc234"));
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
}
