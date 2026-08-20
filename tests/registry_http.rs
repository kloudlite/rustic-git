mod common;
use axum::http::StatusCode;

// Spins the public router on an ephemeral port and returns its base URL.
// (Mirror the harness in tests/http_e2e.rs — reuse its helper rather than writing a second one.)
async fn serve() -> (String, common::TestEnv) { common::serve_public().await }

#[tokio::test]
async fn v2_root_says_the_api_version() {
    let (base, _e) = serve().await;
    let r = reqwest::get(format!("{base}/v2/")).await.unwrap();
    // Anonymous: 401 with a challenge is correct and so is 200. This registry challenges.
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    let www = r.headers().get("www-authenticate").unwrap().to_str().unwrap();
    assert!(www.starts_with("Bearer realm="), "got {www}");
    assert!(www.contains("/v2/token"), "the realm must point at the token endpoint: {www}");
    assert_eq!(r.headers().get("docker-distribution-api-version").unwrap(), "registry/2.0");
}

#[tokio::test]
async fn v2_root_with_a_token_is_200() {
    let (base, e) = serve().await;
    let token = e.store.create_token("acme").await.unwrap();
    let r = reqwest::Client::new()
        .get(format!("{base}/v2/"))
        .basic_auth("acme", Some(&token))
        .send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
}

// Controller ruling (task-3-brief.md): errors_use_the_oci_envelope and
// a_stranger_cannot_read_a_private_image both target manifests/tags routes that no task has
// written yet (Task 8). In their place: one envelope test against an unrouted /v2 path, which
// exercises oci_err through Task 1's middleware in src/http.rs.
#[tokio::test]
async fn unrouted_v2_path_uses_the_oci_envelope() {
    let (base, _e) = serve().await;
    let r = reqwest::get(format!("{base}/v2/acme/nginx/frobnicate")).await.unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    let body: serde_json::Value = serde_json::from_str(&r.text().await.unwrap()).unwrap();
    assert_eq!(body["errors"][0]["code"], "NAME_UNKNOWN");
}
