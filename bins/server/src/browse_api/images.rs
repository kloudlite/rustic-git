//! Container-image browse routes: the owner-scoped image list and per-image tag management.
use super::hidden;
use crate::registry::store::ImageExt;
use crate::router::internal;
use kloudlite_git_core::httpx::Trusted;
use crate::App;
use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use futures::StreamExt;
use serde::Serialize;
use slatedb::object_store::ObjectStoreExt;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Serialize)]
pub(super) struct ImageSummary {
    name: String,
    /// Object-store manifest count, NOT a tag count: `images` is owner-scoped and cannot route to
    /// any one image's database (tags and visibility both live there), so this reads only the
    /// shared object store — see the handler doc below.
    manifests: usize,
    /// When the newest manifest was written, epoch millis. `None` for an image whose manifests are
    /// gone but whose prefix remains — a push that uploaded blobs and never finished.
    updated_ms: Option<i64>,
    /// From the listing-index marker (`false` for an unmarked, pre-backfill image — see
    /// `registry::routes::image_listing`'s fallback). Visibility could not be carried here before
    /// the marker existed, since it lives in the image's own database, which this handler must
    /// never open.
    pub public: bool,
}

/// `GET /api/{owner}/images` — the team's images, for the Container Images page. `?n=&last=`
/// page it like `_catalog`, `Link` when truncated; the whole list without.
///
/// Owner-scoped rather than repo-scoped, so it is the one browse route whose second segment is not
/// a repo name (see `api_route` in `router/route.rs`). It still routes: `images` is a `BROWSE_TAILS` entry,
/// but `repo_of` answers `None` for it and the request is served by whichever node received it.
/// That is only safe because this handler reads the shared object store ALONE — it must never call
/// `image_db`/`store.tags`/`store.image_is_public`, each of which opens a specific image's database
/// with no ownership check, fencing that image's legitimate owner if served on the wrong node. Tag
/// counts and visibility both live in that database, which is why `ImageSummary` carries neither.
pub(super) async fn images(
    State(app): State<Arc<App>>,
    axum::Extension(trusted): axum::Extension<Trusted>,
    headers: HeaderMap,
    Path(owner): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    // `?public=1` is the team's public face: no caller check and `include_private: false`, so the
    // two arms can never be confused — a member who wants the full list asks without the flag.
    let public_only = q.get("public").is_some_and(|v| v == "1");
    if !public_only {
        match crate::registry::auth::caller(&app, &trusted, &headers).await {
            Ok(Some(who)) if who == owner => {}
            Ok(_) => return hidden(),
            Err(r) => return r,
        }
    }
    let markers = match crate::registry::routes::image_listing(&app, &owner, !public_only, &q).await {
        Ok(m) => m,
        Err(e) => return internal(e),
    };
    let names: Vec<String> = markers.iter().map(|m| m.name.clone()).collect();
    let (page, truncated) = crate::registry::paginate(&names, &q);
    let out: Vec<ImageSummary> = markers
        .into_iter()
        .filter(|m| page.contains(&m.name))
        .map(|m| ImageSummary {
            name: m.name,
            manifests: m.manifests as usize,
            updated_ms: if m.updated_ms > 0 { Some(m.updated_ms) } else { None },
            public: m.public,
        })
        .collect();
    let mut r = Json(out).into_response();
    if let Some(last) = truncated {
        let n = q.get("n").cloned().unwrap_or_default();
        let public = if public_only { "&public=1" } else { "" };
        r.headers_mut().insert(
            axum::http::header::LINK,
            format!("</api/{owner}/images?n={n}&last={last}{public}>; rel=\"next\"").parse().unwrap(),
        );
    }
    r
}

#[derive(Serialize)]
pub(super) struct ImageTag {
    tag: String,
    digest: String,
    /// The manifest document's own size on disk — kilobytes, not the image's size.
    size: u64,
    /// What pulling this tag actually transfers: the config blob plus every layer, as the manifest
    /// itself declares them. Summed from the manifest rather than stored, because nothing writes an
    /// image-size field and a stored one could disagree with the layers that are really there.
    bytes: u64,
    /// When this manifest was written, epoch millis, from the object store's own mtime.
    pushed_ms: Option<i64>,
    /// Manifest GETs by this tag — one per `docker pull`.
    pulls: u64,
}

/// `GET /api/{owner}/{image}/imagetags` — the tag rows the image page needs. Shaped like every
/// other repo-scoped browse route (`{image}` fills the `{name}` slot), but it routes by the IMAGE
/// key (`registry::routing_key`, `img/{owner}/{name}`), not the repo key: `repo_of` in `router/route.rs`
/// special-cases the `imagetags` tail so this reaches the node that actually holds the image's
/// database, which may differ from whatever node owns a git repo of the same name.
///
/// `?n=&last=` page it exactly as `tags/list` does, `Link` when truncated; the whole list without.
pub(super) async fn imagetags(
    State(app): State<Arc<App>>,
    axum::Extension(trusted): axum::Extension<Trusted>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    match crate::registry::auth::caller(&app, &trusted, &headers).await {
        Ok(Some(who)) if who == owner => {}
        Ok(_) => return hidden(),
        Err(r) => return r,
    }
    let tags = match app.store.tags(&owner, &name).await {
        Ok(t) => t,
        Err(e) => return internal(e),
    };
    let (page, truncated) = crate::registry::paginate(&tags, &q);
    // One future per tag, eight in flight: the reads per tag are independent of every other
    // tag's, and a 100-tag image was serial round trips. `buffered`, not `buffer_unordered`:
    // the page shows them in `tags`' order and re-sorting would cost what it saved.
    let out: Vec<ImageTag> = futures::stream::iter(page)
        .map(|tag| {
            let (app, owner, name) = (app.clone(), owner.clone(), name.clone());
            async move {
                // A tag whose row cannot be read is left out of the page, but said so: silently
                // dropping it made a corrupt row indistinguishable from a deleted tag.
                let d = match app.store.tag(&owner, &name, &tag).await {
                    Ok(d) => d?,
                    Err(e) => {
                        tracing::warn!(owner = %owner, name = %name, %tag, error = %e, "registry.tag.read.failed");
                        return None;
                    }
                };
                // The row `put_manifest` wrote beside the manifest: three DB reads per tag and no
                // object-store round trip. A manifest pushed before the row existed falls back to
                // one GET — its `meta` is the ObjectMeta a HEAD returns, and the body is read to
                // ADD UP its declared sizes, never to re-emit it.
                let meta = match app.store.image_db(&owner, &name).await {
                    Ok(db) => db.get(crate::registry::manifests::manifest_meta_key(&d)).await.ok().flatten(),
                    Err(_) => None,
                };
                let (size, pushed_ms, bytes) = match meta {
                    Some(v) => {
                        let v = String::from_utf8_lossy(&v);
                        let mut f = v.split('\n').map(|x| x.parse::<u64>().ok());
                        (f.next().flatten().unwrap_or(0), f.next().flatten().map(|p| p as i64), f.next().flatten().unwrap_or(0))
                    }
                    None => match app.store.os.get(&crate::registry::store::manifest_path(&owner, &name, &d)).await {
                        Ok(r) => {
                            let (size, pushed) = (r.meta.size, r.meta.last_modified.timestamp_millis());
                            (size, Some(pushed), r.bytes().await.map(|b| crate::registry::manifests::declared_size(&b)).unwrap_or(0))
                        }
                        Err(_) => (0, None, 0),
                    },
                };
                let pulls = app.store.pulls(&owner, &name, &tag).await.unwrap_or(0);
                Some(ImageTag { tag, digest: d.to_string(), size, bytes, pushed_ms, pulls })
            }
        })
        .buffered(8)
        .filter_map(|t| async move { t })
        .collect()
        .await;
    let mut r = Json(out).into_response();
    if let Some(last) = truncated {
        let n = q.get("n").cloned().unwrap_or_default();
        r.headers_mut().insert(
            axum::http::header::LINK,
            format!("</api/{owner}/{name}/imagetags?n={n}&last={last}>; rel=\"next\"").parse().unwrap(),
        );
    }
    r
}

/// `POST /api/{owner}/{image}/imagetagdelete` — remove one tag. The body is the tag name, plain
/// text, matching the shape of every other browse write here.
///
/// Deletes ONLY the tag row (`store.delete_tag`); the manifest it pointed at is left alone. Other
/// tags may still reference that manifest, and an unreferenced one is a garbage-collection
/// question, not something a single tag delete is in a position to answer.
pub(super) async fn imagetagdelete(
    State(app): State<Arc<App>>,
    axum::Extension(trusted): axum::Extension<Trusted>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    body: axum::body::Bytes,
) -> Response {
    match crate::registry::auth::caller(&app, &trusted, &headers).await {
        Ok(Some(who)) if who == owner => {}
        Ok(_) => return hidden(),
        Err(r) => return r,
    }
    let tag = String::from_utf8_lossy(&body).trim().to_string();
    if tag.is_empty() {
        return (StatusCode::BAD_REQUEST, "missing tag").into_response();
    }
    if !app.store.image_exists(&owner, &name).await.unwrap_or(false) {
        return hidden();
    }
    match app.store.delete_tag(&owner, &name, &tag).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => internal(e),
    }
}

/// `POST /api/{owner}/{image}/imagedelete` — remove the whole image.
///
/// Deletes every manifest object under `manifests/{owner}/{image}/`, then hands off to
/// `Store::delete_image` for the database side — every row this image owns, plus the database's
/// own storage, so it also stops appearing in the Container Images list (see that method's doc
/// comment for why clearing rows alone is not enough). Never touches blobs: `blobs::delete_blob`
/// states the invariant this route honours — "no manifest delete removes a blob... that is the
/// sweeper's job, because only it can see every image that might share the layer" — so layer data
/// is reclaimed later by the sweeper, not here. Scoped entirely to THIS image's own database and
/// its own manifest prefix; a sibling image, even one owned by the same team, lives under a
/// different key and a different prefix and is never read or written by this handler.
pub(super) async fn imagedelete(
    State(app): State<Arc<App>>,
    axum::Extension(trusted): axum::Extension<Trusted>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
) -> Response {
    match crate::registry::auth::caller(&app, &trusted, &headers).await {
        Ok(Some(who)) if who == owner => {}
        Ok(_) => return hidden(),
        Err(r) => return r,
    }
    if !app.store.image_exists(&owner, &name).await.unwrap_or(false) {
        return hidden();
    }
    // Marker first: a crash after this point leaves orphaned manifest/db bytes for GC to sweep,
    // never a listing entry for storage that's (partly) gone.
    if let Err(e) = crate::index::remove(&app.store, crate::index::Kind::Img, &owner, &name).await {
        return internal(e);
    }
    use slatedb::object_store::ObjectStore;
    use futures::{StreamExt, TryStreamExt};
    let prefix = kloudlite_git_registry::store::manifest_prefix(&owner, &name);
    // `delete_stream` feeds deletes straight off the listing — the collect-then-delete loop
    // paid one round trip per manifest. NotFound per object is tolerated: another delete of
    // the same image racing this one changes nothing about the end state.
    let doomed = app.store.os.list(Some(&prefix)).map_ok(|m| m.location).boxed();
    let mut results = app.store.os.delete_stream(doomed);
    while let Some(r) = results.next().await {
        match r {
            Ok(_) | Err(slatedb::object_store::Error::NotFound { .. }) => {}
            Err(e) => return internal(e.into()),
        }
    }
    match app.store.delete_image(&owner, &name).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => internal(e),
    }
}

/// What a pull of this manifest transfers: its config blob plus every layer.
///
/// An index (a multi-platform image) names other MANIFESTS rather than layers; its entries carry
/// their own `size`, so summing them gives the index's total across platforms. Anything
/// unrecognised sums to zero rather than guessing — a wrong number shown confidently is worse than
/// no number.
/// `POST /api/{owner}/{name}/imagevisibility?visibility=public|private`
///
/// The image counterpart of the repo `visibility` route, and it has to exist: `admin
/// set-image-visibility` posts here whenever a fleet is configured, so without it an image's
/// visibility could not be changed on a running cluster at all.
///
/// Routed by the IMAGE key (`img/{owner}/{name}`, see `repo_of`), so this runs on the node that
/// owns the image's database — the only node that may write it. Authorization is the caller being
/// the owner, exactly as `imagedelete` beside it: there is no public-stranger concept for the
/// image write paths.
///
/// The flip itself — keyed lock, remove-permissive-first, marker swap — lives in
/// `set_image_visibility`; this handler only parses and authorizes.
pub(super) async fn imagevisibility(
    State(app): State<Arc<App>>,
    axum::Extension(trusted): axum::Extension<Trusted>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let public = match q.get("visibility").map(String::as_str) {
        Some("public") => true,
        Some("private") => false,
        _ => return (StatusCode::BAD_REQUEST, "visibility must be public or private").into_response(),
    };
    match crate::registry::auth::caller(&app, &trusted, &headers).await {
        Ok(Some(who)) if who == owner => {}
        Ok(_) => return hidden(),
        Err(r) => return r,
    }
    if !app.store.image_exists(&owner, &name).await.unwrap_or(false) {
        return hidden();
    }
    match app.store.set_image_visibility(&owner, &name, public).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => internal(e),
    }
}
