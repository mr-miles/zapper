//! What the warden calls things at the gateway, and which names it takes.
//!
//! Its templates and sandboxes have prefixes of their own, so none can be
//! taken for another: the office's template and sandbox are `zc-office`, a
//! worker profile's template `zcp-<profile>`, a worker's sandbox
//! `zcw-<name>`. Every one carries `zapperclaw: 1`, and a worker's its name
//! and profile, so the gateway's operator sees what each is.

use anyhow::{bail, Result};
use zapper_openshell::Labels;

/// The office's sandbox, and the template it is made from.
pub const OFFICE: &str = "zc-office";
/// A worker profile's template: `zcp-<profile>`.
pub const PROFILE_PREFIX: &str = "zcp-";
/// A worker's sandbox: `zcw-<name>`.
pub const WORKER_PREFIX: &str = "zcw-";

/// Every label key the warden sets.
pub const LABEL: &str = "zapperclaw";
pub const LABEL_ROLE: &str = "zapperclaw-role";
pub const LABEL_WORKER: &str = "zapperclaw-worker";
pub const LABEL_PROFILE: &str = "zapperclaw-profile";

/// The longest a worker's or a profile's name may be.
const MAX_NAME: usize = 40;

/// Checks `name` as a worker's or a profile's: a lowercase letter, then
/// lowercase letters, digits and single hyphens, at most 40, not ending in
/// a hyphen. So it is a label value and part of a sandbox name as it is.
pub fn check(what: &str, name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= MAX_NAME
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !name.ends_with('-')
        && !name.contains("--");
    if !ok {
        bail!(
            "{what} {name:?}: a lowercase letter, then lowercase letters, digits and single hyphens, at most {MAX_NAME}"
        );
    }
    Ok(())
}

/// Worker profile `profile`'s template.
pub fn profile_template(profile: &str) -> String {
    format!("{PROFILE_PREFIX}{profile}")
}

/// Worker `name`'s sandbox.
pub fn worker_sandbox(name: &str) -> String {
    format!("{WORKER_PREFIX}{name}")
}

/// The office sandbox's labels (and its template's).
pub fn office_labels() -> Labels {
    Labels::from([
        (LABEL.to_string(), "1".to_string()),
        (LABEL_ROLE.to_string(), "office".to_string()),
    ])
}

/// Worker profile `profile`'s template's labels.
pub fn profile_labels(profile: &str) -> Labels {
    Labels::from([
        (LABEL.to_string(), "1".to_string()),
        (LABEL_ROLE.to_string(), "worker".to_string()),
        (LABEL_PROFILE.to_string(), profile.to_string()),
    ])
}

/// Worker `name`'s sandbox's labels, of profile `profile`.
pub fn worker_labels(name: &str, profile: &str) -> Labels {
    let mut l = profile_labels(profile);
    l.insert(LABEL_WORKER.to_string(), name.to_string());
    l
}

/// Checks a one-time ticket the office gives for an agent's stream: 32 to
/// 128 lowercase hex digits.
pub fn check_ticket(ticket: &str) -> Result<()> {
    let ok = (32..=128).contains(&ticket.len())
        && ticket
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c));
    if !ok {
        bail!("ticket: 32 to 128 lowercase hex digits");
    }
    Ok(())
}

/// Checks a conversation id to resume (Claude Code's and pi's are UUIDs or
/// like them): 1 to 128 of letters, digits, `-` and `_`, so it is one
/// argument and nothing else.
pub fn check_resume(id: &str) -> Result<()> {
    let ok = (1..=128).contains(&id.len())
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        && !id.starts_with('-');
    if !ok {
        bail!("resume: 1 to 128 letters, digits, '-' and '_', not starting with '-'");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_one_label_and_one_sandbox_name() {
        for ok in ["ada", "ops-2", "a", "research-and-dev"] {
            assert!(check("worker", ok).is_ok(), "{ok}");
        }
        for bad in [
            "", "Ada", "2ada", "ada-", "a--b", "a_b", "a.b", "a b", "../x",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ] {
            assert!(check("worker", bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_prefixes_keep_templates_and_sandboxes_apart() {
        assert_eq!(profile_template("office"), "zcp-office");
        assert_eq!(worker_sandbox("office"), "zcw-office");
        assert_ne!(profile_template("office"), OFFICE);
        assert_ne!(worker_sandbox("office"), OFFICE);
        let l = worker_labels("ada", "claude");
        assert_eq!(l[LABEL_WORKER], "ada");
        assert_eq!(l[LABEL_PROFILE], "claude");
        assert_eq!(l[LABEL], "1");
    }

    #[test]
    fn tickets_and_resume_ids_are_one_plain_argument() {
        assert!(check_ticket(&"a1".repeat(16)).is_ok());
        assert!(check_ticket("abc").is_err(), "too short");
        assert!(check_ticket(&"A1".repeat(16)).is_err(), "uppercase");
        assert!(check_ticket(&format!("{}\nX", "a".repeat(32))).is_err());
        assert!(check_resume("3f2a9c4e-1b2d-4c5e-8f90-123456789abc").is_ok());
        assert!(check_resume("--dangerously-skip-permissions").is_err());
        assert!(check_resume("a b").is_err());
        assert!(check_resume("").is_err());
        assert!(check_resume(&"a".repeat(129)).is_err());
    }
}
