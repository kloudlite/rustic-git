use axum::{http::StatusCode, response::{IntoResponse, Response}};

use kloudlite_git_core::httpx::max_body;

/// Cap on the decompressed size of a gzipped request body — bounds the zlib-bomb amplification
/// on top of the wire-size limit. 8x the body cap.
// ponytail: derived from the boot-time env cap, not the live `max_body` setting — the gzip layer
// is built once at router construction. Rebuild it from `app.central` if a live change matters.
pub(crate) fn max_decompressed() -> u64 {
    (max_body() as u64) * 8
}

pub(crate) fn internal(e: crate::Error) -> Response {
    tracing::error!(error = %e, "internal error");
    (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
}

/// A request the client sent us that we will never satisfy, as opposed to something broken on our
/// end. Distinguished from a bare `crate::err` so `info_refs` can answer 400, not 500, without
/// masking a genuine internal failure the same way.
#[derive(Debug)]
pub(crate) struct ClientError(pub(crate) String);
impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for ClientError {}

pub(crate) fn client_err(msg: impl Into<String>) -> crate::Error {
    ClientError(msg.into()).into()
}

pub(crate) fn bad_request(e: &crate::Error) -> Response {
    (StatusCode::BAD_REQUEST, e.to_string()).into_response()
}

pub(crate) fn fenced_elsewhere() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        "repository is owned by another node; retry",
    )
        .into_response()
}
