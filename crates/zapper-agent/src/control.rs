//! The control channel to the host.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use zapper_proto::proto::{
    GuestEvent, GuestMsg, GuestSpec, HostMsg, CONTROL_PORT, HOST_CID, MAX_LINE,
};
use zapper_proto::ExecRequest;

use crate::sys;

#[derive(Clone)]
pub struct Control {
    writer: Arc<Mutex<File>>,
    pending: Arc<Mutex<HashMap<u64, Sender<bool>>>>,
    next_id: Arc<AtomicU64>,
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
        let spec = match read_msg(&mut reader)? {
            Some(HostMsg::Init { spec }) => *spec,
            other => bail!("expected init, got {other:?}"),
        };
        let control = Control {
            writer: Arc::new(Mutex::new(stream)),
            pending: Default::default(),
            next_id: Arc::new(AtomicU64::new(1)),
        };
        let c = control.clone();
        std::thread::spawn(move || c.read_loop(reader));
        Ok((control, spec))
    }

    fn read_loop(&self, mut reader: BufReader<File>) {
        loop {
            match read_msg(&mut reader) {
                Ok(Some(HostMsg::ExecReply { id, allow })) => {
                    if let Some(tx) = self.pending.lock().unwrap().remove(&id) {
                        let _ = tx.send(allow);
                    }
                }
                Ok(Some(HostMsg::Shutdown)) | Ok(None) => sys::power_off(),
                Ok(Some(HostMsg::Init { .. })) => {}
                Err(e) => {
                    eprintln!("zapper-agent: control: {e:#}");
                    sys::power_off();
                }
            }
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

    /// Asks the host whether an exec may proceed. Fails closed.
    pub fn check_exec(&self, pid: i32, uid: u32, req: ExecRequest) -> bool {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = channel();
        self.pending.lock().unwrap().insert(id, tx);
        self.send(&GuestMsg::ExecCheck { id, pid, uid, req });
        // The host bounds `ask` waits itself; this only guards a lost reply.
        let allow = rx.recv_timeout(Duration::from_secs(600)).unwrap_or(false);
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
