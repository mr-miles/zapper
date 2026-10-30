//! Booting a session's VM and running its guest: building the disks,
//! rendering the guest spec, binding the vsock listeners and the NIC, starting
//! Firecracker, and then the guest control channel — the ready/failed
//! handshake, exec decisions and guest events — over its whole life.
//!
//! `lifecycle::create` calls [`launch`]; everything here is the launch
//! sequence and the long-running tasks it spawns.

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::mpsc;
use zapper_policy::proto::{
    Forward, GuestDisk, GuestMsg, HostMsg, CONTROL_PORT, MCP_PORT, PROXY_PORT,
};
use zapper_policy::{evaluate_exec, Action, AnyRule, ExecRequest, ExecRule, Kind, MountMode};

use super::spec;
use super::{read_guest_line, Session, Status};
use crate::daemon::{gateway, nic, proxy, vm, Daemon, EventBody};

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

pub(super) async fn launch(daemon: &Arc<Daemon>, session: &Arc<Session>) -> Result<()> {
    let dir = &session.dir;
    std::fs::create_dir_all(dir.vm())?;
    let engine = &session.resolved.engine;
    let root = dir.root_disk();
    // A persistent session started again boots the disks it kept, checked
    // already (`lifecycle::start`); otherwise each is made afresh.
    let kept = |p: &std::path::Path| session.persistent && p.exists();
    if !kept(&root) {
        vm::root_disk(
            &daemon.paths.images.join(format!("{}.ext4", engine.image)),
            &root,
            vm::parse_size(&engine.disk)?,
        )
        .await
        .context("root disk")?;
    }
    let mut disks = Vec::new();
    let mut guest_disks = Vec::new();
    for (i, m) in session.resolved.mounts.iter().enumerate() {
        let path = dir.data_disk(i);
        if !kept(&path) {
            let host = vm::mount_disk(m, &session.workdir, &path)
                .await
                .with_context(|| format!("mount {}", m.host))?;
            tracing::debug!(session = %session.id, "mount {} -> {}", host.display(), m.guest);
        }
        let ro = m.mode == MountMode::Ro;
        disks.push((path, ro));
        let device = format!("/dev/vd{}", (b'b' + i as u8) as char);
        guest_disks.push(GuestDisk {
            device,
            path: m.guest.clone(),
            read_only: ro,
        });
    }
    // Guest command secrets are minted once, for the VM to hold.
    let to_mint = spec::guest_mints(&daemon.config.read().unwrap(), &session.resolved);
    let mut minted = std::collections::HashMap::new();
    for (name, m) in to_mint {
        let v = crate::mint::resolve(&m)
            .await
            .with_context(|| format!("secret {name}: minting the secret"))?;
        minted.insert(name, v);
    }
    let spec = {
        let c = daemon.config.read().unwrap();
        spec::build(
            &c,
            &minted,
            &session.resolved,
            &session.id,
            session.tls.pem(),
            guest_disks,
            session.extra_files.clone(),
            forwards(&session.resolved.engine, &session.forwards),
        )?
    };

    // Guest-initiated vsock connections arrive on `<uds>_<port>`.
    let control_path = dir.vsock_port(CONTROL_PORT);
    let control = UnixListener::bind(&control_path)?;
    let proxy_listener = UnixListener::bind(dir.vsock_port(PROXY_PORT))?;
    // The MCP gateway, to the servers registered for the session.
    let mcp = UnixListener::bind(dir.vsock_port(MCP_PORT))?;
    tokio::spawn(gateway::serve(daemon.clone(), session.clone(), mcp));
    let nic_on = engine.network == zapper_policy::NetworkMode::Nic;
    if nic_on {
        let listener = UnixListener::bind(dir.nic_socket())?;
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
    *session.runtime.vm().lock().await = Some(child);

    {
        let (d, s) = (daemon.clone(), session.clone());
        tokio::spawn(async move {
            if let Err(e) = guest_control(&d, &s, control, control_path, spec).await {
                tracing::warn!(session = %s.id, "guest control: {e:#}");
                if matches!(*s.runtime.status().borrow(), Status::Starting) {
                    s.runtime
                        .status()
                        .send_replace(Status::Failed(format!("{e:#}")));
                }
            }
        });
    }
    {
        let (d, s) = (daemon.clone(), session.clone());
        let limit = session.runtime.conns();
        tokio::spawn(crate::daemon::accept_loop(
            proxy_listener,
            limit,
            move |stream| {
                let (d, s) = (d.clone(), s.clone());
                async move {
                    proxy::serve(d, s, stream).await;
                }
            },
        ));
    }
    {
        // VM exit watcher.
        let (d, s) = (daemon.clone(), session.clone());
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(500)).await;
                let mut vm = s.runtime.vm().lock().await;
                let Some(child) = vm.as_mut() else { return };
                if let Ok(Some(status)) = child.try_wait() {
                    *vm = None;
                    drop(vm);
                    let reason = format!("vm exited ({status})");
                    if matches!(*s.runtime.status().borrow(), Status::Starting) {
                        s.runtime
                            .status()
                            .send_replace(Status::Failed(reason.clone()));
                    }
                    super::stop(&d, &s, &reason).await;
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
    session.runtime.set_guest(tx.clone());
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
                session.runtime.status().send_replace(Status::Running);
            }
            GuestMsg::Failed { error } => {
                session
                    .runtime
                    .status()
                    .send_replace(Status::Failed(format!("guest setup: {error}")));
            }
            GuestMsg::ExecCheck { id, pid, uid, req } => {
                let Some(permit) = session.runtime.exec_permit() else {
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
        evaluate_exec(&Session::layers_of(&layers), &req)
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
