mod common;
use axum::http::StatusCode;
use rustic_git::registry::Digest;
use slatedb::object_store::ObjectStoreExt;

async fn authed() -> (String, common::TestEnv, reqwest::Client, String) {
    let (base, e) = common::serve_public().await;
    let token = e.store.create_token("acme").await.unwrap();
    (base, e, reqwest::Client::new(), token)
}

#[tokio::test]
async fn a_blob_pushed_in_one_request_comes_back() {
    let (base, _e, c, token) = authed().await;
    let body = b"layer bytes".to_vec();
    let d = Digest::of(&body);
    let r = c.post(format!("{base}/v2/acme/nginx/blobs/uploads/?digest={d}"))
        .basic_auth("acme", Some(&token)).body(body.clone()).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);
    assert_eq!(r.headers().get("location").unwrap().to_str().unwrap(), format!("/v2/acme/nginx/blobs/{d}"));

    let r = c.get(format!("{base}/v2/acme/nginx/blobs/{d}"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.headers().get("docker-content-digest").unwrap().to_str().unwrap(), d.to_string());
    assert_eq!(r.bytes().await.unwrap().to_vec(), body);
}

#[tokio::test]
async fn a_large_blob_streams_back_exact_bytes() {
    // Regression guard for the streamed GET path: buffering the whole layer in the handler
    // was an anonymous memory-DoS for public images (max_layer is 10 GiB by default).
    let (base, _e, c, token) = authed().await;
    let body = vec![0xABu8; 5 * 1024 * 1024];
    let d = Digest::of(&body);
    let r = c.post(format!("{base}/v2/acme/nginx/blobs/uploads/?digest={d}"))
        .basic_auth("acme", Some(&token)).body(body.clone()).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);

    let r = c.get(format!("{base}/v2/acme/nginx/blobs/{d}"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.headers().get("content-length").unwrap().to_str().unwrap(), body.len().to_string());
    assert_eq!(r.bytes().await.unwrap().to_vec(), body);
}

#[tokio::test]
async fn a_blob_whose_digest_lies_is_refused() {
    let (base, _e, c, token) = authed().await;
    let wrong = Digest::of(b"something else");
    let r = c.post(format!("{base}/v2/acme/nginx/blobs/uploads/?digest={wrong}"))
        .basic_auth("acme", Some(&token)).body(b"layer bytes".to_vec()).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    let body: serde_json::Value = r.json().await.unwrap();
    assert_eq!(body["errors"][0]["code"], "DIGEST_INVALID");
}

#[tokio::test]
async fn head_answers_size_without_the_body() {
    let (base, _e, c, token) = authed().await;
    let body = b"layer bytes".to_vec();
    let d = Digest::of(&body);
    c.post(format!("{base}/v2/acme/nginx/blobs/uploads/?digest={d}"))
        .basic_auth("acme", Some(&token)).body(body.clone()).send().await.unwrap();
    let r = c.head(format!("{base}/v2/acme/nginx/blobs/{d}"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.headers().get("content-length").unwrap().to_str().unwrap(), body.len().to_string());
    assert!(r.bytes().await.unwrap().is_empty());
}

#[tokio::test]
async fn an_absent_blob_is_blob_unknown() {
    let (base, _e, c, token) = authed().await;
    let d = Digest::of(b"never pushed");
    let r = c.get(format!("{base}/v2/acme/nginx/blobs/{d}"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    let body: serde_json::Value = r.json().await.unwrap();
    assert_eq!(body["errors"][0]["code"], "BLOB_UNKNOWN");
}

#[tokio::test]
async fn the_two_request_push_works() {
    let (base, _e, c, token) = authed().await;
    let r = c.post(format!("{base}/v2/acme/nginx/blobs/uploads/"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::ACCEPTED);
    let loc = r.headers().get("location").unwrap().to_str().unwrap().to_string();
    assert!(loc.contains("/blobs/uploads/"), "got {loc}");

    let body = b"whole layer".to_vec();
    let d = Digest::of(&body);
    let r = c.put(format!("{base}{loc}?digest={d}"))
        .basic_auth("acme", Some(&token)).body(body.clone()).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);

    let r = c.get(format!("{base}/v2/acme/nginx/blobs/{d}"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.bytes().await.unwrap().to_vec(), body);
}

#[tokio::test]
async fn a_stranger_cannot_push() {
    let (base, e, c, _token) = authed().await;
    let other = e.store.create_token("other").await.unwrap();
    let body = b"x".to_vec();
    let d = Digest::of(&body);
    let r = c.post(format!("{base}/v2/acme/nginx/blobs/uploads/?digest={d}"))
        .basic_auth("other", Some(&other)).body(body).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
}

/// A no-trailing-slash upload-start request must still reach `start_upload`, not fall through to
/// the `{digest}` route and be answered as a malformed-digest lookup for a blob literally named
/// "uploads".
#[tokio::test]
async fn upload_start_without_trailing_slash_still_works() {
    let (base, _e, c, token) = authed().await;
    let r = c.post(format!("{base}/v2/acme/nginx/blobs/uploads"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::ACCEPTED, "body: {}", r.text().await.unwrap());
}

/// The flow every spec-following client uses before an anonymous pull of a public image: fetch a
/// token with no credentials at all, then present that same anonymous bearer token to GET a blob
/// of an image the owner has made public.
#[tokio::test]
async fn a_public_image_is_pullable_with_the_anonymous_token() {
    let (base, e, c, token) = authed().await;

    // Fetch an anonymous token, exactly as a client that has not logged in does.
    let r = c.get(format!("{base}/v2/token?scope=repository:acme/nginx:pull")).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let body: serde_json::Value = r.json().await.unwrap();
    let anon_token = body["token"].as_str().unwrap().to_string();

    // Owner pushes a blob and makes the image public.
    let blob = b"public layer".to_vec();
    let d = Digest::of(&blob);
    let r = c.post(format!("{base}/v2/acme/nginx/blobs/uploads/?digest={d}"))
        .basic_auth("acme", Some(&token)).body(blob.clone()).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);
    e.store.set_image_visibility("acme", "nginx", true).await.unwrap();

    // The anonymous bearer token can now pull the public blob.
    let r = c.get(format!("{base}/v2/acme/nginx/blobs/{d}"))
        .bearer_auth(&anon_token).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK, "body: {}", r.text().await.unwrap());
    assert_eq!(r.bytes().await.unwrap().to_vec(), blob);
}

#[tokio::test]
async fn a_layer_mounts_from_another_image_in_the_same_team() {
    let (base, _e, c, token) = authed().await;
    let body = b"shared base layer".to_vec();
    let d = Digest::of(&body);
    c.post(format!("{base}/v2/acme/nginx/blobs/uploads/?digest={d}"))
        .basic_auth("acme", Some(&token)).body(body.clone()).send().await.unwrap();

    let r = c.post(format!("{base}/v2/acme/api/blobs/uploads/?mount={d}&from=acme/nginx"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);
    assert_eq!(r.headers().get("location").unwrap().to_str().unwrap(), format!("/v2/acme/api/blobs/{d}"));

    // Readable through the mounting image without a byte having moved.
    let r = c.get(format!("{base}/v2/acme/api/blobs/{d}"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.bytes().await.unwrap().to_vec(), body);
}

#[tokio::test]
async fn mounting_across_teams_falls_back_to_a_session() {
    let (base, e, c, token) = authed().await;
    let other = e.store.create_token("other").await.unwrap();
    let body = b"other team's layer".to_vec();
    let d = Digest::of(&body);
    c.post(format!("{base}/v2/other/thing/blobs/uploads/?digest={d}"))
        .basic_auth("other", Some(&other)).body(body).send().await.unwrap();

    // Blobs are per-owner, so this cannot be a metadata-only mount. The spec's answer is 202:
    // "upload it yourself".
    let r = c.post(format!("{base}/v2/acme/nginx/blobs/uploads/?mount={d}&from=other/thing"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::ACCEPTED);
    assert!(r.headers().get("location").is_some());
}

#[tokio::test]
async fn a_sha512_blob_round_trips() {
    let (base, _e, c, token) = authed().await;
    let body = b"layer bytes hashed with sha512".to_vec();
    let d = Digest::of_algo("sha512", &body).unwrap();
    assert!(d.to_string().starts_with("sha512:"));
    let r = c.post(format!("{base}/v2/acme/nginx/blobs/uploads/?digest={d}"))
        .basic_auth("acme", Some(&token)).body(body.clone()).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::CREATED, "body: {}", r.text().await.unwrap());

    let r = c.get(format!("{base}/v2/acme/nginx/blobs/{d}"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.headers().get("docker-content-digest").unwrap().to_str().unwrap(), d.to_string());
    assert_eq!(r.bytes().await.unwrap().to_vec(), body);
}

#[tokio::test]
async fn a_wrong_sha512_digest_is_refused_and_stores_nothing() {
    let (base, _e, c, token) = authed().await;
    let wrong = Digest::of_algo("sha512", b"something else").unwrap();
    let r = c.post(format!("{base}/v2/acme/nginx/blobs/uploads/?digest={wrong}"))
        .basic_auth("acme", Some(&token)).body(b"layer bytes".to_vec()).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    let body: serde_json::Value = r.json().await.unwrap();
    assert_eq!(body["errors"][0]["code"], "DIGEST_INVALID");
    let r = c.get(format!("{base}/v2/acme/nginx/blobs/{wrong}"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
}

/// A well-formed sha512 digest that was never pushed is a plain 404 BLOB_UNKNOWN, same as
/// sha256 — not a 400, which would mean the parser rejected the digest shape.
#[tokio::test]
async fn an_absent_but_well_formed_sha512_digest_is_blob_unknown() {
    let (base, _e, c, token) = authed().await;
    let d = Digest::of_algo("sha512", b"never pushed").unwrap();
    let r = c.get(format!("{base}/v2/acme/nginx/blobs/{d}"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    let body: serde_json::Value = r.json().await.unwrap();
    assert_eq!(body["errors"][0]["code"], "BLOB_UNKNOWN");
}

#[tokio::test]
async fn a_blob_can_be_deleted() {
    let (base, _e, c, token) = authed().await;
    let body = b"delete me".to_vec();
    let d = Digest::of(&body);
    c.post(format!("{base}/v2/acme/nginx/blobs/uploads/?digest={d}"))
        .basic_auth("acme", Some(&token)).body(body).send().await.unwrap();
    let r = c.delete(format!("{base}/v2/acme/nginx/blobs/{d}"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::ACCEPTED);
    let r = c.get(format!("{base}/v2/acme/nginx/blobs/{d}"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
}

/// GC's `sweep_owner` re-reads `referenced()` after listing to close the "push a manifest during
/// the sweep" race (see gc.rs), but that second read cannot help a blob whose OWN upload
/// timestamp is already older than `grace` — the listing phase drops it as a sweep candidate
/// before `referenced()` is even consulted a second time. A client that HEADs (or cross-repo
/// mounts) an old, unreferenced blob and then references it from a fresh manifest needs that
/// HEAD/mount to refresh the blob's mtime, or the blob is gone by the time the manifest lands.
/// Asserted directly against the object store's mtime (see task-11 brief) rather than racing an
/// actual sweep, which would be flaky by construction.
///
/// Both HEAD and mount are covered in one test, sequentially: `RUSTIC_GIT_BLOB_GRACE_SECS` is
/// process-global, so running them as separate `#[tokio::test]`s races cargo's parallel test
/// threads against each other's env var.
#[tokio::test]
async fn head_and_mount_refresh_an_aged_blobs_mtime_within_half_grace() {
    // A tiny grace makes "half the grace window" a sub-second sleep instead of half an hour.
    std::env::set_var("RUSTIC_GIT_BLOB_GRACE_SECS", "2");
    let (base, e, c, token) = authed().await;

    // --- HEAD refreshes the mtime of an aged, unreferenced blob ---
    let body = b"aging layer bytes".to_vec();
    let d = Digest::of(&body);
    c.post(format!("{base}/v2/acme/nginx/blobs/uploads/?digest={d}"))
        .basic_auth("acme", Some(&token)).body(body).send().await.unwrap();
    let path = rustic_git::registry::store::blob_path("acme", &d);
    let before = e.store.os.head(&path).await.unwrap().last_modified;
    // Older than half of the 2s grace (1s) so the cost guard in refresh_blob_mtime doesn't skip it.
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let r = c.head(format!("{base}/v2/acme/nginx/blobs/{d}"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let after = e.store.os.head(&path).await.unwrap().last_modified;
    assert!(after > before, "HEAD should have refreshed the blob's mtime past its grace-aged value");

    // --- Cross-repo mount refreshes the mtime of an aged, unreferenced blob ---
    let body2 = b"aging shared layer".to_vec();
    let d2 = Digest::of(&body2);
    c.post(format!("{base}/v2/acme/nginx/blobs/uploads/?digest={d2}"))
        .basic_auth("acme", Some(&token)).body(body2).send().await.unwrap();
    let path2 = rustic_git::registry::store::blob_path("acme", &d2);
    let before2 = e.store.os.head(&path2).await.unwrap().last_modified;
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let r = c.post(format!("{base}/v2/acme/api/blobs/uploads/?mount={d2}&from=acme/nginx"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);
    let after2 = e.store.os.head(&path2).await.unwrap().last_modified;
    assert!(after2 > before2, "mount should have refreshed the blob's mtime past its grace-aged value");

    std::env::remove_var("RUSTIC_GIT_BLOB_GRACE_SECS");
}

/// Two PATCHes racing the same session both read `have`, both append from that offset, and
/// last-writer-wins clobbers the other's bytes — dropped without the per-session lock in
/// uploads.rs. Deterministic concurrency is hard to force over HTTP, so this pins the simpler,
/// still load-bearing half: a chunk whose declared start doesn't match what the session actually
/// holds is refused with 416, not silently accepted or a confusing digest failure downstream.
#[tokio::test]
async fn patch_with_mismatched_start_is_416() {
    let (base, _e, c, token) = authed().await;
    let r = c.post(format!("{base}/v2/acme/nginx/blobs/uploads/"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::ACCEPTED);
    let uuid = r.headers().get("docker-upload-uuid").unwrap().to_str().unwrap().to_string();

    // Session has 0 bytes; claim the chunk starts at byte 5.
    let r = c.patch(format!("{base}/v2/acme/nginx/blobs/uploads/{uuid}"))
        .header("content-range", "bytes 5-9")
        .basic_auth("acme", Some(&token)).body(b"abcde".to_vec()).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::RANGE_NOT_SATISFIABLE);
}

/// `bump_pulls` is a read-increment-write; without per-key serialization, concurrent pulls of the
/// same tag on one node lose increments (each racing writer overwrites with its own stale `n+1`).
#[tokio::test]
async fn concurrent_pulls_count_every_hit() {
    let (_base, e, _c, _token) = authed().await;
    let n = 50usize;
    let mut tasks = Vec::new();
    for _ in 0..n {
        let store = e.store.clone();
        tasks.push(tokio::spawn(async move { store.bump_pulls("acme", "nginx", "latest").await }));
    }
    for t in tasks {
        t.await.unwrap().unwrap();
    }
    assert_eq!(e.store.pulls("acme", "nginx", "latest").await.unwrap(), n as u64);
}
