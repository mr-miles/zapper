//! Whole-guest usage for the host: load, memory in use, processes and the
//! root disk, every few seconds.

use std::time::Duration;

use zapper_proto::proto::GuestEvent;

use crate::control::Control;

const EVERY: Duration = Duration::from_secs(5);

/// The 1-minute load and the number of processes, from `/proc/loadavg`
/// ("0.42 0.30 0.20 2/131 4512").
fn loadavg(s: &str) -> Option<(f64, u32)> {
    let mut f = s.split_whitespace();
    let load1 = f.next()?.parse().ok()?;
    let procs = f.nth(2)?.split_once('/')?.1.parse().ok()?;
    Some((load1, procs))
}

/// Memory in use, in KiB: `MemTotal - MemAvailable` from `/proc/meminfo`.
fn mem_used_kib(s: &str) -> Option<u64> {
    let field = |name: &str| {
        s.lines().find_map(|l| {
            l.strip_prefix(name)?
                .trim()
                .trim_end_matches("kB")
                .trim()
                .parse::<u64>()
                .ok()
        })
    };
    Some(field("MemTotal:")?.saturating_sub(field("MemAvailable:")?))
}

/// Used space on the root disk, in KiB.
fn disk_used_kib() -> Option<u64> {
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c"/".as_ptr(), &mut st) } != 0 {
        return None;
    }
    Some((st.f_blocks.saturating_sub(st.f_bfree) as u64).saturating_mul(st.f_frsize as u64) / 1024)
}

fn sample() -> Option<GuestEvent> {
    let (load1, procs) = loadavg(&std::fs::read_to_string("/proc/loadavg").ok()?)?;
    let mem_used_kib = mem_used_kib(&std::fs::read_to_string("/proc/meminfo").ok()?)?;
    Some(GuestEvent::Usage {
        load1,
        mem_used_kib,
        procs,
        disk_used_kib: disk_used_kib().unwrap_or(0),
    })
}

pub fn start(control: Control) {
    std::thread::spawn(move || loop {
        if let Some(event) = sample() {
            control.event(event);
        }
        std::thread::sleep(EVERY);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_proc() {
        assert_eq!(loadavg("0.42 0.30 0.20 2/131 4512\n"), Some((0.42, 131)));
        assert_eq!(loadavg("garbage"), None);
        let mem = "MemTotal:        2014520 kB\nMemFree:         1500000 kB\nMemAvailable:    1800000 kB\n";
        assert_eq!(mem_used_kib(mem), Some(214520));
        assert_eq!(mem_used_kib("MemTotal: 10 kB\n"), None);
    }
}
