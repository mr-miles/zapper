//! A session's interactive terminals: pumping frames between a client and a
//! guest shell, keeping a bounded scrollback for read-only watchers, and
//! recording the session as an asciicast v2 file.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use zapper_policy::proto::{Frame, MAX_FRAME};

use super::Session;
use crate::daemon::{now, record, Daemon, EventBody};

/// Pumps frames between a client and a guest terminal, recording the
/// terminal as an asciicast v2 file in the session directory. A persistent
/// terminal attached again (`id` already the session's) goes on with its
/// watchers and its recording, which takes only what it didn't have of the
/// output the guest sends again; when its client goes, it is detached, not
/// closed.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn pump_shell<C>(
    daemon: Arc<Daemon>,
    session: Arc<Session>,
    client: C,
    guest: UnixStream,
    id: u64,
    argv: Vec<String>,
    tty: Option<(u16, u16)>,
    persist: bool,
) where
    C: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let existing = session.terminals.lock().unwrap().get(&id).cloned();
    let (term, reattached) = match existing.filter(|t| t.persistent) {
        Some(t) => (t, true),
        None => {
            let cast = Recorder::new(session.dir.root(), id, tty, &argv);
            let t = Arc::new(Terminal::new(
                id,
                argv.clone(),
                tty.unwrap_or((24, 80)),
                persist,
                cast,
            ));
            session.terminals.lock().unwrap().insert(id, t.clone());
            (t, false)
        }
    };
    term.state.lock().unwrap().attached = true;
    daemon.emit(
        Some(&session),
        EventBody::Shell {
            id,
            state: if reattached { "attached" } else { "open" }.into(),
            argv: Some(term.argv.clone()),
            exit_code: None,
        },
    );
    let cast = term.cast.clone();
    let watch = term.clone();
    let (mut cr, mut cw) = tokio::io::split(client);
    let (mut gr, mut gw) = guest.into_split();
    let rec = cast.clone();
    let upstream = tokio::spawn(async move {
        while let Ok(Some(frame)) = read_frame(&mut cr).await {
            match &frame {
                Frame::Data(d) => rec.lock().unwrap().write("i", d),
                Frame::Resize(r, c) => {
                    rec.lock()
                        .unwrap()
                        .write("r", format!("{c}x{r}").as_bytes());
                    watch.push(TermChunk::Resize(*r, *c));
                }
                _ => {}
            }
            let mut buf = Vec::new();
            frame.write_to(&mut buf).ok();
            if gw.write_all(&buf).await.is_err() {
                break;
            }
        }
        let _ = gw.shutdown().await;
    });
    let mut exit_code = None;
    // Where in all the terminal has written the guest's next output starts:
    // a persistent terminal's says so first (`Attached`), replaying what it
    // keeps.
    let mut at = term.state.lock().unwrap().recorded;
    while let Ok(Some(frame)) = read_frame(&mut gr).await {
        match &frame {
            Frame::Attached { from, .. } => {
                at = *from;
                let mut st = term.state.lock().unwrap();
                let note = if std::mem::take(&mut st.adopted) {
                    // Adopted: what came before this was recorded by an
                    // earlier daemon, or not at all; record from here.
                    st.recorded = *from;
                    Some(
                        "output before this was recorded by an earlier daemon, if at all"
                            .to_string(),
                    )
                } else if *from > st.recorded {
                    let gap = format!("{} bytes no longer kept by the guest", from - st.recorded);
                    st.recorded = *from;
                    Some(gap)
                } else {
                    None
                };
                drop(st);
                if let Some(note) = note {
                    cast.lock().unwrap().write("m", note.as_bytes());
                }
            }
            Frame::Data(d) | Frame::Stderr(d) => {
                // Only what wasn't recorded yet: a replay repeats the rest.
                let start = at;
                at += d.len() as u64;
                let mut st = term.state.lock().unwrap();
                if at > st.recorded {
                    let new = &d[(st.recorded.saturating_sub(start)) as usize..];
                    st.recorded = at;
                    drop(st);
                    cast.lock().unwrap().write("o", new);
                    term.push(TermChunk::Output(new.to_vec()));
                }
            }
            Frame::Exit(c) => exit_code = Some(*c),
            _ => {}
        }
        let mut buf = Vec::new();
        frame.write_to(&mut buf).ok();
        if cw.write_all(&buf).await.is_err() || exit_code.is_some() {
            break;
        }
    }
    let _ = cw.shutdown().await;
    upstream.abort();
    term.state.lock().unwrap().attached = false;
    // Its program keeps running in the guest: detached, to attach again.
    let detached = term.persistent && exit_code.is_none();
    if !detached {
        term.push(TermChunk::End(exit_code));
    }
    daemon.emit(
        Some(&session),
        EventBody::Shell {
            id,
            state: if detached { "detached" } else { "closed" }.into(),
            argv: None,
            exit_code,
        },
    );
}

impl Session {
    /// The persistent terminals the guest says it keeps, when its control
    /// channel comes back (a daemon that adopts the session knows none of
    /// them): listed, detached, to attach to again, their recordings going
    /// on in their files.
    pub(crate) fn adopt_terminals(&self, ids: &[u64]) {
        let mut terminals = self.terminals.lock().unwrap();
        for &id in ids {
            self.runtime.shells_after(id);
            if terminals.contains_key(&id) {
                continue;
            }
            let cast = Recorder::reopen(self.dir.root(), id);
            let t = Terminal::new(id, vec![], (24, 80), true, cast);
            // Where its recording stopped is an earlier daemon's: it goes on
            // from the first output the guest sends again.
            t.state.lock().unwrap().adopted = true;
            terminals.insert(id, Arc::new(t));
        }
    }
}

/// What a watcher of a terminal receives.
#[derive(Debug, Clone)]
pub enum TermChunk {
    Output(Vec<u8>),
    /// Rows, columns.
    Resize(u16, u16),
    /// The terminal closed, with the command's exit code if known.
    End(Option<i32>),
}

/// A terminal's output so far and its live stream, for read-only watchers.
pub struct Terminal {
    pub id: u64,
    pub argv: Vec<String>,
    /// Its program outlives its connection, to attach to again.
    pub persistent: bool,
    state: Mutex<TermState>,
    tx: tokio::sync::broadcast::Sender<TermChunk>,
    /// Its recording, which a persistent terminal's attachments share.
    cast: Arc<Mutex<Recorder>>,
}

struct TermState {
    /// Current size.
    size: (u16, u16),
    /// Size in effect at the start of `history`.
    base: (u16, u16),
    open: bool,
    /// A client is attached (a persistent terminal can be open without).
    attached: bool,
    /// Adopted from an earlier daemon: where its recording stopped isn't
    /// known here, so it goes on from the next `Attached`.
    adopted: bool,
    /// How much of the terminal's output is recorded.
    recorded: u64,
    history: VecDeque<TermChunk>,
    bytes: usize,
}

/// Output kept per terminal for watchers who join late.
const TERMINAL_HISTORY: usize = 2 << 20;

#[derive(Serialize)]
pub struct TerminalInfo {
    pub id: u64,
    pub argv: Vec<String>,
    pub rows: u16,
    pub cols: u16,
    pub open: bool,
    pub persistent: bool,
    pub attached: bool,
}

impl Terminal {
    fn new(id: u64, argv: Vec<String>, size: (u16, u16), persistent: bool, cast: Recorder) -> Self {
        let state = TermState {
            size,
            base: size,
            open: true,
            attached: false,
            adopted: false,
            recorded: 0,
            history: Default::default(),
            bytes: 0,
        };
        Terminal {
            id,
            argv,
            persistent,
            state: Mutex::new(state),
            tx: tokio::sync::broadcast::channel(1024).0,
            cast: Arc::new(Mutex::new(cast)),
        }
    }

    /// Ended without a client attached (a hang-up), with its exit code.
    pub(crate) fn end(&self, exit_code: Option<i32>) {
        self.push(TermChunk::End(exit_code));
    }

    fn push(&self, chunk: TermChunk) {
        let mut st = self.state.lock().unwrap();
        match &chunk {
            TermChunk::Output(d) => st.bytes += d.len(),
            TermChunk::Resize(r, c) => st.size = (*r, *c),
            TermChunk::End(_) => st.open = false,
        }
        st.history.push_back(chunk.clone());
        while st.bytes > TERMINAL_HISTORY {
            match st.history.pop_front() {
                Some(TermChunk::Output(d)) => st.bytes -= d.len(),
                Some(TermChunk::Resize(r, c)) => st.base = (r, c),
                Some(_) => {}
                None => break,
            }
        }
        // Sent under the lock, so a watcher subscribing in `watch` sees each
        // chunk exactly once.
        let _ = self.tx.send(chunk);
    }

    /// The kept output, starting with the size it was written at, and a
    /// receiver for what comes after.
    pub fn watch(&self) -> (Vec<TermChunk>, tokio::sync::broadcast::Receiver<TermChunk>) {
        let st = self.state.lock().unwrap();
        let rx = self.tx.subscribe();
        let mut snapshot = Vec::with_capacity(st.history.len() + 1);
        snapshot.push(TermChunk::Resize(st.base.0, st.base.1));
        snapshot.extend(st.history.iter().cloned());
        (snapshot, rx)
    }

    pub fn info(&self) -> TerminalInfo {
        let st = self.state.lock().unwrap();
        TerminalInfo {
            id: self.id,
            argv: self.argv.clone(),
            rows: st.size.0,
            cols: st.size.1,
            open: st.open,
            persistent: self.persistent,
            attached: st.attached,
        }
    }
}

pub async fn read_frame(
    r: &mut (impl tokio::io::AsyncRead + Unpin),
) -> std::io::Result<Option<Frame>> {
    let mut header = [0u8; 5];
    match r.read_exact(&mut header).await {
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        other => other?,
    };
    let len = u32::from_be_bytes(header[1..].try_into().unwrap()) as usize;
    if len > MAX_FRAME {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut body = vec![0; len];
    r.read_exact(&mut body).await?;
    Frame::decode(header[0], body).map(Some)
}

/// asciicast v2 writer.
struct Recorder {
    file: Option<record::Writer>,
    start: Instant,
}

impl Recorder {
    fn new(dir: &Path, id: u64, tty: Option<(u16, u16)>, argv: &[String]) -> Self {
        let file = record::Writer::open(&dir.join(format!("shell-{id}.cast"))).ok();
        let (rows, cols) = tty.unwrap_or((24, 80));
        let mut r = Recorder {
            file,
            start: Instant::now(),
        };
        let header = serde_json::json!({ "version": 2, "width": cols, "height": rows, "timestamp": now() as u64, "command": argv.join(" ") });
        if let Some(f) = r.file.as_mut() {
            let _ = f.line(header.to_string().as_bytes());
        }
        r
    }

    /// Goes on with terminal `id`'s recording, written by an earlier daemon:
    /// its times start again from zero, after a marker.
    fn reopen(dir: &Path, id: u64) -> Self {
        let path = dir.join(format!("shell-{id}.cast"));
        let mut r = if path.exists() {
            Recorder {
                file: record::Writer::open(&path).ok(),
                start: Instant::now(),
            }
        } else {
            Recorder::new(dir, id, None, &[])
        };
        r.write("m", b"recording goes on after a daemon restart");
        r
    }

    fn write(&mut self, kind: &str, data: &[u8]) {
        if let Some(f) = self.file.as_mut() {
            let t = self.start.elapsed().as_secs_f64();
            let line = serde_json::json!([t, kind, String::from_utf8_lossy(data)]);
            let _ = f.line(line.to_string().as_bytes());
        }
    }
}

#[cfg(test)]
mod persistent_tests {
    use super::*;
    use crate::daemon::session::NewSession;
    use crate::testing::TestCore;

    /// One attachment of terminal 1: the guest's side sends `frames`, then
    /// goes; the pump runs to its end.
    async fn attachment(t: &TestCore, s: &Arc<Session>, frames: &[Frame]) {
        let (guest, mut fake) = UnixStream::pair().unwrap();
        let (client, _keep) = tokio::io::duplex(1 << 20);
        let mut buf = Vec::new();
        for f in frames {
            f.write_to(&mut buf).unwrap();
        }
        fake.write_all(&buf).await.unwrap();
        drop(fake);
        pump_shell(
            t.core.daemon().clone(),
            s.clone(),
            client,
            guest,
            1,
            vec!["sh".into()],
            Some((24, 80)),
            true,
        )
        .await;
    }

    /// The output events of `shell-1.cast`, and its markers.
    fn cast(s: &Session) -> (String, Vec<String>) {
        let text = std::fs::read_to_string(s.dir.root().join("shell-1.cast")).unwrap();
        let mut out = String::new();
        let mut marks = Vec::new();
        for line in text.lines().skip(1) {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            match v[1].as_str() {
                Some("o") => out.push_str(v[2].as_str().unwrap()),
                Some("m") => marks.push(v[2].as_str().unwrap().to_string()),
                _ => {}
            }
        }
        (out, marks)
    }

    #[tokio::test]
    async fn a_persistent_terminal_is_recorded_once_across_attachments() {
        let t = TestCore::new("profiles:\n  web: {}\n");
        let s = t
            .core
            .test_session(
                NewSession::plain("web".into(), t.workdir(), Default::default()),
                true,
            )
            .unwrap();
        attachment(
            &t,
            &s,
            &[
                Frame::Attached { from: 0, to: 0 },
                Frame::Data(b"one ".to_vec()),
            ],
        )
        .await;
        let term = s.terminals.lock().unwrap().get(&1).cloned().unwrap();
        let info = term.info();
        assert!(info.open && !info.attached && info.persistent, "detached");

        // Attached again: the guest replays what it keeps, "one ", then more.
        attachment(
            &t,
            &s,
            &[
                Frame::Attached { from: 0, to: 9 },
                Frame::Data(b"one two ".to_vec()),
                Frame::Data(b"three ".to_vec()),
            ],
        )
        .await;
        assert_eq!(cast(&s).0, "one two three ");
        assert!(cast(&s).1.is_empty());

        // Attached after the guest dropped some: a gap, then what follows.
        attachment(
            &t,
            &s,
            &[
                Frame::Attached { from: 30, to: 34 },
                Frame::Data(b"four".to_vec()),
                Frame::Exit(0),
            ],
        )
        .await;
        let (out, marks) = cast(&s);
        assert_eq!(out, "one two three four");
        assert_eq!(marks, vec!["16 bytes no longer kept by the guest"]);
        assert!(!term.info().open, "its program exited");
    }

    /// A persistent terminal adopted from an earlier daemon is recorded
    /// again from the first output the guest sends, with a marker.
    #[tokio::test]
    async fn an_adopted_terminal_is_recorded_from_its_first_output() {
        let t = TestCore::new("profiles:\n  web: {}\n");
        let s = t
            .core
            .test_session(
                NewSession::plain("web".into(), t.workdir(), Default::default()),
                true,
            )
            .unwrap();
        s.adopt_terminals(&[1]);
        attachment(
            &t,
            &s,
            &[
                Frame::Attached { from: 100, to: 110 },
                Frame::Data(b"kept".to_vec()),
                Frame::Data(b" live".to_vec()),
            ],
        )
        .await;
        let (out, marks) = cast(&s);
        assert_eq!(out, "kept live");
        assert_eq!(marks.len(), 2, "{marks:?}");
        assert!(marks[1].contains("earlier daemon"), "{marks:?}");
    }
}
