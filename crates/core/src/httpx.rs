/// Identity established by a *peer*. `None` on the public listener, always.
#[derive(Clone)]
pub struct Trusted(pub Option<String>);

/// Cap on a single request body (compressed bytes on the wire). Axum enforces this in the
/// extractor, BEFORE the handler runs, so an unauthenticated client cannot make the server
/// buffer more than this. Override with RUSTIC_GIT_MAX_BODY (bytes).
pub fn max_body() -> usize {
    std::env::var("RUSTIC_GIT_MAX_BODY")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2 * 1024 * 1024 * 1024) // 2 GiB
}
