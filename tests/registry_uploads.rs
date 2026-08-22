mod common;
use axum::http::StatusCode;
use rustic_git::index::{self, Kind, Marker};
use rustic_git::registry::{gc, store::manifest_path, Digest};
use slatedb::object_store::{ObjectStoreExt, PutPayload};
use std::time::Duration;

#[tokio::test]
async fn a_layer_uploads_in_chunks() {
    let (base, e) = common::serve_public().await;
    let c = reqwest::Client::new();
    let token = e.store.create_token("acme").await.unwrap();

    let r = c.post(format!("{base}/v2/acme/nginx/blobs/uploads/"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::ACCEPTED);
    assert!(r.headers().get("docker-upload-uuid").is_some());
    let loc = r.headers().get("location").unwrap().to_str().unwrap().to_string();

    let (a, b) = (b"first half ".to_vec(), b"second half".to_vec());
    let r = c.patch(format!("{base}{loc}"))
        .basic_auth("acme", Some(&token))
        .header("content-range", format!("0-{}", a.len() - 1))
        .body(a.clone()).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::ACCEPTED);
    assert_eq!(r.headers().get("range").unwrap().to_str().unwrap(), format!("0-{}", a.len() - 1));

    let r = c.patch(format!("{base}{loc}"))
        .basic_auth("acme", Some(&token))
        .header("content-range", format!("{}-{}", a.len(), a.len() + b.len() - 1))
        .body(b.clone()).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::ACCEPTED);

    let whole = [a.clone(), b.clone()].concat();
    let d = Digest::of(&whole);
    let r = c.put(format!("{base}{loc}?digest={d}"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);

    let r = c.get(format!("{base}/v2/acme/nginx/blobs/{d}"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.bytes().await.unwrap().to_vec(), whole);
}

#[tokio::test]
async fn a_chunk_out_of_order_is_416() {
    let (base, e) = common::serve_public().await;
    let c = reqwest::Client::new();
    let token = e.store.create_token("acme").await.unwrap();
    let r = c.post(format!("{base}/v2/acme/nginx/blobs/uploads/"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    let loc = r.headers().get("location").unwrap().to_str().unwrap().to_string();
    // Starts at 50 when the session holds 0 bytes: a gap.
    let r = c.patch(format!("{base}{loc}"))
        .basic_auth("acme", Some(&token))
        .header("content-range", "50-59")
        .body(b"0123456789".to_vec()).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::RANGE_NOT_SATISFIABLE);
}

/// `end + 1` on a `Content-Range` end of `u64::MAX` used to overflow and panic in debug. It must
/// be refused as a bad request instead.
#[tokio::test]
async fn a_content_range_end_at_u64_max_is_refused_cleanly() {
    let (base, e) = common::serve_public().await;
    let c = reqwest::Client::new();
    let token = e.store.create_token("acme").await.unwrap();
    let r = c.post(format!("{base}/v2/acme/nginx/blobs/uploads/"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    let loc = r.headers().get("location").unwrap().to_str().unwrap().to_string();
    let r = c.patch(format!("{base}{loc}"))
        .basic_auth("acme", Some(&token))
        .header("content-range", "0-18446744073709551615")
        .body(b"0123456789".to_vec()).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_session_reports_its_progress_and_can_be_cancelled() {
    let (base, e) = common::serve_public().await;
    let c = reqwest::Client::new();
    let token = e.store.create_token("acme").await.unwrap();
    let r = c.post(format!("{base}/v2/acme/nginx/blobs/uploads/"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    let loc = r.headers().get("location").unwrap().to_str().unwrap().to_string();
    c.patch(format!("{base}{loc}")).basic_auth("acme", Some(&token))
        .header("content-range", "0-4").body(b"hello".to_vec()).send().await.unwrap();

    let r = c.get(format!("{base}{loc}")).basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::NO_CONTENT);
    assert_eq!(r.headers().get("range").unwrap().to_str().unwrap(), "0-4");

    let r = c.delete(format!("{base}{loc}")).basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::NO_CONTENT);
    let r = c.get(format!("{base}{loc}")).basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_chunk_whose_declared_length_disagrees_with_its_body_is_refused() {
    let (base, e) = common::serve_public().await;
    let c = reqwest::Client::new();
    let token = e.store.create_token("acme").await.unwrap();
    let r = c.post(format!("{base}/v2/acme/nginx/blobs/uploads/"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    let loc = r.headers().get("location").unwrap().to_str().unwrap().to_string();

    // Claims 100 bytes (0-99) but the body is 5: header and body disagree.
    let r = c.patch(format!("{base}{loc}"))
        .basic_auth("acme", Some(&token))
        .header("content-range", "0-99")
        .body(b"hello".to_vec()).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);

    // The session must not have advanced. An empty session answers `0-0` — the resume
    // protocol reads the header unconditionally, so it is always present.
    let r = c.get(format!("{base}{loc}")).basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::NO_CONTENT);
    assert_eq!(r.headers().get("range").unwrap().to_str().unwrap(), "0-0");
    assert!(r.headers().get("location").is_some(), "a resuming client needs the session URL");
}

#[tokio::test]
async fn a_completed_upload_whose_digest_lies_is_refused_and_stores_nothing() {
    let (base, e) = common::serve_public().await;
    let c = reqwest::Client::new();
    let token = e.store.create_token("acme").await.unwrap();
    let r = c.post(format!("{base}/v2/acme/nginx/blobs/uploads/"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    let loc = r.headers().get("location").unwrap().to_str().unwrap().to_string();
    c.patch(format!("{base}{loc}")).basic_auth("acme", Some(&token))
        .header("content-range", "0-4").body(b"hello".to_vec()).send().await.unwrap();
    let lie = Digest::of(b"not hello");
    let r = c.put(format!("{base}{loc}?digest={lie}"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    let r = c.get(format!("{base}/v2/acme/nginx/blobs/{lie}"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
}

/// The final `PUT` may carry its own Content-Range for the last chunk. If its declared end
/// disagrees with the body actually sent, that's the client's own bookkeeping wrong — a 400, not
/// a DIGEST_INVALID from hashing whatever bytes happened to land.
#[tokio::test]
async fn a_completing_puts_content_range_end_must_match_its_body() {
    let (base, e) = common::serve_public().await;
    let c = reqwest::Client::new();
    let token = e.store.create_token("acme").await.unwrap();
    let r = c.post(format!("{base}/v2/acme/nginx/blobs/uploads/"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    let loc = r.headers().get("location").unwrap().to_str().unwrap().to_string();

    let whole = b"hello world".to_vec();
    let d = Digest::of(&whole);
    // Correct start (0), but a declared end (99) that doesn't match the 11-byte body — must be
    // rejected before the digest is ever computed.
    let r = c.put(format!("{base}{loc}?digest={d}"))
        .basic_auth("acme", Some(&token))
        .header("content-range", "0-99")
        .body(whole.clone()).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    let b: serde_json::Value = r.json().await.unwrap();
    assert_eq!(b["errors"][0]["code"], "BLOB_UPLOAD_INVALID", "not DIGEST_INVALID: the range header is what's wrong");

    // Nothing was stored under the correct digest.
    let r = c.get(format!("{base}/v2/acme/nginx/blobs/{d}"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
}

/// A 416 must tell a resuming client where the session actually stands: `Range: 0-{last}`, plus
/// `Docker-Upload-UUID` and `Location` so it can address the session again.
#[tokio::test]
async fn a_416_carries_range_and_session_headers_to_resume_from() {
    let (base, e) = common::serve_public().await;
    let c = reqwest::Client::new();
    let token = e.store.create_token("acme").await.unwrap();
    let r = c.post(format!("{base}/v2/acme/nginx/blobs/uploads/"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    let loc = r.headers().get("location").unwrap().to_str().unwrap().to_string();
    let uuid = r.headers().get("docker-upload-uuid").unwrap().to_str().unwrap().to_string();

    // Empty session: an out-of-order PATCH must report 0-0.
    let r = c.patch(format!("{base}{loc}"))
        .basic_auth("acme", Some(&token))
        .header("content-range", "50-59")
        .body(b"0123456789".to_vec()).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(r.headers().get("range").unwrap().to_str().unwrap(), "0-0");
    assert_eq!(r.headers().get("docker-upload-uuid").unwrap().to_str().unwrap(), uuid);
    assert_eq!(r.headers().get("location").unwrap().to_str().unwrap(), loc);

    // 5 valid bytes, then a bad-offset PATCH must report 0-4.
    let r = c.patch(format!("{base}{loc}")).basic_auth("acme", Some(&token))
        .header("content-range", "0-4").body(b"hello".to_vec()).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::ACCEPTED);
    let r = c.patch(format!("{base}{loc}"))
        .basic_auth("acme", Some(&token))
        .header("content-range", "50-59")
        .body(b"0123456789".to_vec()).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(r.headers().get("range").unwrap().to_str().unwrap(), "0-4");
}

/// An abandoned session (opened, chunk staged, never completed or cancelled) must not leak the
/// staging object or its DB row forever — the GC sweep has to find and remove both. `Duration::ZERO`
/// grace is the same seam `registry_gc.rs` uses to make "already old enough" true without a real
/// clock: the session was created strictly before the sweep call, so a zero grace window always
/// includes it.
#[tokio::test]
async fn stale_upload_sessions_are_swept() {
    let (base, e) = common::serve_public().await;
    let c = reqwest::Client::new();
    let token = e.store.create_token("acme").await.unwrap();

    let r = c.post(format!("{base}/v2/acme/nginx/blobs/uploads/"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::ACCEPTED);
    let loc = r.headers().get("location").unwrap().to_str().unwrap().to_string();

    c.patch(format!("{base}{loc}")).basic_auth("acme", Some(&token))
        .header("content-range", "0-4").body(b"hello".to_vec()).send().await.unwrap();

    let n = e.store.sweep_stale_uploads("acme", Duration::ZERO).await.unwrap();
    assert_eq!(n, 1);

    // Both halves are gone: the row (a GET on the session now 404s) and the staging object
    // (implied by the same 404 — `received` reads the DB row, which is what a resumed PATCH or a
    // completing PUT actually checks).
    let r = c.get(format!("{base}{loc}")).basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
}

/// A fresh session — inside the grace window — must survive the sweep, the same way a
/// freshly-uploaded blob survives `gc::sweep_owner` within its grace window.
#[tokio::test]
async fn a_fresh_upload_session_survives_the_grace_window() {
    let (base, e) = common::serve_public().await;
    let c = reqwest::Client::new();
    let token = e.store.create_token("acme").await.unwrap();

    let r = c.post(format!("{base}/v2/acme/nginx/blobs/uploads/"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    let loc = r.headers().get("location").unwrap().to_str().unwrap().to_string();
    c.patch(format!("{base}{loc}")).basic_auth("acme", Some(&token))
        .header("content-range", "0-4").body(b"hello".to_vec()).send().await.unwrap();

    let n = e.store.sweep_stale_uploads("acme", Duration::from_secs(3600)).await.unwrap();
    assert_eq!(n, 0, "a session just opened must not be swept out from under an in-flight push");

    let r = c.get(format!("{base}{loc}")).basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::NO_CONTENT);
}

/// The GC worker's structural reconcile (`gc::reconcile_owner`) repairs what object-store reads
/// can prove — see its doc comment for the split with the owning node's visibility repair. These
/// three cases mirror the sweep's own keep-biased contract: never invent or destroy a marker on
/// uncertainty, only on what a listing actually shows.
fn marker(name: &str, public: bool, manifests: u64, updated_ms: i64) -> Marker {
    Marker {
        name: name.to_string(),
        public,
        created_by: "alice@example.com".into(),
        created_ms: 1,
        description: String::new(),
        manifests,
        updated_ms,
    }
}

#[tokio::test]
async fn an_unmarked_image_gains_a_private_marker() {
    let e = common::env().await;
    // Opening the image DB is what makes it exist structurally, without ever writing a marker —
    // the fixture deliberately skips `refresh_image_marker` to simulate the drift this repairs.
    e.store.image_db("acme", "nginx").await.unwrap();
    let manifest = b"fake manifest".to_vec();
    let d = Digest::of(&manifest);
    e.store.os.put(&manifest_path("acme", "nginx", &d), PutPayload::from(manifest)).await.unwrap();

    let n = gc::reconcile_owner(&e.store, "acme").await.unwrap();
    assert_eq!(n, 1);

    let m = index::read(&e.store.os, Kind::Img, "acme", "nginx").await.unwrap();
    assert!(!m.public, "an image discovered with no marker must fail closed to private");
    assert_eq!(m.manifests, 1);
}

#[tokio::test]
async fn a_marker_for_a_deleted_image_is_removed() {
    let e = common::env().await;
    // No image DB is ever opened for "ghost" — the marker is the only trace left, as if the
    // image directory had been deleted out from under it.
    index::write(&e.store.os, Kind::Img, "acme", &marker("ghost", true, 3, 100)).await.unwrap();

    let n = gc::reconcile_owner(&e.store, "acme").await.unwrap();
    assert_eq!(n, 1);
    assert!(index::read(&e.store.os, Kind::Img, "acme", "ghost").await.is_none());
}

#[tokio::test]
async fn a_marker_with_stale_manifest_count_is_corrected() {
    let e = common::env().await;
    e.store.image_db("acme", "nginx").await.unwrap();
    let manifest = b"fake manifest".to_vec();
    let d = Digest::of(&manifest);
    e.store.os.put(&manifest_path("acme", "nginx", &d), PutPayload::from(manifest)).await.unwrap();
    // A marker frozen at "no pushes yet" — as if refresh_image_marker never ran after this push.
    index::write(&e.store.os, Kind::Img, "acme", &marker("nginx", true, 0, 0)).await.unwrap();

    let n = gc::reconcile_owner(&e.store, "acme").await.unwrap();
    assert_eq!(n, 1);

    let m = index::read(&e.store.os, Kind::Img, "acme", "nginx").await.unwrap();
    assert_eq!(m.manifests, 1);
    assert!(m.public, "visibility is not this sweep's to touch — it must be preserved as-is");
}

#[tokio::test]
async fn a_stale_stats_repair_never_resurrects_a_public_marker_over_a_fresh_private_one() {
    let e = common::env().await;
    e.store.image_db("acme", "nginx").await.unwrap();
    let manifest = b"fake manifest".to_vec();
    let d = Digest::of(&manifest);
    e.store.os.put(&manifest_path("acme", "nginx", &d), PutPayload::from(manifest)).await.unwrap();
    // Simulate the race the finding describes: a stale-stats PUBLIC marker left behind by an
    // in-progress reconcile, plus a fresh PRIVATE marker the owning node just wrote via a
    // concurrent visibility flip that landed after the reconcile listed markers.
    index::write(&e.store.os, Kind::Img, "acme", &marker("nginx", true, 0, 0)).await.unwrap();
    index::write(&e.store.os, Kind::Img, "acme", &marker("nginx", false, 1, 1)).await.unwrap();

    gc::reconcile_owner(&e.store, "acme").await.unwrap();

    // The private marker must survive untouched — the worker must never delete "the other side".
    let private_path = index::path(false, Kind::Img, "acme", "nginx");
    assert!(e.store.os.head(&private_path).await.is_ok(), "private marker must not be deleted by the sweep");
    // Fail-closed by construction: `list` dedupes a name present under both prefixes to its
    // private entry (see `both_markers_read_as_private` in src/index.rs), so a public-only
    // listing must not surface it even though a public marker also still exists.
    let public_listing = index::list(&e.store.os, Kind::Img, "acme", false).await.unwrap();
    assert!(public_listing.is_empty(), "must not appear in a public-only listing while both markers exist");
    let full_listing = index::list(&e.store.os, Kind::Img, "acme", true).await.unwrap();
    assert_eq!(full_listing.len(), 1);
    assert!(!full_listing[0].public, "the surviving entry must resolve to private");
}

/// The same structural repair, for code repos. Repo directories live at `repo/{owner}/{name}` —
/// the same prefix images nest under as `repo/img/{owner}/{name}` — so these also pin down that
/// the reserved owner `img` is never swept as if it held code repos.
#[tokio::test]
async fn an_unmarked_repo_gains_a_private_marker() {
    let e = common::env().await;
    // Opening the repo DB is what makes it exist structurally; no marker is ever written.
    e.store.db_for("acme", "web").await.unwrap();

    let n = gc::reconcile_repo_owner(&e.store, "acme").await.unwrap();
    assert_eq!(n, 1);

    let m = index::read(&e.store.os, Kind::Repo, "acme", "web").await.unwrap();
    assert!(!m.public, "a repo discovered with no marker must fail closed to private");
}

#[tokio::test]
async fn a_marker_for_a_deleted_repo_is_removed() {
    let e = common::env().await;
    index::write(&e.store.os, Kind::Repo, "acme", &marker("ghost", true, 0, 0)).await.unwrap();

    let n = gc::reconcile_repo_owner(&e.store, "acme").await.unwrap();
    assert_eq!(n, 1);
    assert!(index::read(&e.store.os, Kind::Repo, "acme", "ghost").await.is_none());
}

#[tokio::test]
async fn a_repo_marker_with_the_wrong_visibility_is_left_alone() {
    let e = common::env().await;
    e.store.db_for("acme", "web").await.unwrap();
    // The DB says private (nothing ever set `meta/public`), the marker claims public. Only the
    // owning node may read that row, so this sweep must not act on the disagreement.
    index::write(&e.store.os, Kind::Repo, "acme", &marker("web", true, 0, 0)).await.unwrap();

    let n = gc::reconcile_repo_owner(&e.store, "acme").await.unwrap();
    assert_eq!(n, 0);
    let m = index::read(&e.store.os, Kind::Repo, "acme", "web").await.unwrap();
    assert!(m.public, "visibility repair belongs to the owning node, not this sweep");
}

#[tokio::test]
async fn the_repo_sweep_never_touches_the_img_keyspace() {
    let e = common::env().await;
    e.store.image_db("acme", "nginx").await.unwrap();
    index::write(&e.store.os, Kind::Img, "acme", &marker("nginx", true, 1, 1)).await.unwrap();

    // `repo/img/acme/nginx` would look like a repo named `acme` owned by `img` to a naive sweep,
    // and its image markers like orphans to a repo sweep of owner `acme`.
    let n = gc::reconcile_repo_owner(&e.store, "img").await.unwrap();
    assert_eq!(n, 0, "the reserved owner `img` holds no code repos");
    assert!(index::read(&e.store.os, Kind::Repo, "img", "acme").await.is_none());

    gc::reconcile_repo_owner(&e.store, "acme").await.unwrap();
    assert!(index::read(&e.store.os, Kind::Img, "acme", "nginx").await.is_some());

    // ...and the image sweep leaves repo markers alone in the same way.
    e.store.db_for("acme", "web").await.unwrap();
    index::write(&e.store.os, Kind::Repo, "acme", &marker("web", false, 0, 0)).await.unwrap();
    gc::reconcile_owner(&e.store, "acme").await.unwrap();
    assert!(index::read(&e.store.os, Kind::Repo, "acme", "web").await.is_some());
}

/// A repo that was DELETED must not be resurrected by the sweep.
///
/// `delete_repo` used to clear the keys inside the database but leave the database's own files
/// under `repo/{owner}/{name}/`. The sweep reads a surviving directory as "this repo exists, it
/// just lost its marker" and helpfully writes one — so in production every repo that had ever been
/// deleted reappeared in its owner's listing the moment marker-backed listings shipped.
#[tokio::test]
async fn a_deleted_repo_is_not_resurrected_by_the_sweep() {
    let e = common::env().await;
    e.store.create_repo("acme", "gone").await.unwrap();
    e.store.delete_repo("acme", "gone").await.unwrap();

    // The sweep must find nothing to do: no directory, so no repo to invent a marker for.
    gc::reconcile_repo_owner(&e.store, "acme").await.unwrap();

    assert!(
        index::read(&e.store.os, Kind::Repo, "acme", "gone").await.is_none(),
        "a deleted repo must not come back with a marker"
    );
}
