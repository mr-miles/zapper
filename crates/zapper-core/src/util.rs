//! Helpers the core's clients share with it: ids and time, files only the
//! user reads, the terminal channel's frames, the warning for listening
//! beyond loopback, and the check that a config's firms have an office.

pub use crate::daemon::session::read_frame;
pub use crate::daemon::{exposed_warning, now, office_config, random_id, write_private};
