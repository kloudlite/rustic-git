pub mod receive;
pub mod upload;

pub const AGENT: &str = "agent=rustic-git/0.1";

/// Run a future to completion from sync code inside spawn_blocking.
pub fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(f))
}

/// "owner/name.git" or "/owner/name" → (owner, name)
pub fn parse_repo_path(p: &str) -> Option<(String, String)> {
    let (o, n) = p.trim_start_matches('/').split_once('/')?;
    let n = n.strip_suffix(".git").unwrap_or(n);
    if !crate::store::valid_segment(o) || !crate::store::valid_segment(n) {
        return None;
    }
    Some((o.to_string(), n.to_string()))
}
