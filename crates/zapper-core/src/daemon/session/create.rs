//! Making a session: its profile resolved, admitted for the session limit
//! and the host's memory, assembled, listed.

use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};

use super::persist::save;
use super::run::boot;
use super::{Counters, GuestRuntime, NewSession, Rules, Saved, Session, SessionDir};
use crate::daemon::{dns, driver, harness, now, random_id, record, tls, Daemon, EventBody};

/// Creates a session without booting it: its profile resolved, its
/// directory and audit log, its rules; listed, as starting.
pub(crate) fn register(daemon: &Arc<Daemon>, new: &NewSession) -> Result<Arc<Session>> {
    let mut resolved = daemon.config.read().unwrap().resolve(&new.profile)?;
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
    let (memory_warning, admitted) = admit(daemon, &new.profile, resolved.engine.memory_mib)?;
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
        false,
    )?;
    if session.persistent {
        save(&session)?;
    }
    daemon.sessions.insert(session.clone());
    drop(admitted);
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

/// Whether another VM may start, before it is listed as starting: within
/// `daemon.max_sessions`, and the host's memory (see [`memory_admission`]).
/// A warning, logged, when it may but the memory is short. The admission
/// holds until the guard returned is dropped, which the caller does once
/// the session counts as starting, so admissions at once see each other.
pub(super) fn admit<'a>(
    daemon: &'a Daemon,
    profile: &str,
    memory_mib: u32,
) -> Result<(Option<String>, std::sync::MutexGuard<'a, ()>)> {
    let admitting = daemon.sessions.admitting();
    let (max, overcommit) = {
        let c = daemon.config.read().unwrap();
        (c.daemon.max_sessions, c.daemon.memory_overcommit)
    };
    let running = daemon.sessions.running();
    if running >= max {
        bail!("{running} sessions running; daemon.max_sessions is {max}");
    }
    let warning = memory_admission(
        &Memory {
            given: daemon.sessions.memory_mib(),
            want: memory_mib as u64,
            total: crate::util::host_memory_mib("MemTotal"),
            available: crate::util::host_memory_mib("MemAvailable"),
        },
        overcommit,
    )?;
    if let Some(w) = &warning {
        tracing::warn!("profile {profile}: {w}");
    }
    Ok((warning, admitting))
}

/// The host's memory against a new VM's, in MiB. `total` and `available`
/// are unknown when `/proc/meminfo` can't be read.
pub(super) struct Memory {
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
pub(super) fn memory_admission(m: &Memory, overcommit: Option<f64>) -> Result<Option<String>> {
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
pub(super) fn assemble(
    daemon: &Arc<Daemon>,
    saved: Saved,
    shared_layers: Vec<(
        zapper_policy::Scope,
        Arc<std::sync::RwLock<zapper_policy::Policy>>,
    )>,
    persistent: bool,
    accept_changed_image: bool,
    audit: Option<record::Writer>,
    restored: bool,
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
    let rules = Rules::new(&saved.id, shared_layers);
    let driver = saved.driven.then(|| match persistent {
        true => driver::Driver::kept(dir.conversation()),
        false => driver::Driver::new(),
    });
    let session = Arc::new(Session {
        tls,
        id: saved.id,
        profile: saved.profile,
        created: saved.created,
        expires: Mutex::new((ttl > 0).then_some(now() + ttl as f64)),
        persistent,
        boots: AtomicU64::new(0),
        saving: Mutex::new(()),
        lifecycle: tokio::sync::Mutex::new(()),
        accept_changed_image,
        workdir: saved.workdir,
        rules,
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
        driver,
        daemon: Arc::downgrade(daemon),
        extra_files: saved.files,
        forwards: saved.forwards,
    });
    if restored {
        // Kept with their ids, which clients revoke them by.
        session.rules.restore(saved.rules);
    } else {
        // A new session's rules get ids of the session's own; saved once,
        // by `register`, not per rule.
        for rule in saved.rules.rules() {
            session.rules.add(rule, None);
        }
    }
    Ok(session)
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
