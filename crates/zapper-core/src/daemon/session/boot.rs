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

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::mpsc;
use zapper_policy::proto::{
    Forward, GuestDisk, GuestMsg, HostMsg, CONTROL_PORT, MAX_LINE, MCP_PORT, PROXY_PORT,
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

/// Before a persistent session's disk is made from its image: the image's
/// hash, compared with the one recorded when it was built, in a `disks`
/// event (`image`). One changed since is refused unless
/// `accept_changed_image`; one built before images were hashed is recorded
/// as unverified, and used.
async fn check_image(
    daemon: &Arc<Daemon>,
    session: &Arc<Session>,
    image: &std::path::Path,
) -> Result<()> {
    let name = session.resolved.engine.image.clone();
    let path = image.to_path_buf();
    let hash = tokio::task::spawn_blocking(move || crate::util::sha256_file(&path)).await??;
    let built = crate::images::built_hash(&daemon.paths.images, &name);
    let (changed, detail) = match &built {
        Some(b) if *b == hash => (false, format!("image {name} as it was built")),
        Some(_) => (true, format!("image {name} changed since it was built")),
        None => (
            false,
            format!("image {name} was built before images were hashed: unverified (rebuild it to have one)"),
        ),
    };
    let accepted = changed && session.accept_changed_image;
    daemon.emit(
        Some(session),
        EventBody::Disks {
            state: "image".into(),
            disks: [(format!("{name}.ext4"), hash)].into(),
            detail: Some(if accepted {
                format!("{detail}; used as it is, as asked")
            } else {
                detail.clone()
            }),
        },
    );
    if changed && !accepted {
        bail!("{detail}; give accept_changed_image (--accept-changed-image) to make the session from it as it is");
    }
    Ok(())
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
    watch_vm(daemon, session);
    Ok(())
}

/// The guest's sockets besides the control channel: the egress proxy (vsock
/// mode), the MCP gateway, and the NIC's capture. Bound afresh, an earlier
/// daemon's left behind removed (Firecracker's own `v.sock` is never one).
fn serve_guest(daemon: &Arc<Daemon>, session: &Arc<Session>) -> Result<()> {
    let dir = &session.dir;
    let bind = |path: PathBuf| {
        let _ = std::fs::remove_file(&path);
        UnixListener::bind(&path)
    };
    let proxy_listener = bind(dir.vsock_port(PROXY_PORT))?;
    // The MCP gateway, to the servers registered for the session.
    let mcp = bind(dir.vsock_port(MCP_PORT))?;
    tokio::spawn(gateway::serve(daemon.clone(), session.clone(), mcp));
    if session.resolved.engine.network == zapper_policy::NetworkMode::Nic {
        let listener = bind(dir.nic_socket())?;
        tokio::spawn(nic::serve(daemon.clone(), session.clone(), listener));
    }
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
    Ok(())
}

/// Stops the session when its VM exits.
fn watch_vm(daemon: &Arc<Daemon>, session: &Arc<Session>) {
    let (d, s) = (daemon.clone(), session.clone());
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let mut vm = s.runtime.vm().lock().await;
            let Some(proc) = vm.as_mut() else { return };
            if let Some(reason) = proc.exited() {
                *vm = None;
                drop(vm);
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

/// Takes on a persistent session whose VM an earlier daemon left running:
/// its guest's sockets bound again, its VMM watched by pid, and its agent's
/// control channel taken again when it connects with its token.
pub(super) async fn adopt(daemon: &Arc<Daemon>, session: &Arc<Session>, pid: u32) -> Result<()> {
    serve_guest(daemon, session)?;
    *session.runtime.vm().lock().await = Some(vm::VmProc::Adopted {
        pid,
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
    // A persistent session's agent may connect again, with this, when the
    // connection is lost (the daemon restarted): kept beside the session,
    // for a daemon that adopts it.
    let resume = if session.persistent {
        let token = crate::daemon::random_id(32);
        crate::util::write_private(&session.dir.resume_token(), token.as_bytes())?;
        Some(token)
    } else {
        None
    };
    let init = HostMsg::Init {
        spec: Box::new(spec),
        resume,
    };
    serve_control(daemon, session, stream, Some(init)).await?;
    if session.persistent {
        resume_control(daemon, session).await?;
    }
    Ok(())
}

/// For a persistent session whose control connection was lost: its agent
/// connects again, and the connection is taken for it only if its first
/// line is `Resume` with the session's token (others, which could be the
/// workload's, are closed); then the socket goes again, as after the first
/// connection. Until the session stops.
pub(crate) async fn resume_control(daemon: &Arc<Daemon>, session: &Arc<Session>) -> Result<()> {
    let path = session.dir.vsock_port(CONTROL_PORT);
    let mut status = session.runtime.status().subscribe();
    loop {
        if !matches!(*status.borrow(), Status::Starting | Status::Running) {
            return Ok(());
        }
        let token = std::fs::read_to_string(session.dir.resume_token())
            .context("the session's resume token")?;
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path)?;
        // Each connection checked in a task of its own, a few at once: one
        // that sends nothing (which the workload could open) holds up no
        // other. The first with the token is taken.
        let (found_tx, mut found_rx) = mpsc::channel::<(tokio::net::UnixStream, Vec<u64>)>(1);
        // Connections being checked, oldest first. Full, the oldest still
        // silent makes room for the new one, which is never refused: the
        // agent sends its line as it connects, so the workload would have
        // to open dozens of connections between the two to push it out.
        let mut checking: std::collections::VecDeque<tokio::task::AbortHandle> = Default::default();
        let refused: Arc<std::sync::Mutex<(u64, String)>> = Default::default();
        let mut tick = tokio::time::interval(Duration::from_secs(10));
        let (stream, terminals) = loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let Ok((mut stream, _)) = accepted else { continue };
                    checking.retain(|h| !h.is_finished());
                    if checking.len() >= RESUME_CHECKS {
                        if let Some(oldest) = checking.pop_front() {
                            oldest.abort();
                            let mut r = refused.lock().unwrap();
                            r.0 += 1;
                            r.1 = "silent while newer connections came".into();
                        }
                    }
                    let (tx, token, refused) = (found_tx.clone(), token.clone(), refused.clone());
                    let task = tokio::spawn(async move {
                        match resumed(&mut stream, &token).await {
                            Ok(terminals) => {
                                let _ = tx.send((stream, terminals)).await;
                            }
                            Err(why) => {
                                let mut r = refused.lock().unwrap();
                                r.0 += 1;
                                r.1 = why;
                            }
                        }
                    });
                    checking.push_back(task.abort_handle());
                }
                Some(found) = found_rx.recv() => break found,
                _ = status.changed() => return Ok(()),
                // Refusals recorded at most every ten seconds, counted.
                _ = tick.tick() => note_refused(daemon, session, &refused),
            }
        };
        note_refused(daemon, session, &refused);
        session.adopt_terminals(&terminals);
        drop(listener);
        let _ = std::fs::remove_file(&path);
        daemon.emit(
            Some(session),
            EventBody::Session {
                state: "running".into(),
                detail: Some("its agent connected again".into()),
            },
        );
        // Told it was taken: the agent asks again what was waiting.
        serve_control(daemon, session, stream, Some(HostMsg::Resumed)).await?;
    }
}

/// Connections checked for a resumption at once.
const RESUME_CHECKS: usize = 64;

/// Records the control connections refused since last time, if any: how
/// many, and why the last was.
fn note_refused(
    daemon: &Arc<Daemon>,
    session: &Arc<Session>,
    refused: &std::sync::Mutex<(u64, String)>,
) {
    let (n, why) = std::mem::take(&mut *refused.lock().unwrap());
    if n == 0 {
        return;
    }
    tracing::warn!(session = %session.id, "refused {n} control connections: {why}");
    daemon.emit(
        Some(session),
        EventBody::Session {
            state: "running".into(),
            detail: Some(format!(
                "refused {n} control connection{} (the last: {why})",
                if n == 1 { "" } else { "s" }
            )),
        },
    );
}

/// Reads a resumed connection's first line: `Resume` with `token`, and the
/// persistent terminals the guest keeps.
async fn resumed(stream: &mut tokio::net::UnixStream, token: &str) -> Result<Vec<u64>, String> {
    // A byte at a time: the exec checks the agent asks again follow the line
    // at once, and must stay in the stream for `serve_control`.
    let line = tokio::time::timeout(Duration::from_secs(2), async {
        let mut line = Vec::new();
        loop {
            match stream.read_u8().await {
                Ok(b'\n') => return Ok(line),
                Ok(b) if line.len() < MAX_LINE => line.push(b),
                Ok(_) => return Err("an oversized first line".to_string()),
                Err(_) => return Err("closed before its first line".to_string()),
            }
        }
    })
    .await
    .map_err(|_| "no first line in time".to_string())??;
    match serde_json::from_slice::<GuestMsg>(&line) {
        Ok(GuestMsg::Resume {
            token: t,
            terminals,
        }) if t == token => Ok(terminals),
        Ok(GuestMsg::Resume { .. }) => Err("a wrong resume token".into()),
        _ => Err("its first line isn't a resumption".into()),
    }
}

/// Serves a guest's control connection until it closes: its messages in, and
/// the host's out (`first` before any).
async fn serve_control(
    daemon: &Arc<Daemon>,
    session: &Arc<Session>,
    stream: tokio::net::UnixStream,
    first: Option<HostMsg>,
) -> Result<()> {
    let (r, mut w) = stream.into_split();
    let (tx, mut rx) = mpsc::channel::<HostMsg>(256);
    if let Some(m) = first {
        tx.send(m).await?;
    }
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
            // Only first, on a connection made again (`resume_control`).
            GuestMsg::Resume { .. } => {}
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

#[cfg(test)]
mod image_tests {
    use super::*;
    use crate::daemon::session::NewSession;
    use crate::testing::TestCore;

    /// An image `web` in the images directory, built (its hash recorded)
    /// with `built`, then holding `now`.
    fn image(t: &TestCore, built: Option<&[u8]>, now: &[u8]) -> std::path::PathBuf {
        let dir = &t.core.daemon().paths.images;
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join("web.ext4");
        if let Some(b) = built {
            std::fs::write(&path, b).unwrap();
            let hash = crate::util::sha256_file(&path).unwrap();
            std::fs::write(
                dir.join("web.json"),
                serde_json::json!({ "sha256": hash }).to_string(),
            )
            .unwrap();
        }
        std::fs::write(&path, now).unwrap();
        path
    }

    fn session(t: &TestCore, accept: bool) -> Arc<Session> {
        let mut new = NewSession::plain("web".into(), t.workdir(), Default::default());
        new.persistent = true;
        new.accept_changed_image = accept;
        t.core.test_session(new, true).unwrap()
    }

    fn image_event(t: &TestCore, s: &Session) -> String {
        t.core
            .daemon()
            .recent_events(Some(&s.id))
            .into_iter()
            .rev()
            .find_map(|e| match e.body {
                EventBody::Disks { state, detail, .. } if state == "image" => detail,
                _ => None,
            })
            .unwrap_or_default()
    }

    const CONFIG: &str = "profiles:\n  web: { engine: { image: web } }\n";

    #[tokio::test]
    async fn a_persistent_sessions_image_is_checked_against_its_build() {
        let t = TestCore::new(CONFIG);
        let path = image(&t, Some(b"as built"), b"as built");
        let s = session(&t, false);
        check_image(t.core.daemon(), &s, &path).await.unwrap();
        assert_eq!(image_event(&t, &s), "image web as it was built");

        let path = image(&t, Some(b"as built"), b"changed");
        let e = check_image(t.core.daemon(), &s, &path).await.unwrap_err();
        assert!(
            e.to_string()
                .contains("image web changed since it was built"),
            "{e}"
        );
        assert_eq!(image_event(&t, &s), "image web changed since it was built");

        let accepting = session(&t, true);
        check_image(t.core.daemon(), &accepting, &path)
            .await
            .unwrap();
        assert!(image_event(&t, &accepting).ends_with("used as it is, as asked"));
    }

    #[tokio::test]
    async fn an_image_built_before_hashing_is_unverified_and_used() {
        let t = TestCore::new(CONFIG);
        let path = image(&t, None, b"old");
        let s = session(&t, false);
        check_image(t.core.daemon(), &s, &path).await.unwrap();
        assert!(
            image_event(&t, &s).contains("unverified"),
            "{}",
            image_event(&t, &s)
        );
    }
}

#[cfg(test)]
mod resume_tests {
    use super::*;
    use crate::daemon::session::NewSession;
    use crate::testing::TestCore;
    use tokio::io::AsyncBufReadExt;

    async fn connect(path: &std::path::Path) -> tokio::net::UnixStream {
        for _ in 0..100 {
            if let Ok(s) = tokio::net::UnixStream::connect(path).await {
                return s;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the control socket didn't come back");
    }

    /// A persistent session's control channel taken again: only with its
    /// token; the exec check sent right after the resumption is answered,
    /// so nothing after the first line is lost; the guest's persistent
    /// terminals are listed.
    #[tokio::test]
    async fn the_control_channel_is_taken_again_only_with_the_token() {
        let t = TestCore::new(
            "profiles:\n  web: { policy: { exec: [{ action: allow, path: \"*\" }] } }\n",
        );
        let mut new = NewSession::plain("web".into(), t.workdir(), Default::default());
        new.persistent = true;
        let s = t.core.test_session(new, true).unwrap();
        std::fs::create_dir_all(s.dir.vm()).unwrap();
        crate::util::write_private(&s.dir.resume_token(), b"the-token").unwrap();
        let (d, s2) = (t.core.daemon().clone(), s.clone());
        let task = tokio::spawn(async move { resume_control(&d, &s2).await });
        let path = s.dir.vsock_port(CONTROL_PORT);

        let mut wrong = connect(&path).await;
        wrong
            .write_all(b"{\"type\":\"resume\",\"token\":\"guessed\",\"terminals\":[]}\n")
            .await
            .unwrap();
        let mut buf = Vec::new();
        let _ = wrong.read_to_end(&mut buf).await;
        assert!(buf.is_empty(), "closed, unanswered");
        // Before the agent: more connections that send nothing than are
        // checked at once, held open, and more opened all the while, as the
        // workload could. They hold the agent up none the less.
        let mut silent = Vec::new();
        for _ in 0..RESUME_CHECKS + 6 {
            silent.push(connect(&path).await);
        }
        let flood = {
            let path = path.clone();
            tokio::spawn(async move {
                let mut held = Vec::new();
                loop {
                    if let Ok(c) = tokio::net::UnixStream::connect(&path).await {
                        held.push(c);
                    }
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
        };

        let mut agent = connect(&path).await;
        agent
            .write_all(
                b"{\"type\":\"resume\",\"token\":\"the-token\",\"terminals\":[3]}\n\
                  {\"type\":\"exec_check\",\"id\":7,\"pid\":10,\"uid\":1000,\"req\":{\"path\":\"/bin/true\"}}\n",
            )
            .await
            .unwrap();
        let mut reader = tokio::io::BufReader::new(agent);
        let mut line = String::new();
        let mut next = async || {
            line.clear();
            tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
                .await
                .unwrap()
                .unwrap();
            line.clone()
        };
        let first = next().await;
        assert!(first.contains("\"resumed\""), "told it was taken: {first}");
        let reply = next().await;
        assert!(
            reply.contains("\"exec_reply\"") && reply.contains("\"id\":7"),
            "{reply}"
        );
        flood.abort();
        drop(silent);
        assert!(!path.exists(), "the socket goes again");
        let refusals: Vec<String> = t
            .core
            .daemon()
            .recent_events(Some(&s.id))
            .into_iter()
            .filter_map(|e| match e.body {
                EventBody::Session {
                    detail: Some(d), ..
                } if d.starts_with("refused") => Some(d),
                _ => None,
            })
            .collect();
        // Counted, not an event each: the wrong token, and the silent
        // connections that made room.
        assert_eq!(refusals.len(), 1, "{refusals:?}");
        assert!(refusals[0].starts_with("refused "), "{refusals:?}");
        assert!(s.terminals.lock().unwrap().contains_key(&3));
        assert!(s.runtime.next_shell() > 3);

        s.runtime
            .status()
            .send_replace(Status::Stopped("stopped".into()));
        drop(reader);
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}
