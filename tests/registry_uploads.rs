mod common;
use axum::http::StatusCode;
use rustic_git::registry::Digest;

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

    // The session must not have advanced.
    let r = c.get(format!("{base}{loc}")).basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::NO_CONTENT);
    assert!(r.headers().get("range").is_none());
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
