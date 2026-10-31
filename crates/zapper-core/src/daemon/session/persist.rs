//! What a persistent session keeps beside its record, to be listed again
//! after the daemon restarts.

use std::path::PathBuf;

use anyhow::Result;

use super::{Saved, Session, SessionDir};

/// The files a persistent session keeps beside its record, to be listed
/// again, started again or adopted: the one list of them, so what is
/// written and what goes with the session agree. Its record (`audit.jsonl`,
/// recordings, logs) isn't among them: it stays.
pub(super) fn kept_files(dir: &SessionDir) -> [PathBuf; 6] {
    [
        dir.saved(),
        dir.ca(),
        dir.dns(),
        dir.resume_token(),
        dir.vmm(),
        dir.conversation(),
    ]
}

/// Removes what a persistent session kept, its disks among it, so no
/// daemon lists it again. Its record stays.
pub(super) fn forget(dir: &SessionDir) {
    let _ = std::fs::remove_dir_all(dir.vm());
    for kept in kept_files(dir) {
        let _ = std::fs::remove_file(kept);
    }
}

/// Writes a persistent session's `session.json`, its own rules as they are.
pub(super) fn save(session: &Session) -> Result<()> {
    let _one_at_a_time = session.saving.lock().unwrap();
    let saved = Saved {
        id: session.id.clone(),
        profile: session.profile.clone(),
        created: session.created,
        workdir: session.workdir.clone(),
        resolved: session.resolved.clone(),
        labels: session.labels.clone(),
        rules: session.own_policy(),
        files: session.extra_files.clone(),
        forwards: session.forwards.clone(),
        driven: session.driver.is_some(),
        ssh: session.ssh_enabled(),
        ssh_terminal: session.ssh_terminal(),
    };
    crate::util::write_private(&session.dir.saved(), &serde_json::to_vec_pretty(&saved)?)
}
