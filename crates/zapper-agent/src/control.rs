//! The control channel to the host. For a persistent session it outlives a
//! restart of the daemon: when it is lost, the agent connects again and sends
//! the token `Init` gave it (`Resume`), and exec checks still waiting are
//! asked again; one waiting longer than `HOST_GONE` while the host is gone
//! is refused. Without a token, a lost channel powers the guest off.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use zapper_proto::proto::{
    GuestEvent, GuestMsg, GuestSpec, HostMsg, CONTROL_PORT, HOST_CID, MAX_LINE,
};
use zapper_proto::ExecRequest;

use crate::sys;

/// How long an exec check waits while the host is gone before it is
/// refused.
const HOST_GONE: Duration = Duration::from_secs(60);

/// Exec checks waiting for an answer, by id: where to send it, and the
/// question, to ask again on a new connection.
type Pending = Arc<Mutex<HashMap<u64, (Sender<bool>, GuestMsg)>>>;

#[derive(Clone)]
pub struct Control {
    writer: Arc<Mutex<File>>,
    pending: Pending,
    next_id: Arc<AtomicU64>,
    /// What to send on a new connection, if this session's channel may be
    /// made again.
    resume: Option<String>,
    /// Since when the host has been gone, while it is.
    gone_since: Arc<Mutex<Option<Instant>>>,
}

impl Control {
    /// Connects to the host and waits for the session spec.
    pub fn connect() -> Result<(Control, GuestSpec)> {
        let mut attempt = 0;
        let stream = loop {
            match sys::vsock_connect(HOST_CID, CONTROL_PORT) {
                Ok(s) => break s,
                Err(e) if attempt < 50 => {
                    attempt += 1;
                    eprintln!("zapper-agent: control connect: {e}, retrying");
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(e) => return Err(e).context("connecting to host"),
            }
        };
        let mut reader = BufReader::new(stream.try_clone()?);
        let (spec, resume) = match read_msg(&mut reader)? {
            Some(HostMsg::Init { spec, resume }) => (*spec, resume),
            other => bail!("expected init, got {other:?}"),
        };
        let control = Control {
            writer: Arc::new(Mutex::new(stream)),
            pending: Default::default(),
            next_id: Arc::new(AtomicU64::new(1)),
            resume,
            gone_since: Default::default(),
        };
        let c = control.clone();
        std::thread::spawn(move || c.read_loop(reader));
        Ok((control, spec))
    }

    fn read_loop(&self, mut reader: BufReader<File>) {
        loop {
            match read_msg(&mut reader) {
                Ok(Some(HostMsg::ExecReply { id, allow })) => {
                    if let Some((tx, _)) = self.pending.lock().unwrap().remove(&id) {
                        let _ = tx.send(allow);
                    }
                }
                Ok(Some(HostMsg::Shutdown)) => sys::power_off(),
                Ok(Some(HostMsg::Init { .. } | HostMsg::Resumed)) => {}
                Ok(None) | Err(_) => match &self.resume {
                    Some(token) => reader = self.reconnect(token),
                    None => {
                        eprintln!("zapper-agent: control channel lost");
                        sys::power_off();
                    }
                },
            }
        }
    }

    /// Connects to the host again until it answers (it may be restarting),
    /// sends the resume token and the persistent terminals, and asks again
    /// the exec checks waiting. Meanwhile, nothing that needs the host
    /// happens: the guest's network goes nowhere, and execs wait.
    fn reconnect(&self, token: &str) -> BufReader<File> {
        self.gone_since
            .lock()
            .unwrap()
            .get_or_insert_with(Instant::now);
        eprintln!("zapper-agent: control channel lost; connecting again");
        let mut wait = Duration::from_millis(200);
        loop {
            std::thread::sleep(wait);
            wait = (wait * 2).min(Duration::from_secs(2));
            let Ok(stream) = sys::vsock_connect(HOST_CID, CONTROL_PORT) else {
                continue;
            };
            let (Ok(reader), Ok(writer)) = (stream.try_clone(), stream.try_clone()) else {
                continue;
            };
            *self.writer.lock().unwrap() = writer;
            self.send(&GuestMsg::Resume {
                token: token.to_string(),
                terminals: crate::holder::ids(),
            });
            // Taken only when the host says so: one that closes it (a host
            // not adopting the session yet, or one refusing it) isn't, and
            // the host stays gone, its 60 seconds running on.
            let fd = std::os::fd::AsRawFd::as_raw_fd(&reader);
            sys::set_recv_timeout(fd, 5);
            let mut reader = BufReader::new(reader);
            if !matches!(read_msg(&mut reader), Ok(Some(HostMsg::Resumed))) {
                continue;
            }
            sys::set_recv_timeout(fd, 0);
            let waiting: Vec<GuestMsg> = self
                .pending
                .lock()
                .unwrap()
                .values()
                .map(|(_, m)| m.clone())
                .collect();
            for m in &waiting {
                self.send(m);
            }
            *self.gone_since.lock().unwrap() = None;
            return reader;
        }
    }

    pub fn send(&self, msg: &GuestMsg) {
        let mut line = serde_json::to_vec(msg).expect("message serialises");
        line.push(b'\n');
        if let Err(e) = self.writer.lock().unwrap().write_all(&line) {
            eprintln!("zapper-agent: control write: {e}");
        }
    }

    pub fn event(&self, event: GuestEvent) {
        self.send(&GuestMsg::Event { event });
    }

    /// Asks the host whether an exec may proceed. Fails closed: refused if
    /// no answer comes, or the host is gone for longer than `HOST_GONE`.
    pub fn check_exec(&self, pid: i32, uid: u32, req: ExecRequest) -> bool {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = channel();
        let msg = GuestMsg::ExecCheck { id, pid, uid, req };
        self.pending.lock().unwrap().insert(id, (tx, msg.clone()));
        self.send(&msg);
        // The host bounds `ask` waits itself; this guards a lost reply, and
        // a host that is gone.
        let asked = Instant::now();
        let allow = loop {
            match rx.recv_timeout(Duration::from_secs(1)) {
                Ok(allow) => break allow,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break false,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    let gone = self
                        .gone_since
                        .lock()
                        .unwrap()
                        .is_some_and(|t| t.elapsed() > HOST_GONE);
                    if gone || asked.elapsed() > Duration::from_secs(600) {
                        break false;
                    }
                }
            }
        };
        self.pending.lock().unwrap().remove(&id);
        allow
    }
}

fn read_msg(reader: &mut BufReader<File>) -> Result<Option<HostMsg>> {
    let mut line = Vec::new();
    let n = Read::take(&mut *reader, MAX_LINE as u64 + 1).read_until(b'\n', &mut line)?;
    if n == 0 {
        return Ok(None);
    }
    if n > MAX_LINE {
        bail!("control line too long");
    }
    Ok(Some(serde_json::from_slice(&line)?))
}
