mod common;
use axum::http::StatusCode;
use rustic_git::registry::Digest;
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
