//! What kind of failure an error is, carried inside an `anyhow::Error`, for
//! those who answer for it: the API's status codes come from these, not
//! from the error's words.

use std::fmt;

/// What kind of failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// What was named isn't there (a session, a rule, an approval).
    NotFound,
    /// It can't be done as things are, but could be (a start whose disks
    /// changed, until the change is accepted).
    Conflict,
}

/// An error of a kind: its words, and the kind.
#[derive(Debug)]
struct Kinded {
    kind: ErrorKind,
    msg: String,
}

impl fmt::Display for Kinded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.msg)
    }
}

impl std::error::Error for Kinded {}

/// An error of `kind` saying `msg`.
pub fn error(kind: ErrorKind, msg: impl fmt::Display) -> anyhow::Error {
    anyhow::Error::new(Kinded {
        kind,
        msg: msg.to_string(),
    })
}

/// What was named isn't there: a [`ErrorKind::NotFound`].
pub fn not_found(msg: impl fmt::Display) -> anyhow::Error {
    error(ErrorKind::NotFound, msg)
}

/// It can't be done as things are: a [`ErrorKind::Conflict`].
pub fn conflict(msg: impl fmt::Display) -> anyhow::Error {
    error(ErrorKind::Conflict, msg)
}

/// The kind of `e`, if it has one (anywhere in its chain, so context added
/// on the way keeps it).
pub fn kind_of(e: &anyhow::Error) -> Option<ErrorKind> {
    e.chain()
        .find_map(|c| c.downcast_ref::<Kinded>())
        .map(|k| k.kind)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context as _;

    #[test]
    fn a_kind_survives_context_and_its_words_are_kept() {
        let e = error(ErrorKind::NotFound, "no session x");
        assert_eq!(kind_of(&e), Some(ErrorKind::NotFound));
        assert_eq!(e.to_string(), "no session x");
        let e = Err::<(), _>(e).context("starting it").unwrap_err();
        assert_eq!(kind_of(&e), Some(ErrorKind::NotFound));
        assert_eq!(format!("{e:#}"), "starting it: no session x");
        assert_eq!(kind_of(&anyhow::anyhow!("no kind")), None);
    }
}
