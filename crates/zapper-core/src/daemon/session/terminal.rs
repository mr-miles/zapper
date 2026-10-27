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
/// terminal as an asciicast v2 file in the session directory.
pub(crate) async fn pump_shell<C>(
    daemon: Arc<Daemon>,
    session: Arc<Session>,
    client: C,
    guest: UnixStream,
    id: u64,
    argv: Vec<String>,
    tty: Option<(u16, u16)>,
) where
    C: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    daemon.emit(
        Some(&session),
        EventBody::Shell {
            id,
            state: "open".into(),
            argv: Some(argv.clone()),
            exit_code: None,
        },
    );
    let cast = Arc::new(Mutex::new(Recorder::new(
        session.dir.root(),
        id,
        tty,
        &argv,
    )));
    let term = Arc::new(Terminal::new(id, argv.clone(), tty.unwrap_or((24, 80))));
    session.terminals.lock().unwrap().insert(id, term.clone());
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
    while let Ok(Some(frame)) = read_frame(&mut gr).await {
        match &frame {
            Frame::Data(d) | Frame::Stderr(d) => {
                cast.lock().unwrap().write("o", d);
                term.push(TermChunk::Output(d.clone()));
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
    term.push(TermChunk::End(exit_code));
    daemon.emit(
        Some(&session),
        EventBody::Shell {
            id,
            state: "closed".into(),
            argv: None,
            exit_code,
        },
    );
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
    state: Mutex<TermState>,
    tx: tokio::sync::broadcast::Sender<TermChunk>,
}

struct TermState {
    /// Current size.
    size: (u16, u16),
    /// Size in effect at the start of `history`.
    base: (u16, u16),
    open: bool,
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
}

impl Terminal {
    fn new(id: u64, argv: Vec<String>, size: (u16, u16)) -> Self {
        let state = TermState {
            size,
            base: size,
            open: true,
            history: Default::default(),
            bytes: 0,
        };
        Terminal {
            id,
            argv,
            state: Mutex::new(state),
            tx: tokio::sync::broadcast::channel(1024).0,
        }
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

    fn write(&mut self, kind: &str, data: &[u8]) {
        if let Some(f) = self.file.as_mut() {
            let t = self.start.elapsed().as_secs_f64();
            let line = serde_json::json!([t, kind, String::from_utf8_lossy(data)]);
            let _ = f.line(line.to_string().as_bytes());
        }
    }
}
