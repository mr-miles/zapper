//! What an earlier daemon left: persistent sessions listed again, and the
//! VMs it left running adopted (or stopped, when they can't be).

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;

use super::control::resume_control;
use super::create::assemble;
use super::launch::{serve_guest, watch_vm};
use super::run::{arm_ttl, stop};
use super::{disks, Saved, Session, SessionDir};
use crate::daemon::{driver, now, record, vm, Daemon, EventBody};

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
        let id = entry.file_name().to_string_lossy().into_owned();
        let saved = match std::fs::read(dir.saved()) {
            Err(_) => continue,
            Ok(b) => match serde_json::from_slice::<Saved>(&b) {
                Ok(saved) => saved,
                Err(e) => {
                    // Not to be listed or adopted; a VM it left running
                    // isn't left running unwatched.
                    if let Some(vmm) = live_vmm(&dir, &id) {
                        kill_vmm(&vmm, &id);
                    }
                    tracing::warn!(
                        "session {id}: its session.json can't be read ({e}); not listed, its VM stopped if it ran"
                    );
                    continue;
                }
            },
        };
        // Its VMM, still running, which an earlier daemon left: adopted, with
        // a runtime to serve it in; else stopped, so a start can't boot its
        // disks twice.
        let mut left_running = false;
        if let Some(vmm) = live_vmm(&dir, &saved.id) {
            if let Ok(rt) = tokio::runtime::Handle::try_current() {
                match adopt(daemon, saved.clone(), &dir, &vmm, &rt) {
                    Ok(()) => continue,
                    Err(e) => {
                        tracing::warn!("adopting the session in {}: {e:#}", dir.root().display())
                    }
                }
            }
            left_running = kill_vmm(&vmm, &saved.id);
        }
        let _ = std::fs::remove_file(dir.vmm());
        let record =
            std::fs::read_to_string(crate::daemon::record::view(&dir.audit())).unwrap_or_default();
        let was_running = disks::last_stop(&disks::events(&record)) == disks::LastStop::RanSince;
        let session = match assemble(daemon, saved, vec![], true, false, None, true) {
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
        session.runtime.restored(why);
        daemon.sessions.insert(session);
    }
}

/// The durable VMM of session `id` in `dir`, if it still runs (its pid that
/// session's VMM, not one reused since).
pub(super) fn live_vmm(dir: &SessionDir, id: &str) -> Option<crate::daemon::vm::Vmm> {
    let vmm: crate::daemon::vm::Vmm =
        serde_json::from_slice(&std::fs::read(dir.vmm()).ok()?).ok()?;
    vmm.runs_for(id).then_some(vmm)
}

/// Stops session `id`'s VMM `vmm` (its Firecracker dies with it); whether
/// it was running.
pub(super) fn kill_vmm(vmm: &crate::daemon::vm::Vmm, id: &str) -> bool {
    if !vmm.runs_for(id) {
        return false;
    }
    unsafe { libc::kill(vmm.pid as i32, libc::SIGKILL) };
    for _ in 0..50 {
        if !vmm.runs_for(id) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    true
}

/// Takes on a persistent session whose VMM an earlier daemon left running:
/// listed as running, its record written to again, its subordinate id
/// claimed again, its lifetime from when its VM started; then, in `rt`, its
/// guest served again ([`serve_adopted`]), its driver started, and its
/// lifetime watched.
pub(super) fn adopt(
    daemon: &Arc<Daemon>,
    saved: Saved,
    dir: &SessionDir,
    vmm: &crate::daemon::vm::Vmm,
    rt: &tokio::runtime::Handle,
) -> Result<()> {
    let audit = record::Writer::open(&dir.audit())?;
    let session = assemble(daemon, saved, vec![], true, false, Some(audit), true)?;
    if let Some(n) = vmm.subid {
        crate::subids::claim(&session.id, n)?;
    }
    let run = session.boots.fetch_add(1, Ordering::Relaxed) + 1;
    let ttl = session.resolved.engine.ttl_secs;
    // `vmm.json` was written when the VM started.
    let started = std::fs::metadata(dir.vmm())
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
        .unwrap_or_else(now);
    let expires = (ttl > 0).then_some(started + ttl as f64);
    *session.expires.lock().unwrap() = expires;
    session.runtime.adopted();
    daemon.sessions.insert(session.clone());
    daemon.emit(
        Some(&session),
        EventBody::Session {
            state: "running".into(),
            detail: Some("adopted: its VM ran on through a daemon restart".into()),
        },
    );
    let (d, s, vmm) = (daemon.clone(), session, vmm.clone());
    rt.spawn(async move {
        if let Err(e) = serve_adopted(&d, &s, vmm).await {
            stop(&d, &s, &format!("adopting it failed: {e:#}")).await;
            return;
        }
        driver::drive(&d, &s);
        if let Some(at) = expires {
            arm_ttl(&d, &s, run, at);
        }
    });
    Ok(())
}

/// Takes on a persistent session whose VM an earlier daemon left running:
/// its guest's sockets bound again, its VMM watched by pid and start, and
/// its agent's control channel taken again when it connects with its token.
pub(super) async fn serve_adopted(
    daemon: &Arc<Daemon>,
    session: &Arc<Session>,
    vmm: vm::Vmm,
) -> Result<()> {
    serve_guest(daemon, session)?;
    *session.runtime.vm().lock().await = Some(vm::VmProc::Adopted {
        vmm,
        session: session.id.clone(),
    });
    watch_vm(daemon, session);
    let (d, s) = (daemon.clone(), session.clone());
    tokio::spawn(async move {
        if let Err(e) = resume_control(&d, &s).await {
            tracing::warn!(session = %s.id, "taking the control channel again: {e:#}");
        }
    });
    Ok(())
}
