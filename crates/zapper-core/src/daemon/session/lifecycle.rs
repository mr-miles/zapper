//! A session's life: resolving and registering it, then stopping and tearing
//! it down. Booting its VM and running its guest control channel is `boot`.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use zapper_policy::proto::HostMsg;

use super::{Counters, GuestRuntime, NewSession, Rules, Session, SessionDir, Status};
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
    let created = now();
    let ttl = resolved.engine.ttl_secs;
    let session = Arc::new(Session {
        id: id.clone(),
        profile: new.profile.clone(),
        created,
        expires: (ttl > 0).then_some(created + ttl as f64),
        workdir: new.workdir.clone(),
        rules: Rules::new(new.shared_layers.clone()),
        secrets: Default::default(),
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

#[cfg(test)]
mod tests {
    use super::{memory_admission, Memory};

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
}
