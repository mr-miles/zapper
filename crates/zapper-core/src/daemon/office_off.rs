//! The Office, left out of the build (`--no-default-features`): its routes,
//! which serve nothing.

pub mod http {
    use axum::Router;

    use crate::Core;

    pub fn read_routes() -> Router<Core> {
        Router::new()
    }

    pub fn board_routes() -> Router<Core> {
        Router::new()
    }
}
