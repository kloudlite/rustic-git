mod common;
use axum::http::StatusCode;
use rustic_git::registry::Digest;

const MEDIA: &str = "application/vnd.oci.image.manifest.v1+json";

fn manifest() -> Vec<u8> {
    serde_json::json!({
        "schemaVersion": 2,
        "mediaType": MEDIA,
        "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": Digest::of(b"cfg").to_string(), "size": 3},
        "layers": [{"mediaType": "application/vnd.oci.image.layer.v1.tar+gzip", "digest": Digest::of(b"layer").to_string(), "size": 5}]
    }).to_string().into_bytes()
}

async fn pushed() -> (String, common::TestEnv, reqwest::Client, String, Vec<u8>, Digest) {
    let (base, e) = common::serve_public().await;
    let c = reqwest::Client::new();
    let token = e.store.create_token("acme").await.unwrap();
    let m = manifest();
    let d = Digest::of(&m);
    (base, e, c, token, m, d)
}

#[tokio::test]
async fn a_manifest_pushed_by_tag_comes_back_by_tag_and_by_digest() {
    let (base, _e, c, token, m, d) = pushed().await;
    let r = c.put(format!("{base}/v2/acme/nginx/manifests/latest"))
        .basic_auth("acme", Some(&token)).header("content-type", MEDIA)
        .body(m.clone()).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);
    assert_eq!(r.headers().get("docker-content-digest").unwrap().to_str().unwrap(), d.to_string());
    assert_eq!(r.headers().get("location").unwrap().to_str().unwrap(), format!("/v2/acme/nginx/manifests/{d}"));

    for reference in ["latest", &d.to_string()] {
        let r = c.get(format!("{base}/v2/acme/nginx/manifests/{reference}"))
            .basic_auth("acme", Some(&token)).send().await.unwrap();
        assert_eq!(r.status(), StatusCode::OK, "reading {reference}");
        assert_eq!(r.headers().get("content-type").unwrap().to_str().unwrap(), MEDIA);
        assert_eq!(r.headers().get("docker-content-digest").unwrap().to_str().unwrap(), d.to_string());
        assert_eq!(r.bytes().await.unwrap().to_vec(), m, "bytes must be byte-identical: the digest is over them");
    }
}

#[tokio::test]
async fn a_manifest_put_by_digest_that_does_not_match_is_refused() {
    let (base, _e, c, token, m, _d) = pushed().await;
    let lie = Digest::of(b"different");
    let r = c.put(format!("{base}/v2/acme/nginx/manifests/{lie}"))
        .basic_auth("acme", Some(&token)).header("content-type", MEDIA)
        .body(m).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    let b: serde_json::Value = r.json().await.unwrap();
    assert_eq!(b["errors"][0]["code"], "DIGEST_INVALID");
}

#[tokio::test]
async fn an_unknown_manifest_is_manifest_unknown() {
    let (base, _e, c, token, _m, _d) = pushed().await;
    let r = c.get(format!("{base}/v2/acme/nginx/manifests/nope"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    let b: serde_json::Value = r.json().await.unwrap();
    assert_eq!(b["errors"][0]["code"], "MANIFEST_UNKNOWN");
}

#[tokio::test]
async fn tags_list_sorts_and_paginates() {
    let (base, _e, c, token, m, _d) = pushed().await;
    for t in ["v3", "v1", "v2"] {
        c.put(format!("{base}/v2/acme/nginx/manifests/{t}"))
            .basic_auth("acme", Some(&token)).header("content-type", MEDIA)
            .body(m.clone()).send().await.unwrap();
    }
    let r = c.get(format!("{base}/v2/acme/nginx/tags/list"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    let b: serde_json::Value = r.json().await.unwrap();
    assert_eq!(b["name"], "acme/nginx");
    assert_eq!(b["tags"], serde_json::json!(["v1", "v2", "v3"]));

    let r = c.get(format!("{base}/v2/acme/nginx/tags/list?n=2"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert!(r.headers().get("link").is_some(), "a truncated list must carry a Link header");
    let b: serde_json::Value = r.json().await.unwrap();
    assert_eq!(b["tags"], serde_json::json!(["v1", "v2"]));

    let r = c.get(format!("{base}/v2/acme/nginx/tags/list?n=2&last=v2"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    let b: serde_json::Value = r.json().await.unwrap();
    assert_eq!(b["tags"], serde_json::json!(["v3"]));
}

#[tokio::test]
async fn deleting_a_tag_leaves_the_manifest_and_deleting_the_manifest_takes_its_tags() {
    let (base, _e, c, token, m, d) = pushed().await;
    for t in ["latest", "v1"] {
        c.put(format!("{base}/v2/acme/nginx/manifests/{t}"))
            .basic_auth("acme", Some(&token)).header("content-type", MEDIA)
            .body(m.clone()).send().await.unwrap();
    }
    let r = c.delete(format!("{base}/v2/acme/nginx/manifests/latest"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::ACCEPTED);
    // The tag is gone; the manifest and the other tag are not.
    assert_eq!(
        c.get(format!("{base}/v2/acme/nginx/manifests/latest")).basic_auth("acme", Some(&token))
            .send().await.unwrap().status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        c.get(format!("{base}/v2/acme/nginx/manifests/v1")).basic_auth("acme", Some(&token))
            .send().await.unwrap().status(),
        StatusCode::OK
    );

    // By digest: the manifest goes, and every tag pointing at it goes with it.
    let r = c.delete(format!("{base}/v2/acme/nginx/manifests/{d}"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::ACCEPTED);
    assert_eq!(
        c.get(format!("{base}/v2/acme/nginx/manifests/v1")).basic_auth("acme", Some(&token))
            .send().await.unwrap().status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn a_public_image_pulls_anonymously_and_still_refuses_a_push() {
    let (base, e, c, token, m, _d) = pushed().await;
    c.put(format!("{base}/v2/acme/nginx/manifests/latest"))
        .basic_auth("acme", Some(&token)).header("content-type", MEDIA)
        .body(m.clone()).send().await.unwrap();
    e.store.set_image_visibility("acme", "nginx", true).await.unwrap();

    let r = c.get(format!("{base}/v2/acme/nginx/manifests/latest")).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let r = c.put(format!("{base}/v2/acme/nginx/manifests/v9"))
        .header("content-type", MEDIA).body(m).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn two_pushes_to_one_tag_leave_it_pointing_at_exactly_one_of_them() {
    let (base, _e, c, token, _m, _d) = pushed().await;
    let a = serde_json::json!({"schemaVersion": 2, "mediaType": MEDIA, "layers": [], "annotations": {"who": "a"}})
        .to_string().into_bytes();
    let b = serde_json::json!({"schemaVersion": 2, "mediaType": MEDIA, "layers": [], "annotations": {"who": "b"}})
        .to_string().into_bytes();
    let (da, db) = (Digest::of(&a), Digest::of(&b));
    let put = |body: Vec<u8>| {
        let (c, base, token) = (c.clone(), base.clone(), token.clone());
        async move {
            c.put(format!("{base}/v2/acme/nginx/manifests/latest"))
                .basic_auth("acme", Some(&token)).header("content-type", MEDIA)
                .body(body).send().await.unwrap().status()
        }
    };
    let (ra, rb) = tokio::join!(put(a), put(b));
    assert_eq!((ra, rb), (StatusCode::CREATED, StatusCode::CREATED));

    // Whichever won, the tag resolves to ONE of them and reading it twice agrees.
    let read = || async {
        let r = c.get(format!("{base}/v2/acme/nginx/manifests/latest"))
            .basic_auth("acme", Some(&token)).send().await.unwrap();
        r.headers().get("docker-content-digest").unwrap().to_str().unwrap().to_string()
    };
    let first = read().await;
    assert!(first == da.to_string() || first == db.to_string(), "got {first}");
    assert_eq!(read().await, first, "the tag must not flap between the two");
}

#[tokio::test]
async fn errors_use_the_oci_envelope() {
    let (base, _e) = common::serve_public().await;
    let c = reqwest::Client::new();
    let r = c.get(format!("{base}/v2/acme/nope/manifests/latest")).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    let b: serde_json::Value = r.json().await.unwrap();
    assert_eq!(b["errors"][0]["code"], "UNAUTHORIZED");
}

#[tokio::test]
async fn a_stranger_cannot_read_a_private_image() {
    let (base, e, c, token, m, _d) = pushed().await;
    c.put(format!("{base}/v2/acme/nginx/manifests/latest"))
        .basic_auth("acme", Some(&token)).header("content-type", MEDIA)
        .body(m).send().await.unwrap();
    let other = e.store.create_token("other").await.unwrap();
    let r = c.get(format!("{base}/v2/acme/nginx/tags/list"))
        .basic_auth("other", Some(&other)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    let b: serde_json::Value = r.json().await.unwrap();
    assert_eq!(b["errors"][0]["code"], "DENIED");
}
