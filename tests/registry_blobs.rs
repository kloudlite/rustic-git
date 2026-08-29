mod common;
use axum::http::StatusCode;
use rustic_git_registry::store::ImageExt;
use rustic_git_registry::Digest;

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

/// The Basic username must be the owner the token belongs to. The token is the secret, but a
/// credential whose two halves disagree is a credential that did not verify — a refusal.
#[tokio::test]
async fn basic_auth_with_the_wrong_username_is_refused() {
    let (base, _e, c, token) = authed().await;
    let r = c.get(format!("{base}/v2/")).basic_auth("bob", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    let r = c.get(format!("{base}/v2/")).basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
}

/// RFC 7235: the auth scheme is case-insensitive.
#[tokio::test]
async fn a_lowercase_basic_scheme_is_accepted() {
    use base64::Engine;
    let (base, _e, c, token) = authed().await;
    let cred = base64::engine::general_purpose::STANDARD.encode(format!("acme:{token}"));
    let r = c.get(format!("{base}/v2/")).header("authorization", format!("basic {cred}")).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
}

/// docker probes with HEAD before every pull; a public image must answer one anonymously.
#[tokio::test]
async fn an_anonymous_head_of_a_public_blob_is_200() {
    let (base, e, c, token) = authed().await;
    let body = b"public layer".to_vec();
    let d = Digest::of(&body);
    c.post(format!("{base}/v2/acme/nginx/blobs/uploads/?digest={d}"))
        .basic_auth("acme", Some(&token)).body(body.clone()).send().await.unwrap();
    e.store.set_image_visibility("acme", "nginx", true).await.unwrap();
    let r = c.head(format!("{base}/v2/acme/nginx/blobs/{d}")).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.headers().get("content-length").unwrap().to_str().unwrap(), body.len().to_string());
}

/// Deleting is a write: anonymous gets the challenge, a stranger gets denied, and the blob stays.
#[tokio::test]
async fn deleting_a_blob_requires_the_owner() {
    let (base, e, c, token) = authed().await;
    let body = b"keep me".to_vec();
    let d = Digest::of(&body);
    c.post(format!("{base}/v2/acme/nginx/blobs/uploads/?digest={d}"))
        .basic_auth("acme", Some(&token)).body(body).send().await.unwrap();
    e.store.set_image_visibility("acme", "nginx", true).await.unwrap();

    let r = c.delete(format!("{base}/v2/acme/nginx/blobs/{d}")).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    let other = e.store.create_token("other").await.unwrap();
    let r = c.delete(format!("{base}/v2/acme/nginx/blobs/{d}")).basic_auth("other", Some(&other)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    let r = c.head(format!("{base}/v2/acme/nginx/blobs/{d}")).basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK, "the blob must survive both refusals");
}

const MEDIA: &str = "application/vnd.oci.image.manifest.v1+json";

fn manifest_naming(digests: &[&Digest]) -> Vec<u8> {
    let layers: Vec<_> = digests.iter().map(|d| serde_json::json!({"mediaType": "application/vnd.oci.image.layer.v1.tar+gzip", "digest": d.to_string(), "size": 1})).collect();
    serde_json::json!({"schemaVersion": 2, "mediaType": MEDIA, "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": digests[0].to_string(), "size": 1}, "layers": layers})
        .to_string().into_bytes()
}

/// Blob bytes are per owner, so the only thing keeping a public image from serving a private
/// sibling's layers is the per-image row: a layer both hold is served through the public name, a
/// layer only the private one holds is BLOB_UNKNOWN through it — not DENIED, which would confirm
/// the digest exists.
#[tokio::test]
async fn a_public_image_does_not_serve_a_private_siblings_layers() {
    let (base, e, c, token) = authed().await;
    let shared = Digest::of(b"base layer");
    let secret = Digest::of(b"private-only layer");
    for (image, bytes, d) in [("nginx", b"base layer".as_slice(), &shared), ("api", b"private-only layer".as_slice(), &secret)] {
        let r = c.post(format!("{base}/v2/acme/{image}/blobs/uploads/?digest={d}"))
            .basic_auth("acme", Some(&token)).body(bytes.to_vec()).send().await.unwrap();
        assert_eq!(r.status(), StatusCode::CREATED);
    }
    for (image, named) in [("nginx", vec![&shared]), ("api", vec![&shared, &secret])] {
        let r = c.put(format!("{base}/v2/acme/{image}/manifests/latest"))
            .basic_auth("acme", Some(&token)).header("content-type", MEDIA)
            .body(manifest_naming(&named)).send().await.unwrap();
        assert_eq!(r.status(), StatusCode::CREATED, "{image}");
    }
    e.store.set_image_visibility("acme", "nginx", true).await.unwrap();

    let r = c.get(format!("{base}/v2/acme/nginx/blobs/{shared}")).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK, "the public image holds the shared layer");
    // The private image refuses at the door, before any blob question is asked.
    let r = c.get(format!("{base}/v2/acme/api/blobs/{shared}")).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    let r = c.get(format!("{base}/v2/acme/api/blobs/{secret}")).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    let r = c.get(format!("{base}/v2/acme/nginx/blobs/{secret}")).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND, "not 403: the public URL must not confirm the digest");
    let r = c.head(format!("{base}/v2/acme/nginx/blobs/{secret}")).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    // A logged-in stranger gets the same answer as anonymous.
    let other = e.store.create_token("other").await.unwrap();
    let r = c.get(format!("{base}/v2/acme/nginx/blobs/{secret}")).basic_auth("other", Some(&other)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);

    for image in ["nginx", "api"] {
        for d in [&shared, &secret] {
            let r = c.get(format!("{base}/v2/acme/{image}/blobs/{d}")).basic_auth("acme", Some(&token)).send().await.unwrap();
            assert_eq!(r.status(), StatusCode::OK, "the owner pulls everything: {image} {d}");
        }
    }
}

/// An image pushed before the rows existed has none: the first stranger's pull derives them from
/// the manifests it already has, so a public image keeps serving its own layers — and still not
/// a sibling's.
#[tokio::test]
async fn an_image_without_blob_rows_is_backfilled_from_its_manifests() {
    let (base, e, c, _token) = authed().await;
    common::seed_blobs(&e, "acme", &[b"old layer", b"someone else's layer"]).await;
    let (mine, theirs) = (Digest::of(b"old layer"), Digest::of(b"someone else's layer"));
    let m = manifest_naming(&[&mine]);
    use slatedb::object_store::ObjectStoreExt;
    e.store.os
        .put(&rustic_git_registry::store::manifest_path("acme", "nginx", &Digest::of(&m)), slatedb::object_store::PutPayload::from(m))
        .await.unwrap();
    e.store.set_image_visibility("acme", "nginx", true).await.unwrap();

    let r = c.get(format!("{base}/v2/acme/nginx/blobs/{mine}")).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let r = c.get(format!("{base}/v2/acme/nginx/blobs/{theirs}")).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
}
