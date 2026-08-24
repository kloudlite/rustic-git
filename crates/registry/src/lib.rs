//! An OCI Distribution v1.1 registry, served by the git nodes.
//!
//! An image is `{owner}/{name}` in a namespace of its own: no git repo is required, and a repo of
//! the same name grants no claim on it. What makes the two safe to serve from one process is this
//! module's key derivation — see `routing_key`.

// `Result<T, axum::Response>` is the handler idiom here: the Err is an early-return response,
// unwrapped exactly once per request by `?`. Boxing it to please the size lint would add an
// allocation per refusal for no measurable gain.
#![allow(clippy::result_large_err)]

pub(crate) use rustic_git_core::{err, hex, jwt, httpx::Trusted};
pub(crate) use rustic_git_core::httpx as httpauth;
pub(crate) use rustic_git_storage::store as dbstore;
pub(crate) use rustic_git_storage::{index, ownership, pool};
pub(crate) use rustic_git_app::App;
pub(crate) use rustic_git_core::{Error, Result};

/// The tails that make a `/v2/{owner}/{name}/...` path an IMAGE path (one that must be routed to
/// the node holding that image's database). A path whose tail is missing here is not a registry
/// endpoint, is not routable, and is refused before any handler sees it — exactly as `BROWSE_TAILS`
/// does for the browse API.
const IMAGE_TAILS: [&str; 4] = ["blobs", "manifests", "tags", "referrers"];

/// The `/v2` paths that name no image. They are answered locally by whichever node receives them:
/// `/v2/` and `/v2/token` touch no database, and `_catalog` is an object-store listing.
pub const LOCAL_V2: [&str; 3] = ["", "token", "_catalog"];

pub mod auth;
pub mod blobs;
pub mod gc;
pub mod manifests;
pub mod referrers;
pub mod routes;
pub mod store;
pub mod uploads;
pub use store::Digest;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

/// The spec's error body. Every `/v2` refusal goes through here: a client that gets a bare string
/// where it expects this JSON reports a confusing error and retries nothing.
pub fn oci_err(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        serde_json::json!({"errors": [{"code": code, "message": message, "detail": null}]})
            .to_string(),
    )
        .into_response()
}

/// 500s on `/v2` still owe the client the OCI envelope, not the plain-text body `http::internal`
/// gives git handlers. Detail stays server-side (matches `internal`'s convention) — only the code
/// and a generic message cross the wire.
pub fn oci_internal(e: crate::Error) -> Response {
    eprintln!("internal error: {e}"); // ponytail: eprintln; swap for a logger when one exists
    oci_err(StatusCode::INTERNAL_SERVER_ERROR, "UNKNOWN", "internal error")
}

pub fn is_v2_path(path: &str) -> bool {
    let p = path.trim_start_matches('/');
    p == "v2" || p.starts_with("v2/")
}

/// `Some((owner, name))` when the path names an image. Deliberately strict: the name is ONE
/// segment, so `/v2/a/b/c/manifests/x` is None rather than being folded into some other image.
pub fn image_route(path: &str) -> Option<(&str, &str)> {
    let mut it = path.trim_start_matches('/').strip_prefix("v2/")?.split('/');
    let (owner, name, tail) = (it.next()?, it.next()?, it.next()?);
    if !IMAGE_TAILS.contains(&tail) {
        return None;
    }
    (crate::dbstore::valid_owner(owner) && crate::dbstore::valid_segment(name))
        .then_some((owner, name))
}

/// The ownership-map key for an image.
///
/// `img/` is a prefix no git route can produce: `repo_of` emits it only for `/v2/` paths, and
/// `img` is a reserved owner name so no repo key begins with it either. `lib.rs` turns a key back
/// into pool coordinates with `split_once('/')`, which yields `("img", "{owner}/{name}")` — the
/// same pair `pool_coords` returns, so claim, renew, evict, and release need no knowledge of
/// images at all.
pub fn routing_key(owner: &str, name: &str) -> String {
    format!("img/{owner}/{name}")
}

pub fn pool_coords(owner: &str, name: &str) -> (&'static str, String) {
    ("img", format!("{owner}/{name}"))
}

/// The immediate sub-prefixes under `prefix` in the object store — its "directory listing". Shared
/// by `routes::image_names` (`repo/img/{owner}/`) and `worker::blob_owners` (`blobs/`): both are the
/// same eight lines of `list_with_delimiter` plus a `common_prefixes` map, over a different prefix.
pub async fn list_dir_names(
    os: &std::sync::Arc<dyn slatedb::object_store::ObjectStore>,
    prefix: &str,
) -> crate::Result<Vec<String>> {
    let prefix = slatedb::object_store::path::Path::from(prefix.to_string());
    let listing = os
        .list_with_delimiter(Some(&prefix))
        .await
        .map_err(|e| crate::err(e.to_string()))?;
    let mut names: Vec<String> = listing
        .common_prefixes
        .iter()
        .filter_map(|p| p.parts().next_back().map(|n| n.as_ref().to_string()))
        .collect();
    names.sort();
    Ok(names)
}

/// `n`/`last` pagination over a sorted list, shared by `tags/list` and `_catalog`.
/// Returns the page and, when the list was truncated, the value the next `last` should be.
pub(crate) fn paginate(
    all: &[String],
    q: &std::collections::HashMap<String, String>,
) -> (Vec<String>, Option<String>) {
    let start = match q.get("last") {
        Some(last) => all.partition_point(|v| v.as_str() <= last.as_str()),
        None => 0,
    };
    let rest = &all[start.min(all.len())..];
    let n: usize = q.get("n").and_then(|v| v.parse().ok()).unwrap_or(rest.len());
    let page: Vec<String> = rest.iter().take(n).cloned().collect();
    let truncated = (page.len() < rest.len()).then(|| page.last().cloned()).flatten();
    (page, truncated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_paths_parse() {
        assert_eq!(image_route("/v2/acme/nginx/blobs/sha256:aa"), Some(("acme", "nginx")));
        assert_eq!(image_route("/v2/acme/nginx/manifests/latest"), Some(("acme", "nginx")));
        assert_eq!(image_route("/v2/acme/nginx/blobs/uploads/"), Some(("acme", "nginx")));
        assert_eq!(image_route("/v2/acme/nginx/tags/list"), Some(("acme", "nginx")));
        assert_eq!(image_route("/v2/acme/nginx/referrers/sha256:aa"), Some(("acme", "nginx")));
    }

    #[test]
    fn non_image_v2_paths_do_not_route() {
        // These are answered locally on whichever node receives them.
        assert_eq!(image_route("/v2/"), None);
        assert_eq!(image_route("/v2"), None);
        assert_eq!(image_route("/v2/token"), None);
        assert_eq!(image_route("/v2/_catalog"), None);
        // A nested name is not a two-segment image, so it never routes.
        assert_eq!(image_route("/v2/acme/team/nginx/manifests/latest"), None);
        // An unknown tail is not a registry endpoint.
        assert_eq!(image_route("/v2/acme/nginx/frobnicate"), None);
    }

    #[test]
    fn keys_cannot_collide_with_a_repo() {
        // The image acme/nginx and the repo acme/nginx are different objects.
        assert_eq!(routing_key("acme", "nginx"), "img/acme/nginx");
        assert_ne!(routing_key("acme", "nginx"), "acme/nginx");
        // The key round-trips through split_once exactly as lib.rs does it.
        let key = routing_key("acme", "nginx");
        let (o, n) = key.split_once('/').unwrap();
        assert_eq!((o, n), ("img", "acme/nginx"));
        assert_eq!(pool_coords("acme", "nginx"), ("img", "acme/nginx".to_string()));
        // And no repo can be owned by `img`, so no repo database nests under one.
        assert!(!crate::dbstore::valid_owner("img"));
        assert!(!crate::dbstore::valid_owner("v2"));
    }
}
