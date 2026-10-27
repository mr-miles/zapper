//! A session's life: resolving and registering it, booting its VM, the guest
//! control channel and exec decisions, and stopping and tearing it down.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::{mpsc, watch, Semaphore};
use zapper_policy::proto::{
    Forward, GuestDisk, GuestMsg, HostMsg, CONTROL_PORT, MCP_PORT, PROXY_PORT,
};
use zapper_policy::{
    evaluate_exec, Action, AnyRule, ExecRequest, ExecRule, Kind, MountMode, Policy,
};

use super::{read_guest_line, Counters, NewSession, Session, Status};
use crate::daemon::{
    driver, gateway, harness, nic, now, proxy, random_id, record, spec, tls, vm, Daemon, EventBody,
};

/// The guest ports the host connects to over vsock: those the session was
/// created with, then the profile's `engine.ports` not among them.
fn forwards(engine: &zapper_policy::Engine, given: &[Forward]) -> Vec<Forward> {
    let mut v = given.to_vec();
    for p in &engine.ports {
        if !v.iter().any(|f| f.vsock_port == *p as u32) {
            v.push(Forward {
                vsock_port: *p as u32,
                tcp_port: *p,
                nic: engine.network == zapper_policy::NetworkMode::Nic,
            });
        }
    }
    v
}

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
    let dir = daemon.paths.state.join("sessions").join(&id);
    std::fs::create_dir_all(&dir)?;
    let audit = record::Writer::open(&dir.join("audit.jsonl"))?;
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
        policy: RwLock::new(Policy::default()),
        status: watch::channel(Status::Starting).0,
        ca: tls::SessionCa::new(&id)?,
        dir: dir.clone(),
        resolved,
        counters: Counters::default(),
        usage: Mutex::new(None),
        dns: Mutex::new(Default::default()),
        dns_rate: Mutex::new(Default::default()),
        dns_lookups: Arc::new(Semaphore::new(8)),
        terminals: Mutex::new(Default::default()),
        requests: Default::default(),
        guest: Mutex::new(None),
        vm: tokio::sync::Mutex::new(None),
        audit: Mutex::new(Some(audit)),
        next_shell: AtomicU64::new(1),
        conns: Arc::new(Semaphore::new(256)),
        exec_checks: Arc::new(Semaphore::new(64)),
        labels: new.labels.clone(),
        shared_layers: new.shared_layers.clone(),
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
    let (id, dir) = (session.id.clone(), session.dir.clone());
    let ttl = session.resolved.engine.ttl_secs;

    let started = Instant::now();
    if let Err(e) = boot(&daemon, &session).await {
        let msg = format!("{e:#}");
        session.status.send_replace(Status::Failed(msg.clone()));
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
    let mut status = session.status.subscribe();
    let ready = tokio::time::timeout(
        Duration::from_secs(120),
        status.wait_for(|s| !matches!(s, Status::Starting)),
    )
    .await
    .is_ok();
    let state = session.status.borrow().clone();
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
                dir.join("console.log").display()
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

async fn boot(daemon: &Arc<Daemon>, session: &Arc<Session>) -> Result<()> {
    let dir = &session.dir;
    let vmdir = vm::vm_dir(dir);
    std::fs::create_dir_all(&vmdir)?;
    let engine = &session.resolved.engine;
    let root = vmdir.join("root.ext4");
    vm::root_disk(
        &daemon.paths.images.join(format!("{}.ext4", engine.image)),
        &root,
        vm::parse_size(&engine.disk)?,
    )
    .await
    .context("root disk")?;
    let mut disks = Vec::new();
    let mut guest_disks = Vec::new();
    for (i, m) in session.resolved.mounts.iter().enumerate() {
        let path = vmdir.join(format!("disk{i}.ext4"));
        let host = vm::mount_disk(m, &session.workdir, &path)
            .await
            .with_context(|| format!("mount {}", m.host))?;
        let ro = m.mode == MountMode::Ro;
        disks.push((path, ro));
        let device = format!("/dev/vd{}", (b'b' + i as u8) as char);
        guest_disks.push(GuestDisk {
            device,
            path: m.guest.clone(),
            read_only: ro,
        });
        tracing::debug!(session = %session.id, "mount {} -> {}", host.display(), m.guest);
    }
    let spec = {
        let c = daemon.config.read().unwrap();
        spec::build(
            &c,
            &session.resolved,
            &session.id,
            &session.ca.pem,
            guest_disks,
            session.extra_files.clone(),
            forwards(&session.resolved.engine, &session.forwards),
        )?
    };

    // Guest-initiated vsock connections arrive on `<uds>_<port>`.
    let vsock = vm::vsock_path(dir);
    let control_path = PathBuf::from(format!("{}_{CONTROL_PORT}", vsock.display()));
    let proxy_path = PathBuf::from(format!("{}_{PROXY_PORT}", vsock.display()));
    let control = UnixListener::bind(&control_path)?;
    let proxy_listener = UnixListener::bind(&proxy_path)?;
    // The MCP gateway, to the servers registered for the session.
    let mcp = UnixListener::bind(PathBuf::from(format!("{}_{MCP_PORT}", vsock.display())))?;
    tokio::spawn(gateway::serve(daemon.clone(), session.clone(), mcp));
    let nic_on = engine.network == zapper_policy::NetworkMode::Nic;
    if nic_on {
        let listener = UnixListener::bind(vmdir.join(crate::netns::SOCKET))?;
        tokio::spawn(nic::serve(daemon.clone(), session.clone(), listener));
    }

    let subids = daemon.config.read().unwrap().daemon.vmm_subids;
    let child = vm::start(vm::VmSpec {
        firecracker: &daemon.paths.firecracker,
        kernel: daemon.paths.kernels.join(&engine.kernel),
        initrd: &daemon.paths.initrd,
        root,
        disks,
        engine,
        dir,
        id: &session.id,
        nic: nic_on,
        subids,
    })
    .await?;
    *session.vm.lock().await = Some(child);

    {
        let (d, s) = (daemon.clone(), session.clone());
        tokio::spawn(async move {
            if let Err(e) = guest_control(&d, &s, control, control_path, spec).await {
                tracing::warn!(session = %s.id, "guest control: {e:#}");
                if matches!(*s.status.borrow(), Status::Starting) {
                    s.status.send_replace(Status::Failed(format!("{e:#}")));
                }
            }
        });
    }
    {
        let (d, s) = (daemon.clone(), session.clone());
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = proxy_listener.accept().await else {
                    break;
                };
                let Ok(permit) = s.conns.clone().try_acquire_owned() else {
                    drop(stream);
                    continue;
                };
                let (d, s) = (d.clone(), s.clone());
                tokio::spawn(async move {
                    proxy::serve(d, s, stream).await;
                    drop(permit);
                });
            }
        });
    }
    {
        // VM exit watcher.
        let (d, s) = (daemon.clone(), session.clone());
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(500)).await;
                let mut vm = s.vm.lock().await;
                let Some(child) = vm.as_mut() else { return };
                if let Ok(Some(status)) = child.try_wait() {
                    *vm = None;
                    drop(vm);
                    let reason = format!("vm exited ({status})");
                    if matches!(*s.status.borrow(), Status::Starting) {
                        s.status.send_replace(Status::Failed(reason.clone()));
                    }
                    stop(&d, &s, &reason).await;
                    return;
                }
            }
        });
    }
    Ok(())
}

async fn guest_control(
    daemon: &Arc<Daemon>,
    session: &Arc<Session>,
    listener: UnixListener,
    path: PathBuf,
    spec: zapper_policy::proto::GuestSpec,
) -> Result<()> {
    let (stream, _) = tokio::time::timeout(Duration::from_secs(60), listener.accept())
        .await
        .context("guest did not connect")??;
    // Only the first connection is the agent: anything later could be the
    // workload. Remove the socket so further guest connections are refused.
    drop(listener);
    let _ = std::fs::remove_file(&path);

    let (r, mut w) = stream.into_split();
    let (tx, mut rx) = mpsc::channel::<HostMsg>(256);
    tx.send(HostMsg::Init {
        spec: Box::new(spec),
    })
    .await?;
    *session.guest.lock().unwrap() = Some(tx.clone());
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            let mut line = serde_json::to_vec(&msg).unwrap();
            line.push(b'\n');
            if w.write_all(&line).await.is_err() {
                break;
            }
        }
    });

    let mut reader = BufReader::new(r);
    while let Some(line) = read_guest_line(&mut reader).await? {
        let msg: GuestMsg = match serde_json::from_slice(&line) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(session = %session.id, "bad guest message: {e}");
                continue;
            }
        };
        match msg {
            GuestMsg::Ready => {
                session.status.send_replace(Status::Running);
            }
            GuestMsg::Failed { error } => {
                session
                    .status
                    .send_replace(Status::Failed(format!("guest setup: {error}")));
            }
            GuestMsg::ExecCheck { id, pid, uid, req } => {
                let Ok(permit) = session.exec_checks.clone().try_acquire_owned() else {
                    let _ = tx.try_send(HostMsg::ExecReply { id, allow: false });
                    continue;
                };
                let (d, s, tx) = (daemon.clone(), session.clone(), tx.clone());
                tokio::spawn(async move {
                    let allow = decide_exec(&d, &s, pid, uid, req).await;
                    let _ = tx.send(HostMsg::ExecReply { id, allow }).await;
                    drop(permit);
                });
            }
            GuestMsg::Event { event } => {
                use zapper_policy::proto::GuestEvent;
                match &event {
                    GuestEvent::Usage { .. } => {
                        *session.usage.lock().unwrap() = Some(event);
                        continue;
                    }
                    GuestEvent::Exit { .. } => {
                        session.counters.exits.fetch_add(1, Ordering::Relaxed);
                    }
                    _ => {}
                }
                daemon.emit(Some(session), EventBody::Guest { event });
            }
        }
    }
    Ok(())
}

async fn decide_exec(
    daemon: &Arc<Daemon>,
    session: &Arc<Session>,
    pid: i32,
    uid: u32,
    req: ExecRequest,
) -> bool {
    let started = Instant::now();
    let decision = {
        let c = daemon.config.read().unwrap();
        let layers = session.layers(&c);
        evaluate_exec(&spec::layers_of(&layers), &req)
    };
    let allow = match decision.action {
        Action::Allow => true,
        Action::Deny => false,
        Action::Ask => {
            let suggestion = AnyRule::Exec(ExecRule {
                id: None,
                action: Action::Allow,
                path: req.path.clone(),
                args: None,
                final_: false,
                comment: Some("approved".into()),
            });
            daemon
                .ask(
                    session,
                    Kind::Exec,
                    format!("exec {}", req.argv.join(" ")),
                    suggestion,
                )
                .await
        }
    };
    let counter = if allow {
        &session.counters.exec_allowed
    } else {
        &session.counters.exec_denied
    };
    counter.fetch_add(1, Ordering::Relaxed);
    daemon.emit(
        Some(session),
        EventBody::Exec {
            pid,
            uid,
            path: req.path,
            argv: req.argv,
            cwd: req.cwd,
            action: if allow { Action::Allow } else { Action::Deny },
            rule: decision.rule,
            scope: decision.scope.map(|s| s.to_string()),
            wait_ms: started.elapsed().as_secs_f64() * 1000.0,
        },
    );
    allow
}

/// Stops a session's VM and removes its disks; the audit log, console log and
/// recordings stay.
pub(crate) async fn stop(daemon: &Arc<Daemon>, session: &Arc<Session>, reason: &str) {
    {
        let current = session.status.borrow().clone();
        if matches!(current, Status::Stopped(_)) {
            return;
        }
        if !matches!(current, Status::Failed(_)) {
            session.status.send_replace(Status::Stopped(reason.into()));
        }
    }
    session.send(HostMsg::Shutdown);
    teardown(daemon, session, Some(reason)).await;
}

async fn teardown(daemon: &Arc<Daemon>, session: &Arc<Session>, reason: Option<&str>) {
    if let Some(driver) = &session.driver {
        driver.stop();
    }
    if let Some(mut child) = session.vm.lock().await.take() {
        if tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .is_err()
        {
            let _ = child.kill().await;
        }
    }
    crate::subids::release(&session.id);
    *session.guest.lock().unwrap() = None;
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
    let vmdir = vm::vm_dir(&session.dir);
    for f in ["vm.json", "metrics.json"] {
        let _ = std::fs::rename(vmdir.join(f), session.dir.join(f));
    }
    let _ = std::fs::remove_dir_all(vmdir);
    *session.audit.lock().unwrap() = None;
}

#[cfg(test)]
mod forward_tests {
    use zapper_policy::proto::Forward;
    use zapper_policy::{Engine, NetworkMode};

    #[test]
    fn a_profiles_ports_join_the_sessions_forwards_once() {
        let given = [Forward {
            vsock_port: 6443,
            tcp_port: 6443,
            nic: true,
        }];
        let engine = Engine {
            ports: vec![6443, 8080],
            ..Engine::default()
        };
        let f = super::forwards(&engine, &given);
        let ports: Vec<(u32, u16, bool)> = f
            .iter()
            .map(|f| (f.vsock_port, f.tcp_port, f.nic))
            .collect();
        assert_eq!(ports, [(6443, 6443, true), (8080, 8080, true)]);
        // Without a NIC, to the guest's loopback.
        let vsock = Engine {
            ports: vec![8080],
            network: NetworkMode::Vsock,
            ..Engine::default()
        };
        assert!(!super::forwards(&vsock, &[])[0].nic);
    }
}
