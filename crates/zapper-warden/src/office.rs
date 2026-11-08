//! The office's process: the one command the warden runs in the office's
//! sandbox (`office.command`, zapperclaw), with the office's environment
//! and the board's token, started again whenever it stops. Its output goes
//! to the warden's log.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::json;
use zapper_policy::proto::Frame;

use crate::audit::Audit;
use crate::gateway::Gateway;
use crate::names;
use crate::profiles::Prepared;

/// Where the board's token is kept, in the warden's state directory.
pub const BOARD_TOKEN: &str = "board-token";

/// The board's token: the one in `dir`, or a new one kept there (0600).
pub fn board_token(dir: &Path) -> Result<String> {
    let path = dir.join(BOARD_TOKEN);
    if let Ok(t) = std::fs::read_to_string(&path) {
        let t = t.trim().to_string();
        if t.len() >= 32 {
            return Ok(t);
        }
    }
    let t: String = rand::random::<[u8; 32]>()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)
        .and_then(|mut f| f.write_all(t.as_bytes()))
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(t)
}

/// The office command's environment: its template's, the board's token,
/// and `HOME` (where zapperclaw finds its config and keeps its state) if
/// the template doesn't say.
pub fn env(office: &Prepared, token: &str) -> BTreeMap<String, String> {
    let mut env = office.setup.env.clone();
    env.entry("HOME".into())
        .or_insert_with(|| format!("/home/{}", office.profile.agent.user));
    env.insert("ZAPPERCLAW_BOARD_TOKEN".into(), token.into());
    env
}

/// Runs `command` in the office's sandbox until the task is dropped,
/// starting it again (after a growing pause) whenever it stops.
pub async fn keep_running(
    gw: Arc<dyn Gateway>,
    office: Prepared,
    command: Vec<String>,
    token: String,
    audit: Arc<Audit>,
) {
    if command.is_empty() {
        std::future::pending::<()>().await;
    }
    let env = env(&office, &token);
    let workdir = office.profile.agent.workdir.clone();
    let mut pause = 2;
    loop {
        let started = std::time::Instant::now();
        match gw
            .exec(names::OFFICE, command.clone(), env.clone(), workdir.clone())
            .await
        {
            Ok(mut stream) => {
                audit.record("office_started", json!({ "command": command }));
                let code = pump(&mut stream).await;
                audit.record("office_stopped", json!({ "exit_code": code }));
                tracing::warn!("the office's process stopped (exit code {code:?})");
            }
            Err(e) => tracing::warn!("starting the office's process: {e:#}"),
        }
        if started.elapsed() > Duration::from_secs(60) {
            pause = 2;
        }
        tokio::time::sleep(Duration::from_secs(pause)).await;
        pause = (pause * 2).min(60);
    }
}

/// Logs the process's output until it exits: its exit code, if it said.
async fn pump(stream: &mut Box<dyn zapper_driver::process::DriveStream>) -> Option<i32> {
    let mut out = Vec::new();
    let log = |buf: &mut Vec<u8>, bytes: &[u8]| {
        buf.extend_from_slice(bytes);
        while let Some(i) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=i).collect();
            let line = String::from_utf8_lossy(&line[..i]);
            tracing::info!(target: "office", "{line}");
        }
        if buf.len() > 64 * 1024 {
            tracing::info!(target: "office", "{}", String::from_utf8_lossy(buf));
            buf.clear();
        }
    };
    loop {
        match zapper_driver::read_frame(stream).await {
            Ok(Some(Frame::Data(d))) | Ok(Some(Frame::Stderr(d))) => log(&mut out, &d),
            Ok(Some(Frame::Exit(code))) => return Some(code),
            Ok(Some(_)) => {}
            Ok(None) => return None,
            Err(e) => {
                tracing::warn!("the office's process: {e}");
                return None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_board_token_is_made_once_and_kept_private() {
        let dir = std::env::temp_dir().join(format!("zc-token-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = board_token(&dir).unwrap();
        assert_eq!(a.len(), 64);
        assert_eq!(board_token(&dir).unwrap(), a, "kept");
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.join(BOARD_TOKEN))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0);
        let _ = std::fs::remove_dir_all(dir);
    }
}
