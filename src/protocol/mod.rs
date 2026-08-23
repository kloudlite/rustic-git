pub mod receive;
pub mod upload;

pub const AGENT: &str = "agent=rustic-git/0.1";

/// Run a future to completion from sync code inside `spawn_blocking`.
///
/// `block_in_place` turns the CURRENT worker thread into a blocking one, so this must only run
/// on a multi-thread runtime (`flavor = "multi_thread"` in every test that reaches it) and never
/// from a `LocalSet`; on a current-thread runtime it panics.
pub fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(f))
}

/// "owner/name.git" or "/owner/name" → (owner, name)
pub fn parse_repo_path(p: &str) -> Option<(String, String)> {
    let (o, n) = p.trim_start_matches('/').split_once('/')?;
    parse_repo_pair(o, n)
}

/// The pair form of `parse_repo_path`, for callers that already hold the two segments —
/// they were formatting them into one string just to split it again.
pub fn parse_repo_pair(owner: &str, name: &str) -> Option<(String, String)> {
    let n = name.strip_suffix(".git").unwrap_or(name);
    if !crate::store::valid_segment(owner) || !crate::store::valid_segment(n) {
        return None;
    }
    Some((owner.to_string(), n.to_string()))
}
