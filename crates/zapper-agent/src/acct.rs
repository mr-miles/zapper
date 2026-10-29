//! Process exits from kernel process accounting (`acct(2)`, v3 records).

use std::fs::File;
use std::io::Read;
use std::time::Duration;

use zapper_proto::proto::GuestEvent;

use crate::control::Control;
use crate::sys;

const PACCT: &str = "/run/zapper/pacct";
const RECORD: usize = 64;
/// Units of the time fields.
const AHZ: u64 = 100;

/// Decodes a `comp_t`: 13-bit mantissa, 3-bit base-8 exponent.
fn comp(v: u16) -> u64 {
    ((v & 0x1fff) as u64) << (3 * (v >> 13) as u64)
}

fn decode(r: &[u8; RECORD]) -> Option<GuestEvent> {
    let u32_at = |i: usize| u32::from_ne_bytes(r[i..i + 4].try_into().unwrap());
    let u16_at = |i: usize| u16::from_ne_bytes(r[i..i + 2].try_into().unwrap());
    if r[1] != 3 {
        return None;
    }
    let status = u32_at(4) as i32;
    let (exit_code, signal) = if status & 0x7f == 0 {
        ((status >> 8) & 0xff, None)
    } else {
        (128 + (status & 0x7f), Some(status & 0x7f))
    };
    let etime = f32::from_ne_bytes(r[28..32].try_into().unwrap());
    let comm = &r[48..64];
    let comm = String::from_utf8_lossy(&comm[..comm.iter().position(|&b| b == 0).unwrap_or(16)])
        .into_owned();
    Some(GuestEvent::Exit {
        pid: u32_at(16),
        ppid: u32_at(20),
        uid: u32_at(8),
        comm,
        exit_code,
        signal,
        elapsed_ms: (etime as f64 * 1000.0 / AHZ as f64) as u64,
        user_ms: comp(u16_at(32)) * 1000 / AHZ,
        sys_ms: comp(u16_at(34)) * 1000 / AHZ,
        mem_kib: comp(u16_at(36)),
        io_bytes: comp(u16_at(38)),
    })
}

/// Turns accounting on and forwards every record to the host.
pub fn start(control: Control) {
    if let Err(e) = std::fs::write(PACCT, b"").and_then(|_| sys::acct(PACCT)) {
        eprintln!("zapper-agent: process accounting unavailable: {e}");
        return;
    }
    let me = std::process::id();
    std::thread::spawn(move || {
        let Ok(mut f) = File::open(PACCT) else { return };
        let mut buf = Vec::new();
        loop {
            let mut chunk = [0u8; 4096];
            match f.read(&mut chunk) {
                Ok(0) | Err(_) => std::thread::sleep(Duration::from_millis(200)),
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
            while buf.len() >= RECORD {
                let rec: [u8; RECORD] = buf[..RECORD].try_into().unwrap();
                buf.drain(..RECORD);
                match decode(&rec) {
                    Some(GuestEvent::Exit { pid, .. }) if pid == me => {}
                    Some(event) => control.event(event),
                    None => {}
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_comp_t() {
        assert_eq!(comp(5), 5);
        assert_eq!(comp((1 << 13) | 3), 24);
    }
}
