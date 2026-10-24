//! `zapper-keeper serve`, as the daemon meets it: appends are taken only
//! from the configured writer's uid, and an append is acknowledged only
//! once its line is in the file.

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::Duration;

use zapper_keeper::Client;

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("zapper-keeper-serve-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A running keeper, stopped when dropped (a failed test included).
struct Kept(Child);

impl Drop for Kept {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A keeper keeping `dir/record` for `uid`, on `dir/keeper.sock`, once it
/// listens.
fn keeper(dir: &Path, uid: u32) -> Kept {
    let socket = dir.join("keeper.sock");
    let _ = std::fs::remove_file(&socket);
    let child = Command::new(env!("CARGO_BIN_EXE_zapper-keeper"))
        .args(["serve", "--socket"])
        .arg(&socket)
        .arg("--dir")
        .arg(dir.join("record"))
        .arg("--key")
        .arg(dir.join("key"))
        .args(["--writer-uid", &uid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let kept = Kept(child);
    for _ in 0..500 {
        if std::os::unix::net::UnixStream::connect(&socket).is_ok() {
            return kept;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("the keeper didn't listen");
}

fn me() -> u32 {
    unsafe { libc::getuid() }
}

/// A connection from another uid than the writer's is closed unanswered,
/// whatever it asks; the writer's is served.
#[test]
fn only_the_writers_uid_appends() {
    let d = tmp("uid");
    let other = keeper(&d, me() + 1);
    let c = Client::new(&d.join("keeper.sock"));
    assert!(c.hello().is_err(), "another uid's connection is refused");
    assert!(c.append("firms/acme/journal.jsonl", b"{}").is_err());
    assert!(
        !d.join("record/firms/acme/journal.jsonl").exists(),
        "nothing written"
    );
    drop(other);

    let mine = keeper(&d, me());
    let c = Client::new(&d.join("keeper.sock"));
    assert!(c.hello().is_ok());
    assert_eq!(c.append("firms/acme/journal.jsonl", b"{}").unwrap(), 1);
    drop(mine);
    let _ = std::fs::remove_dir_all(&d);
}

/// When an append is acknowledged its line is already in the file: a
/// reader that doesn't ask the keeper sees each one as soon as it's acked.
#[test]
fn an_append_is_in_the_file_when_acknowledged() {
    let d = tmp("flush");
    let k = keeper(&d, me());
    let c = Client::new(&d.join("keeper.sock"));
    let stream = "firms/acme/runs/r1/journal.jsonl";
    let file = d.join("record").join(stream);
    for i in 1..=50u64 {
        let line = format!("{{\"n\":{i}}}");
        assert_eq!(c.append(stream, line.as_bytes()).unwrap(), i);
        let text = std::fs::read_to_string(&file).unwrap();
        assert_eq!(text.lines().last(), Some(line.as_str()), "line {i}");
        assert_eq!(text.lines().count() as u64, i);
    }
    drop(k);
    let _ = std::fs::remove_dir_all(&d);
}
