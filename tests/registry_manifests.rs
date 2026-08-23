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
    // Every blob a manifest in this file names: `manifest()`'s two, the empty config the referrer
    // tests use, and the second config `a_push_refreshes_the_image_marker` pushes.
    common::seed_blobs(&e, "acme", &[b"cfg", b"layer", b"{}", b"cfg2"]).await;
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

/// Deleting a manifest by digest must also drop its `image/manifest-type/{d}`
/// row — otherwise it orphans forever (never read again, never swept).
#[tokio::test]
async fn deleting_a_manifest_by_digest_drops_its_media_type_row() {
    let (base, e, c, token, m, d) = pushed().await;
    c.put(format!("{base}/v2/acme/nginx/manifests/latest"))
        .basic_auth("acme", Some(&token)).header("content-type", MEDIA)
        .body(m.clone()).send().await.unwrap();

    let db = e.store.image_db("acme", "nginx").await.unwrap();
    let key = format!("image/manifest-type/{d}").into_bytes();
    assert!(db.get(key.clone()).await.unwrap().is_some(), "row exists before delete");

    let r = c.delete(format!("{base}/v2/acme/nginx/manifests/{d}"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::ACCEPTED);

    assert!(db.get(key).await.unwrap().is_none(), "the media-type row must not be orphaned");
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

#[tokio::test]
async fn a_manifest_with_a_subject_is_listed_as_its_referrer() {
    let (base, _e, c, token, m, subject) = pushed().await;
    c.put(format!("{base}/v2/acme/nginx/manifests/latest"))
        .basic_auth("acme", Some(&token)).header("content-type", MEDIA)
        .body(m).send().await.unwrap();

    let sig = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": MEDIA,
        "artifactType": "application/vnd.example.signature",
        "config": {"mediaType": "application/vnd.oci.empty.v1+json", "digest": Digest::of(b"{}").to_string(), "size": 2},
        "layers": [],
        "subject": {"mediaType": MEDIA, "digest": subject.to_string(), "size": 1}
    }).to_string().into_bytes();
    let sig_d = Digest::of(&sig);
    let r = c.put(format!("{base}/v2/acme/nginx/manifests/{sig_d}"))
        .basic_auth("acme", Some(&token)).header("content-type", MEDIA)
        .body(sig.clone()).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);

    let r = c.get(format!("{base}/v2/acme/nginx/referrers/{subject}"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(
        r.headers().get("content-type").unwrap().to_str().unwrap(),
        "application/vnd.oci.image.index.v1+json"
    );
    let b: serde_json::Value = r.json().await.unwrap();
    assert_eq!(b["manifests"][0]["digest"], sig_d.to_string());
    assert_eq!(b["manifests"][0]["artifactType"], "application/vnd.example.signature");

    // Filtered, and the filter is announced.
    let r = c.get(format!("{base}/v2/acme/nginx/referrers/{subject}?artifactType=application/vnd.other"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert!(r.headers().get("oci-filters-applied").is_some());
    let b: serde_json::Value = r.json().await.unwrap();
    assert_eq!(b["manifests"], serde_json::json!([]));
}

#[tokio::test]
async fn referrers_of_an_unreferenced_digest_is_an_empty_index() {
    let (base, _e, c, token, _m, _d) = pushed().await;
    let d = Digest::of(b"nothing points here");
    let r = c.get(format!("{base}/v2/acme/nginx/referrers/{d}"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    // Empty, not 404: the spec is explicit about this.
    assert_eq!(r.status(), StatusCode::OK);
    let b: serde_json::Value = r.json().await.unwrap();
    assert_eq!(b["manifests"], serde_json::json!([]));
}

#[tokio::test]
async fn the_catalog_lists_only_what_the_caller_may_see() {
    let (base, e, c, token, m, _d) = pushed().await;
    for image in ["nginx", "api"] {
        c.put(format!("{base}/v2/acme/{image}/manifests/latest"))
            .basic_auth("acme", Some(&token)).header("content-type", MEDIA)
            .body(m.clone()).send().await.unwrap();
    }
    common::seed_blobs(&e, "other", &[b"cfg", b"layer"]).await;
    let other = e.store.create_token("other").await.unwrap();
    c.put(format!("{base}/v2/other/secret/manifests/latest"))
        .basic_auth("other", Some(&other)).header("content-type", MEDIA)
        .body(m.clone()).send().await.unwrap();

    let r = c.get(format!("{base}/v2/_catalog"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    let b: serde_json::Value = r.json().await.unwrap();
    assert_eq!(b["repositories"], serde_json::json!(["acme/api", "acme/nginx"]));
}

/// A pull is a manifest GET by tag. HEADs and digest GETs must not count — docker probes with
/// HEAD and re-reads by digest, so counting those would inflate every pull to three.
#[tokio::test]
async fn a_pull_counts_once_and_probes_do_not() {
    let (base, e, c, token, m, d) = pushed().await;
    c.put(format!("{base}/v2/acme/nginx/manifests/latest"))
        .basic_auth("acme", Some(&token)).header("content-type", MEDIA)
        .body(m).send().await.unwrap();

    c.get(format!("{base}/v2/acme/nginx/manifests/latest"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    c.head(format!("{base}/v2/acme/nginx/manifests/latest"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    c.get(format!("{base}/v2/acme/nginx/manifests/{d}"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();

    assert_eq!(e.store.pulls("acme", "nginx", "latest").await.unwrap(), 1);
}

/// Spec: a manifest carrying a `subject` MUST get `OCI-Subject` on the 201, and one without must
/// NOT carry the header at all.
#[tokio::test]
async fn a_manifest_with_a_subject_gets_the_oci_subject_header_on_push() {
    let (base, _e, c, token, m, subject) = pushed().await;
    c.put(format!("{base}/v2/acme/nginx/manifests/latest"))
        .basic_auth("acme", Some(&token)).header("content-type", MEDIA)
        .body(m).send().await.unwrap();

    let sig = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": MEDIA,
        "layers": [],
        "subject": {"mediaType": MEDIA, "digest": subject.to_string(), "size": 1}
    }).to_string().into_bytes();
    let sig_d = Digest::of(&sig);
    let r = c.put(format!("{base}/v2/acme/nginx/manifests/{sig_d}"))
        .basic_auth("acme", Some(&token)).header("content-type", MEDIA)
        .body(sig).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);
    assert_eq!(r.headers().get("oci-subject").unwrap().to_str().unwrap(), subject.to_string());

    // No subject: no header at all.
    let plain = serde_json::json!({"schemaVersion": 2, "mediaType": MEDIA, "layers": []})
        .to_string().into_bytes();
    let r = c.put(format!("{base}/v2/acme/nginx/manifests/v2"))
        .basic_auth("acme", Some(&token)).header("content-type", MEDIA)
        .body(plain).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);
    assert!(r.headers().get("oci-subject").is_none());
}

/// `MAX_MANIFEST` is 4 MiB but axum's `DefaultBodyLimit` default is 2 MB — without an explicit
/// limit on the manifest route, a legal ~3 MB manifest would 413 before `put_manifest` ever runs.
#[tokio::test]
async fn a_manifest_over_two_megabytes_but_under_the_limit_is_accepted() {
    let (base, _e, c, token, _m, _d) = pushed().await;
    let padding = "x".repeat(3 * 1024 * 1024);
    let big = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": MEDIA,
        "layers": [],
        "annotations": {"padding": padding}
    }).to_string().into_bytes();
    assert!(big.len() > 2 * 1024 * 1024, "the test must actually exceed axum's 2 MB default");
    let d = Digest::of(&big);
    let r = c.put(format!("{base}/v2/acme/nginx/manifests/{d}"))
        .basic_auth("acme", Some(&token)).header("content-type", MEDIA)
        .body(big.clone()).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::CREATED, "body: {}", r.text().await.unwrap());

    let r = c.get(format!("{base}/v2/acme/nginx/manifests/{d}"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.bytes().await.unwrap().to_vec(), big, "manifest bytes must round-trip byte-identical");
}

#[tokio::test]
async fn a_push_refreshes_the_image_marker() {
    let (base, e, c, token, m, _d) = pushed().await;
    let r = c.put(format!("{base}/v2/acme/nginx/manifests/latest"))
        .basic_auth("acme", Some(&token)).header("content-type", MEDIA)
        .body(m.clone()).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);

    let marker = rustic_git::index::read(&e.store.os, rustic_git::index::Kind::Img, "acme", "nginx")
        .await
        .expect("first push must create the marker");
    assert!(!marker.public, "a first push must create the marker private, fail closed");
    assert_eq!(marker.manifests, 1);
    let first_updated = marker.updated_ms;

    // Second manifest, different bytes, so it's a second object under manifests/acme/nginx/.
    let m2 = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": MEDIA,
        "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": Digest::of(b"cfg2").to_string(), "size": 4},
        "layers": []
    }).to_string().into_bytes();
    let r = c.put(format!("{base}/v2/acme/nginx/manifests/other"))
        .basic_auth("acme", Some(&token)).header("content-type", MEDIA)
        .body(m2).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);

    let marker = rustic_git::index::read(&e.store.os, rustic_git::index::Kind::Img, "acme", "nginx")
        .await
        .expect("marker must still exist after the second push");
    assert_eq!(marker.manifests, 2);
    assert!(marker.updated_ms >= first_updated);
}

/// A HEAD must carry the same `Content-Length` a GET would.
///
/// The OCI spec requires it and clients lean on it: without it a client cannot learn the manifest's
/// size from a probe, so it falls back to a full GET — real clients log
/// "HEAD request failed, falling back on GET" and pay a second round trip on every check.
#[tokio::test]
async fn a_head_carries_the_same_content_length_as_a_get() {
    let (base, _e, c, token, m, _d) = pushed().await;
    c.put(format!("{base}/v2/acme/nginx/manifests/latest"))
        .basic_auth("acme", Some(&token))
        .header("content-type", MEDIA)
        .body(m.clone())
        .send()
        .await
        .unwrap();

    let g = c
        .get(format!("{base}/v2/acme/nginx/manifests/latest"))
        .basic_auth("acme", Some(&token))
        .send()
        .await
        .unwrap();
    let get_len = g.headers().get("content-length").cloned();
    assert_eq!(
        get_len.as_ref().map(|v| v.to_str().unwrap().to_string()),
        Some(m.len().to_string()),
        "a GET must report the manifest's length"
    );

    let h = c
        .head(format!("{base}/v2/acme/nginx/manifests/latest"))
        .basic_auth("acme", Some(&token))
        .send()
        .await
        .unwrap();
    assert_eq!(h.status(), StatusCode::OK);
    assert_eq!(
        h.headers().get("content-length").map(|v| v.to_str().unwrap().to_string()),
        Some(m.len().to_string()),
        "a HEAD must report the length a GET would, not omit it and not zero"
    );
    // The other two headers a probing client reads must survive too.
    assert_eq!(h.headers().get("content-type").unwrap().to_str().unwrap(), MEDIA);
    assert!(h.headers().get("docker-content-digest").is_some());
}

/// Spec: a manifest naming a blob the registry does not hold is refused with
/// `MANIFEST_BLOB_UNKNOWN`. Before this, a slow push whose early layers the grace sweep had
/// removed still got a 201 — and a broken image.
#[tokio::test]
async fn a_manifest_naming_a_missing_blob_is_manifest_blob_unknown() {
    let (base, _e, c, token, _m, _d) = pushed().await;
    let m = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": MEDIA,
        "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": Digest::of(b"cfg").to_string(), "size": 3},
        "layers": [{"mediaType": "application/vnd.oci.image.layer.v1.tar+gzip", "digest": Digest::of(b"never pushed").to_string(), "size": 12}]
    }).to_string().into_bytes();
    let r = c.put(format!("{base}/v2/acme/nginx/manifests/latest"))
        .basic_auth("acme", Some(&token)).header("content-type", MEDIA)
        .body(m).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    let b: serde_json::Value = r.json().await.unwrap();
    assert_eq!(b["errors"][0]["code"], "MANIFEST_BLOB_UNKNOWN");
    // Nothing landed: the tag does not resolve.
    let r = c.get(format!("{base}/v2/acme/nginx/manifests/latest"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
}

/// An index names manifests, not blobs — `manifests[].digest` must be looked up where manifests
/// live. And `subject` is exempt: the spec allows a referrer to arrive before its subject.
#[tokio::test]
async fn an_index_entry_is_looked_up_as_a_manifest_and_subject_is_exempt() {
    let (base, _e, c, token, m, d) = pushed().await;
    let r = c.put(format!("{base}/v2/acme/nginx/manifests/{d}"))
        .basic_auth("acme", Some(&token)).header("content-type", MEDIA)
        .body(m).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);

    let index = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": [{"mediaType": MEDIA, "digest": d.to_string(), "size": 1, "platform": {"architecture": "amd64", "os": "linux"}}],
        "subject": {"mediaType": MEDIA, "digest": Digest::of(b"not here yet").to_string(), "size": 1}
    }).to_string().into_bytes();
    let r = c.put(format!("{base}/v2/acme/nginx/manifests/multi"))
        .basic_auth("acme", Some(&token)).header("content-type", "application/vnd.oci.image.index.v1+json")
        .body(index).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::CREATED, "body: {}", r.text().await.unwrap());
}

/// A foreign/nondistributable layer (Windows base images) is fetched from its `urls`, never from
/// this registry — the existence check must skip it or every such push 404s.
#[tokio::test]
async fn a_foreign_layer_is_not_required_to_be_present() {
    let (base, _e, c, token, _m, _d) = pushed().await;
    let m = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": MEDIA,
        "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": Digest::of(b"cfg").to_string(), "size": 3},
        "layers": [
            {"mediaType": "application/vnd.oci.image.layer.v1.tar+gzip", "digest": Digest::of(b"layer").to_string(), "size": 5},
            {"mediaType": "application/vnd.docker.image.rootfs.foreign.diff.tar.gzip",
             "digest": Digest::of(b"windows base").to_string(), "size": 9,
             "urls": ["https://mcr.microsoft.com/v2/windows/blobs/sha256:x"]}
        ]
    }).to_string().into_bytes();
    let r = c.put(format!("{base}/v2/acme/nginx/manifests/win"))
        .basic_auth("acme", Some(&token)).header("content-type", MEDIA)
        .body(m).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::CREATED, "body: {}", r.text().await.unwrap());
}

/// A manifest that is not JSON cannot be walked for the blobs it names, so the GC sweep would
/// have to abort for this owner forever. Refuse it at the door instead.
#[tokio::test]
async fn a_manifest_that_is_not_json_is_manifest_invalid() {
    let (base, _e, c, token, _m, _d) = pushed().await;
    let r = c.put(format!("{base}/v2/acme/nginx/manifests/latest"))
        .basic_auth("acme", Some(&token)).header("content-type", MEDIA)
        .body(b"not json at all".to_vec()).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    let b: serde_json::Value = r.json().await.unwrap();
    assert_eq!(b["errors"][0]["code"], "MANIFEST_INVALID");

    // Valid JSON that is not an object is no more walkable than garbage, and must be refused too.
    let r = c.put(format!("{base}/v2/acme/nginx/manifests/latest"))
        .basic_auth("acme", Some(&token)).header("content-type", MEDIA)
        .body(b"[]".to_vec()).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    let b: serde_json::Value = r.json().await.unwrap();
    assert_eq!(b["errors"][0]["code"], "MANIFEST_INVALID");
}

/// A client that pushed by sha512 digest and then pushes the same bytes by tag must get the tag
/// pointed at the sha512 identity it already uses — not a freshly minted sha256 one.
#[tokio::test]
async fn a_tag_push_after_a_sha512_digest_push_keeps_the_sha512_identity() {
    let (base, _e, c, token, m, _d) = pushed().await;
    let d512 = Digest::of_algo("sha512", &m).unwrap();
    let r = c.put(format!("{base}/v2/acme/nginx/manifests/{d512}"))
        .basic_auth("acme", Some(&token)).header("content-type", MEDIA)
        .body(m.clone()).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::CREATED, "body: {}", r.text().await.unwrap());

    let r = c.put(format!("{base}/v2/acme/nginx/manifests/latest"))
        .basic_auth("acme", Some(&token)).header("content-type", MEDIA)
        .body(m).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);
    assert_eq!(r.headers().get("docker-content-digest").unwrap().to_str().unwrap(), d512.to_string());

    let r = c.get(format!("{base}/v2/acme/nginx/manifests/latest"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.headers().get("docker-content-digest").unwrap().to_str().unwrap(), d512.to_string());
}

/// Opening an image's database creates it. A DELETE aimed at an image that was never pushed must
/// 404 without leaving a phantom image behind for the listing to find.
#[tokio::test]
async fn deleting_a_manifest_of_a_missing_image_creates_nothing() {
    let (base, e, c, token, _m, d) = pushed().await;
    let r = c.delete(format!("{base}/v2/acme/ghost/manifests/{d}"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    let b: serde_json::Value = r.json().await.unwrap();
    assert_eq!(b["errors"][0]["code"], "MANIFEST_UNKNOWN");
    assert!(!e.store.pool.exists("img", "acme/ghost").await.unwrap(), "a DELETE must not create the image");
}

/// Spec: `tags/list` for a repository that does not exist is `NAME_UNKNOWN`, not an empty list.
#[tokio::test]
async fn tags_list_of_a_missing_image_is_name_unknown() {
    let (base, _e, c, token, _m, _d) = pushed().await;
    let r = c.get(format!("{base}/v2/acme/ghost/tags/list"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    let b: serde_json::Value = r.json().await.unwrap();
    assert_eq!(b["errors"][0]["code"], "NAME_UNKNOWN");
}

/// Spec: `artifactType` is omitted from a referrers entry when the manifest has neither it nor a
/// config media type — never emitted as `null`.
#[tokio::test]
async fn a_referrer_without_an_artifact_type_omits_the_field() {
    let (base, _e, c, token, m, subject) = pushed().await;
    c.put(format!("{base}/v2/acme/nginx/manifests/latest"))
        .basic_auth("acme", Some(&token)).header("content-type", MEDIA)
        .body(m).send().await.unwrap();
    let sig = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": MEDIA,
        "layers": [],
        "subject": {"mediaType": MEDIA, "digest": subject.to_string(), "size": 1}
    }).to_string().into_bytes();
    let sig_d = Digest::of(&sig);
    let r = c.put(format!("{base}/v2/acme/nginx/manifests/{sig_d}"))
        .basic_auth("acme", Some(&token)).header("content-type", MEDIA)
        .body(sig).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);

    let r = c.get(format!("{base}/v2/acme/nginx/referrers/{subject}"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    let b: serde_json::Value = r.json().await.unwrap();
    let entry = &b["manifests"][0];
    assert_eq!(entry["digest"], sig_d.to_string());
    assert!(entry.get("artifactType").is_none(), "got {entry}");

    // And deleting the referrer by digest unindexes it.
    let r = c.delete(format!("{base}/v2/acme/nginx/manifests/{sig_d}"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::ACCEPTED);
    let r = c.get(format!("{base}/v2/acme/nginx/referrers/{subject}"))
        .basic_auth("acme", Some(&token)).send().await.unwrap();
    let b: serde_json::Value = r.json().await.unwrap();
    assert_eq!(b["manifests"], serde_json::json!([]));
}

/// `_catalog` pages exactly like `tags/list`: `n` caps, `last` is exclusive, a truncated page
/// carries `Link`. `n=0` is an empty page — nothing to continue from, so no `Link` either.
#[tokio::test]
async fn the_catalog_paginates() {
    let (base, _e, c, token, m, _d) = pushed().await;
    for image in ["api", "nginx", "web"] {
        c.put(format!("{base}/v2/acme/{image}/manifests/latest"))
            .basic_auth("acme", Some(&token)).header("content-type", MEDIA)
            .body(m.clone()).send().await.unwrap();
    }
    let r = c.get(format!("{base}/v2/_catalog?n=2")).basic_auth("acme", Some(&token)).send().await.unwrap();
    assert!(r.headers().get("link").unwrap().to_str().unwrap().contains("last=acme/nginx"));
    let b: serde_json::Value = r.json().await.unwrap();
    assert_eq!(b["repositories"], serde_json::json!(["acme/api", "acme/nginx"]));

    let r = c.get(format!("{base}/v2/_catalog?n=2&last=acme/nginx")).basic_auth("acme", Some(&token)).send().await.unwrap();
    assert!(r.headers().get("link").is_none());
    let b: serde_json::Value = r.json().await.unwrap();
    assert_eq!(b["repositories"], serde_json::json!(["acme/web"]));

    let r = c.get(format!("{base}/v2/_catalog?n=0")).basic_auth("acme", Some(&token)).send().await.unwrap();
    assert!(r.headers().get("link").is_none());
    let b: serde_json::Value = r.json().await.unwrap();
    assert_eq!(b["repositories"], serde_json::json!([]));
}
