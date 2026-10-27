//! A session's life: resolving and registering it, then stopping and tearing
//! it down. Booting its VM and running its guest control channel is `boot`.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use tokio::sync::Semaphore;
use zapper_policy::proto::HostMsg;

use super::{Counters, GuestRuntime, NewSession, Rules, Session, SessionDir, Status};
use crate::daemon::{dns, driver, harness, now, random_id, record, tls, Daemon, EventBody};

/// Creates a session without booting it: its profile resolved, its
/// directory and audit log, its rules; listed, as starting.
pub(crate) fn register(daemon: &Arc<Daemon>, new: &NewSession) -> Result<Arc<Session>> {
    let (mut resolved, max) = {
        let c = daemon.config.read().unwrap();
        (c.resolve(&new.profile)?, c.daemon.max_sessions)
    };
    if let Some(overlay) = &new.agent_overlay {
        let mut agent = serde_json::to_value(&resolved.agent)?;
        zapper_policy::merge(&mut agent, overlay);
        resolved.agent = serde_json::from_value(agent).context("agent overlay")?;
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
    let id = random_id(6);
    let dir = SessionDir::new(daemon.paths.state.join("sessions").join(&id));
    std::fs::create_dir_all(dir.root())?;
    let audit = record::Writer::open(&dir.audit())?;
    // A firm's workers are driven by the office's foreman.
    let driven = resolved.agent.driven
        && !new.labels.contains_key("firm")
        && daemon.config.read().unwrap().daemon.driver;
    let created = now();
    let ttl = resolved.engine.ttl_secs;
    let session = Arc::new(Session {
        id: id.clone(),
        profile: new.profile.clone(),
        created,
        expires: (ttl > 0).then_some(created + ttl as f64),
        workdir: new.workdir.clone(),
        rules: Rules::new(new.shared_layers.clone()),
        runtime: GuestRuntime::new(),
        tls: tls::SessionTls::new(&id)?,
        dir: dir.clone(),
        resolved,
        counters: Counters::default(),
        usage: Mutex::new(None),
        dns: dns::SessionDns::new(),
        terminals: Mutex::new(Default::default()),
        requests: Default::default(),
        audit: Mutex::new(Some(audit)),
        conns: Arc::new(Semaphore::new(256)),
        labels: new.labels.clone(),
        driver: driven.then(driver::Driver::new),
        daemon: Arc::downgrade(daemon),
        extra_files: new.files.clone(),
        forwards: new.forwards.clone(),
    });
    for rule in new.policy.rules() {
        session.add_rule(rule, None);
    }
    daemon.sessions.insert(session.clone());
    daemon.emit(
        Some(&session),
        EventBody::Session {
            state: "starting".into(),
            detail: Some(format!("profile {}", new.profile)),
        },
    );
    Ok(session)
}

pub(crate) async fn create(daemon: Arc<Daemon>, new: NewSession) -> Result<Arc<Session>> {
    let session = register(&daemon, &new)?;
    let id = session.id.clone();
    let ttl = session.resolved.engine.ttl_secs;

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
            stop(&d, &s, "ttl expired").await;
        });
    }
    Ok(session)
}

/// Stops a session's VM and removes its disks; the audit log, console log and
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
    let _ = std::fs::remove_dir_all(sd.vm());
    *session.audit.lock().unwrap() = None;
}
