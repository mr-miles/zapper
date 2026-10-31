//! What a persistent session keeps beside its record, to be listed again
//! after the daemon restarts.

use anyhow::Result;

use super::{Saved, Session};

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
    };
    crate::util::write_private(&session.dir.saved(), &serde_json::to_vec_pretty(&saved)?)
}
