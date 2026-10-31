//! Starting a session's VM: its disks, the guest's sockets, the VMM, and
//! watching it end.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::net::UnixListener;
use zapper_policy::proto::{Forward, GuestDisk, CONTROL_PORT, MCP_PORT, PROXY_PORT};
use zapper_policy::MountMode;

use super::control::guest_control;
use super::disks::check_image;
use super::{spec, Session, Status};
use crate::daemon::{gateway, nic, proxy, vm, Daemon};

/// The guest ports the host connects to over vsock: those the session was
/// created with, then the profile's `engine.ports` not among them.
pub(super) fn forwards(engine: &zapper_policy::Engine, given: &[Forward]) -> Vec<Forward> {
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
    // already (`run::start`); otherwise each is made afresh.
    let kept = |p: &std::path::Path| session.persistent && p.exists();
    if !kept(&root) {
        let image = daemon.paths.images.join(format!("{}.ext4", engine.image));
        if session.persistent {
            check_image(daemon, session, &image).await?;
        }
        vm::root_disk(&image, &root, vm::parse_size(&engine.disk)?)
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
    serve_guest(daemon, session)?;
    let nic_on = engine.network == zapper_policy::NetworkMode::Nic;

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
        durable: session.persistent,
    })
    .await?;
    *session.runtime.vm().lock().await = Some(vm::VmProc::Child(child));
    // Stopped while this VM was made: its teardown found none to stop, so
    // this one is the caller's to end (`teardown` takes it from here).
    if !matches!(session.runtime.current(), Status::Starting) {
        bail!("stopped while it started");
    }

    {
        let (d, s) = (daemon.clone(), session.clone());
        tokio::spawn(async move {
            if let Err(e) = guest_control(&d, &s, control, control_path, spec).await {
                tracing::warn!(session = %s.id, "guest control: {e:#}");
                s.runtime.fail_if_starting(format!("{e:#}"));
            }
        });
    }
    watch_vm(daemon, session);
    Ok(())
}

/// The guest's sockets besides the control channel: the egress proxy (vsock
/// mode), the MCP gateway, and the NIC's capture. Bound afresh, an earlier
/// daemon's left behind removed (Firecracker's own `v.sock` is never one).
pub(super) fn serve_guest(daemon: &Arc<Daemon>, session: &Arc<Session>) -> Result<()> {
    let dir = &session.dir;
    let bind = |path: PathBuf| {
        let _ = std::fs::remove_file(&path);
        UnixListener::bind(&path)
    };
    // A boot's listeners end with it (`GuestRuntime::clear_guest`); any
    // left from before (an adoption after a failed start) go first.
    let rt = &session.runtime;
    rt.end_listeners();
    let proxy_listener = bind(dir.vsock_port(PROXY_PORT))?;
    // The MCP gateway, to the servers registered for the session.
    let mcp = bind(dir.vsock_port(MCP_PORT))?;
    let task = tokio::spawn(gateway::serve(daemon.clone(), session.clone(), mcp));
    rt.hold_listener(task.abort_handle());
    if session.resolved.engine.network == zapper_policy::NetworkMode::Nic {
        let listener = bind(dir.nic_socket())?;
        let task = tokio::spawn(nic::serve(daemon.clone(), session.clone(), listener));
        rt.hold_listener(task.abort_handle());
    }
    let (d, s) = (daemon.clone(), session.clone());
    let limit = rt.conns();
    let task = tokio::spawn(crate::daemon::accept_loop(
        proxy_listener,
        limit,
        move |stream| {
            let (d, s) = (d.clone(), s.clone());
            async move {
                proxy::serve(d, s, stream).await;
            }
        },
    ));
    rt.hold_listener(task.abort_handle());
    Ok(())
}

/// Stops the session when its VM exits.
pub(super) fn watch_vm(daemon: &Arc<Daemon>, session: &Arc<Session>) {
    let (d, s) = (daemon.clone(), session.clone());
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let mut vm = s.runtime.vm().lock().await;
            let Some(proc) = vm.as_mut() else { return };
            if let Some(reason) = proc.exited() {
                *vm = None;
                drop(vm);
                s.runtime.fail_if_starting(reason.clone());
                super::stop(&d, &s, &reason).await;
                return;
            }
        }
    });
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
