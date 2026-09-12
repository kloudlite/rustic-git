//! `max_layer` is a process-global `OnceLock`, so these run in their own binary with a cap small
//! enough to trip from a test body. Every test sets the same value, but four threads racing
//! `set_var` is still a data race (2026-09-12), so the write goes through one `Once`: the first
//! caller writes it, the rest block until it has landed and only then start a server that reads it.
mod common;
use axum::http::StatusCode;
use kloudlite_registry::Digest;

const CAP: &str = "16";

async fn authed() -> (String, common::TestEnv, reqwest::Client, String) {
    static CAP_SET: std::sync::Once = std::sync::Once::new();
    CAP_SET.call_once(|| std::env::set_var("KLOUDLITE_MAX_LAYER", CAP));
    let (base, e) = common::serve_public().await;
    let token = e.store.create_token("acme").await.unwrap();
    (base, e, reqwest::Client::new(), token)
}

/// One byte over the layer cap is refused with the OCI envelope, and nothing lands — the cap is
/// enforced by the streaming writer, since axum's body limit does not cover the `Body` extractor.
#[tokio::test]
async fn an_oversized_single_shot_blob_is_413_and_stores_nothing() {
    let (base, _e, c, token) = authed().await;
    let body = vec![0u8; 17];
    let d = Digest::of(&body);
    let r = c
        .post(format!("{base}/v2/acme/nginx/blobs/uploads/?digest={d}"))
        .basic_auth("acme", Some(&token))
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let b: serde_json::Value = r.json().await.unwrap();
    assert_eq!(b["errors"][0]["code"], "SIZE_INVALID");
    let r = c
        .get(format!("{base}/v2/acme/nginx/blobs/{d}"))
        .basic_auth("acme", Some(&token))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
}

/// The cap is on the LAYER, not the chunk: a session grown past it one small chunk at a time is
/// refused on the chunk that crosses, and the session stays where it was.
#[tokio::test]
async fn a_chunked_upload_that_crosses_the_cap_is_413() {
    let (base, _e, c, token) = authed().await;
    let r = c
        .post(format!("{base}/v2/acme/nginx/blobs/uploads/"))
        .basic_auth("acme", Some(&token))
        .send()
        .await
        .unwrap();
    let loc = r.headers().get("location").unwrap().to_str().unwrap().to_string();
    let r = c
        .patch(format!("{base}{loc}"))
        .basic_auth("acme", Some(&token))
        .header("content-range", "0-9")
        .body(vec![1u8; 10])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::ACCEPTED);
    let r = c
        .patch(format!("{base}{loc}"))
        .basic_auth("acme", Some(&token))
        .header("content-range", "10-19")
        .body(vec![2u8; 10])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let r = c.get(format!("{base}{loc}")).basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(
        r.headers().get("range").unwrap().to_str().unwrap(),
        "0-9",
        "the refused chunk must not advance the session"
    );
}

/// The manifest route keeps its own, separate cap (`MAX_MANIFEST`, 4 MiB), enforced by the
/// handler after it has drained the body. A 405 is axum's own, and `oci_envelope` re-wraps it so
/// a client that parses every `/v2` error as the OCI envelope still can.
#[tokio::test]
async fn an_oversized_manifest_is_413() {
    let (base, _e, c, token) = authed().await;
    let body = vec![b'x'; 4 * 1024 * 1024 + 1];
    let r = c
        .put(format!("{base}/v2/acme/nginx/manifests/latest"))
        .basic_auth("acme", Some(&token))
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let v: serde_json::Value = r.json().await.expect("an OCI envelope, not axum's plain text");
    assert_eq!(v["errors"][0]["code"], "SIZE_INVALID");

    let r = c.delete(format!("{base}/v2/_catalog")).basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::METHOD_NOT_ALLOWED);
    let v: serde_json::Value = r.json().await.expect("a 405 is the envelope too");
    assert_eq!(v["errors"][0]["code"], "UNSUPPORTED");
}

/// The refusal must arrive AFTER the whole body has been read: a proxy in front is still writing
/// the body upstream when an early 413 closes the connection, and turns the broken pipe into a
/// 502. Sent as raw HTTP/1.1 in small writes, the way nginx streams it, so a server that answers
/// early and closes fails the write here instead of merely racing it.
#[tokio::test]
async fn an_oversized_manifest_is_drained_before_the_413() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (base, _e, _c, token) = authed().await;
    let addr = base.trim_start_matches("http://").to_string();
    let mut s = tokio::net::TcpStream::connect(&addr).await.unwrap();
    let body = vec![b'x'; 5 * 1024 * 1024];
    let auth = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, format!("acme:{token}"));
    let head = format!(
        "PUT /v2/acme/nginx/manifests/latest HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Basic {auth}\r\nContent-Type: application/vnd.oci.image.manifest.v1+json\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    s.write_all(head.as_bytes()).await.unwrap();
    for chunk in body.chunks(64 * 1024) {
        s.write_all(chunk).await.expect("the server must keep reading until the body is gone");
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
    let mut reply = Vec::new();
    tokio::time::timeout(std::time::Duration::from_secs(10), s.read_to_end(&mut reply)).await.ok();
    let reply = String::from_utf8_lossy(&reply);
    assert!(reply.starts_with("HTTP/1.1 413"), "got: {}", reply.lines().next().unwrap_or(""));
    assert!(reply.contains("SIZE_INVALID"));
}
