//! Helpers the core's clients share with it: ids and time, files only the
//! user reads, a config with its inline secrets hidden, the terminal
//! channel's frames, and the warning for listening beyond loopback.

pub use crate::daemon::session::read_frame;
pub use crate::daemon::{exposed_warning, now, random_id, redact, write_private, ID_CHARS};
