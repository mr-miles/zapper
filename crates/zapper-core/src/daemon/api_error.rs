//! The error the API's routes answer with: the office's too, while it lives
//! in the core.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

pub struct ApiError(pub StatusCode, pub String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        use crate::errors::{kind_of, ErrorKind};
        let msg = format!("{e:#}");
        let code = match kind_of(&e) {
            Some(ErrorKind::NotFound) => StatusCode::NOT_FOUND,
            Some(ErrorKind::Conflict) => StatusCode::CONFLICT,
            // Errors not given a kind yet: one that names what isn't there.
            None if msg.starts_with("no ") => StatusCode::NOT_FOUND,
            None => StatusCode::BAD_REQUEST,
        };
        ApiError(code, msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errors::{error, ErrorKind};

    /// A kind sets the status; an error without one is guessed at as before.
    #[test]
    fn a_kind_sets_the_status() {
        let status = |e: anyhow::Error| ApiError::from(e).0;
        assert_eq!(
            status(error(ErrorKind::NotFound, "gone")),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            status(error(ErrorKind::Conflict, "no: changed")),
            StatusCode::CONFLICT
        );
        assert_eq!(
            status(anyhow::anyhow!("no session x")),
            StatusCode::NOT_FOUND
        );
        assert_eq!(status(anyhow::anyhow!("bad")), StatusCode::BAD_REQUEST);
    }
}
