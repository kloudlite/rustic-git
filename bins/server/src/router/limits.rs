use axum::{http::StatusCode, response::{IntoResponse, Response}};

use kloudlite_core::httpx::max_body;

/// Cap on the decompressed size of a gzipped request body — bounds the zlib-bomb amplification
/// on top of the wire-size limit. 8x the body cap.
// ponytail: derived from the boot-time env cap, not the live `max_body` setting — the gzip layer
// is built once at router construction. Rebuild it from `app.central` if a live change matters.
pub(crate) fn max_decompressed() -> u64 {
    (max_body() as u64) * 8
}

pub(crate) fn internal(e: crate::Error) -> Response {
    // A key routed to no owner: nothing exists there, so a read that skipped its probe is a 404.
    if kloudlite_storage::pool::is_unowned_err(&e) {
        // Debug, not silence: the 404 is right for a name nothing owns, but it is also how a
        // scoping bug (a key wrongly inside `pool::unowned`) would present — a 404 on a repo that
        // does exist, with nothing in the log to tell the two apart.
        tracing::debug!(error = %e, "request.unowned");
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    tracing::error!(error = %e, "request.failed");
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A read that skipped its probe and hit the pool's refusal for a key routed to no owner is a
    /// 404, not a 500: browse and git callers see "no such repo", which is what routing decided.
    /// Without the mapping this is the generic 500.
    #[test]
    fn an_unowned_key_is_not_found_not_an_internal_error() {
        let e: crate::Error =
            kloudlite_storage::pool::UnownedError { repo: "alice/web".into() }.into();
        assert_eq!(internal(e).status(), StatusCode::NOT_FOUND);
        // A genuine failure still reports as one.
        assert_eq!(internal(kloudlite_core::err("boom")).status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}
