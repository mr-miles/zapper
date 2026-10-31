//! Running a session: booting it, starting a stopped persistent one again,
//! stopping it, and what is torn down when it stops.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use zapper_policy::proto::HostMsg;

use super::create::admit;
use super::disks::{check_disks, keep_disks};
use super::launch::launch;
use super::{session_event, with_warning};
use super::{Session, Status};
use crate::daemon::{driver, now, record, Daemon};

/// Stops the session at `at` (unix seconds), its lifetime ended, if it is
/// still in run `run`: a session stopped and started again since has a
/// lifetime of its own.
pub(super) fn arm_ttl(daemon: &Arc<Daemon>, session: &Arc<Session>, run: u64, at: f64) {
    let (d, s) = (daemon.clone(), session.clone());
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs_f64((at - now()).max(0.0))).await;
        if s.boots.load(Ordering::Relaxed) == run {
            stop(&d, &s, "ttl expired").await;
        }
    });
}

/// Boots a registered (or stopped, persistent) session's VM and waits for its
/// guest: running, with its driver and its `ttl_secs` for this run.
pub(super) async fn boot(daemon: &Arc<Daemon>, session: &Arc<Session>) -> Result<()> {
    let daemon = daemon.clone();
    let session = session.clone();
    let id = session.id.clone();
    let ttl = session.resolved.engine.ttl_secs;
    let run = session.boots.fetch_add(1, Ordering::Relaxed) + 1;

    let started = Instant::now();
    if let Err(e) = launch(&daemon, &session).await {
        let msg = format!("{e:#}");
        session.runtime.fail_if_starting(msg.clone());
        session_event(&daemon, &session, "failed", msg.as_str());
        teardown(&daemon, &session, None).await;
        return Err(anyhow!(msg));
    }
    let mut status = session.runtime.subscribe();
    let ready = tokio::time::timeout(
        Duration::from_secs(120),
        status.wait_for(|s| !matches!(s, Status::Starting)),
    )
    .await
    .is_ok();
    let state = session.runtime.current();
    match (ready, state) {
        (true, Status::Running) => {
            let ms = started.elapsed().as_millis();
            session_event(&daemon, &session, "running", format!("ready in {ms} ms"));
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
        arm_ttl(&daemon, &session, run, now() + ttl as f64);
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
    // Not while a stop is on its disks, and one start at a time: claimed
    // before the disks are checked, so a second start sees it starting.
    let lifecycle = session.lifecycle.lock().await;
    let was = session.runtime.current();
    let (memory_warning, admitted) =
        admit(daemon, &session.profile, session.resolved.engine.memory_mib)?;
    if !session.runtime.claim_start() {
        bail!("session {id} is already running");
    }
    drop(admitted);
    let undo = |e: anyhow::Error| {
        session.runtime.undo_start(was.clone());
        *session.audit.lock().unwrap() = None;
        e
    };
    *session.audit.lock().unwrap() =
        Some(record::Writer::open(&session.dir.audit()).map_err(undo)?);
    check_disks(daemon, session, accept_changed)
        .await
        .map_err(undo)?;
    drop(lifecycle);
    let ttl = session.resolved.engine.ttl_secs;
    *session.expires.lock().unwrap() = (ttl > 0).then(|| now() + ttl as f64);
    let detail = with_warning("started again, from its kept disks".into(), &memory_warning);
    session_event(daemon, session, "starting", detail);
    if let Some(driver) = &session.driver {
        driver.revive();
    }
    boot(daemon, session).await
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
    // A start checking the disks finishes first; another start waits for
    // this one's teardown.
    let _lifecycle = session.lifecycle.lock().await;
    if !session.runtime.begin_stop(reason) {
        return;
    }
    session.send(HostMsg::Shutdown);
    teardown(daemon, session, Some(reason)).await;
}

pub(super) async fn teardown(daemon: &Arc<Daemon>, session: &Arc<Session>, reason: Option<&str>) {
    if let Some(driver) = &session.driver {
        driver.stop();
    }
    if let Some(mut vm) = session.runtime.vm().lock().await.take() {
        vm.stop_within(Duration::from_secs(5)).await;
    }
    crate::subids::release(&session.id);
    session.runtime.clear_guest();
    daemon.drop_approvals(session);
    if let Some(reason) = reason {
        session_event(daemon, session, "stopped", reason);
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
