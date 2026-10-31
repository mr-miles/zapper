//! The guest agent's control channel: the first connection, a persistent
//! session's resumption, and the messages on it.

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::mpsc;
use zapper_policy::proto::{GuestMsg, HostMsg, CONTROL_PORT, MAX_LINE};

use super::exec::decide_exec;
use super::{Session, Status};
use crate::daemon::{Daemon, EventBody};

pub(super) async fn guest_control(
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
pub(super) const RESUME_CHECKS: usize = 64;

/// Records the control connections refused since last time, if any: how
/// many, and why the last was.
pub(super) fn note_refused(
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
pub(super) async fn resumed(
    stream: &mut tokio::net::UnixStream,
    token: &str,
) -> Result<Vec<u64>, String> {
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
pub(super) async fn serve_control(
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
                session.runtime.mark_ready();
            }
            GuestMsg::Failed { error } => {
                session.runtime.fail(format!("guest setup: {error}"));
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

/// One line from the guest's control channel, at most `MAX_LINE` bytes:
/// `None` at the end, an error for a longer one. Shared by the guest-control
/// loop and nothing else.
pub(crate) async fn read_guest_line(
    reader: &mut (impl tokio::io::AsyncBufRead + Unpin),
) -> Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    let n = reader
        .take(MAX_LINE as u64 + 1)
        .read_until(b'\n', &mut line)
        .await?;
    if n == 0 {
        return Ok(None);
    }
    if n > MAX_LINE {
        bail!("guest sent an oversized message");
    }
    Ok(Some(line))
}

#[cfg(test)]
mod resume_tests {
    use super::super::launch::serve_guest;
    use super::*;
    use crate::daemon::session::NewSession;
    use crate::testing::TestCore;
    use tokio::io::AsyncBufReadExt;

    /// The tasks accepting on a guest's sockets end when it stops: none is
    /// left holding the session, and its sockets accept nothing more.
    #[tokio::test]
    async fn a_stopped_sessions_listeners_end() {
        let t = TestCore::new("profiles:\n  web: {}\n");
        let s = t
            .core
            .test_session(
                NewSession::plain("web".into(), t.workdir(), Default::default()),
                true,
            )
            .unwrap();
        std::fs::create_dir_all(s.dir.vm()).unwrap();
        let before = Arc::strong_count(&s);
        serve_guest(t.core.daemon(), &s).unwrap();
        assert!(Arc::strong_count(&s) > before, "the listeners hold it");
        // Serving again (an adoption) ends the first boot's.
        serve_guest(t.core.daemon(), &s).unwrap();
        let held = Arc::strong_count(&s);

        super::super::stop(t.core.daemon(), &s, "stopped").await;
        for _ in 0..100 {
            if Arc::strong_count(&s) <= before {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            Arc::strong_count(&s) <= before,
            "held by {} before, {held} serving, {} after",
            before,
            Arc::strong_count(&s)
        );
    }

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
