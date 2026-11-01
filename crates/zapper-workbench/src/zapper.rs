//! zapper's API, on its unix socket: the workbench is its operator, and the
//! one client with clients of its own. The client itself is shared with the
//! office in `zapper-client`.

pub use zapper_client::{socket_path, url_encode, Answer, Zapper};
