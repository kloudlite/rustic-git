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

#[tokio::test]
async fn the_token_endpoint_mints_a_usable_bearer() {
    let (base, e) = serve().await;
    let token = e.store.create_token("acme").await.unwrap();
    let r = reqwest::Client::new()
        .get(format!("{base}/v2/token?service=localhost&scope=repository:acme/nginx:pull,push"))
        .basic_auth("acme", Some(&token))
        .send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let body: serde_json::Value = r.json().await.unwrap();
    let bearer = body["token"].as_str().unwrap().to_string();
    // Both field names, because clients disagree about which one they read.
    assert_eq!(body["access_token"].as_str().unwrap(), bearer);
    assert!(body["expires_in"].as_u64().unwrap() > 0);
    // A STRING, and RFC 3339: docker decodes this field into a time.Time and refuses a number.
    let issued = body["issued_at"].as_str().expect("issued_at must be a JSON string");
    chrono::DateTime::parse_from_rfc3339(issued).expect("issued_at must be RFC 3339");

    let r = reqwest::Client::new()
        .get(format!("{base}/v2/"))
        .bearer_auth(&bearer)
        .send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
}

#[tokio::test]
async fn a_bad_credential_gets_no_token() {
    let (base, _e) = serve().await;
    let r = reqwest::Client::new()
        .get(format!("{base}/v2/token?scope=repository:acme/nginx:pull"))
        .basic_auth("acme", Some("not-a-token"))
        .send().await.unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_forged_bearer_is_refused() {
    let (base, _e) = serve().await;
    let r = reqwest::Client::new()
        .get(format!("{base}/v2/"))
        .bearer_auth("not.a.jwt")
        .send().await.unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
}

/// `images` must read the shared object store alone (no `image_db` call — see the handler's doc
/// comment in `browse_api.rs`), so what it counts is manifests actually written to the object
/// store, not tags in a database it must never open on an unrouted node.
async fn put_manifest_bytes(e: &common::TestEnv, owner: &str, name: &str, body: &[u8]) {
    use slatedb::object_store::ObjectStoreExt;
    let d = rustic_git::registry::Digest::of(body);
    e.store
        .os
        .put(
            &rustic_git::registry::store::manifest_path(owner, name, &d),
            slatedb::object_store::PutPayload::from(body.to_vec()),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn the_browse_api_lists_a_teams_images() {
    // The peer listener is where browse routes live; mirror tests/browse_http.rs's harness.
    let (base, e) = common::serve_peer().await;
    // `image_names` lists the `repo/img/{owner}/` object-store prefix, which only exists once the
    // image's database has been opened at least once — a real push does that via `put_manifest`;
    // here `put_tag` is the public entry point that does the same (`touch_image` is crate-private).
    e.store.put_tag("acme", "nginx", "latest", &rustic_git::registry::Digest::of(b"m1")).await.unwrap();
    put_manifest_bytes(&e, "acme", "nginx", b"m1").await;
    put_manifest_bytes(&e, "acme", "nginx", b"m2").await;
    // The `images` route checks the caller against `{owner}`, same as every other browse handler
    // in `browse_api.rs` — the api tier presents this header once it has verified the caller is a
    // member of `acme`. See `peer_get_as`.
    let r = common::peer_get_as(&base, "acme", "/api/acme/images").await;
    assert_eq!(r.status(), StatusCode::OK);
    let b: serde_json::Value = r.json().await.unwrap();
    assert_eq!(b[0]["name"], "nginx");
    assert_eq!(b[0]["manifests"], 2);
}

#[tokio::test]
async fn a_team_gets_none_of_another_teams_images() {
    let (base, e) = common::serve_peer().await;
    put_manifest_bytes(&e, "acme", "nginx", b"m1").await;
    let r = common::peer_get_as(&base, "umbrella", "/api/acme/images").await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_browse_api_lists_an_images_own_tags() {
    let (base, e) = common::serve_peer().await;
    e.store.put_tag("acme", "nginx", "latest", &rustic_git::registry::Digest::of(b"m")).await.unwrap();
    e.store.put_tag("acme", "nginx", "v1", &rustic_git::registry::Digest::of(b"m")).await.unwrap();
    let r = common::peer_get_as(&base, "acme", "/api/acme/nginx/imagetags").await;
    assert_eq!(r.status(), StatusCode::OK);
    let b: serde_json::Value = r.json().await.unwrap();
    let tags: Vec<&str> = b.as_array().unwrap().iter().map(|t| t["tag"].as_str().unwrap()).collect();
    assert!(tags.contains(&"latest"), "{tags:?}");
    assert!(tags.contains(&"v1"), "{tags:?}");
}

#[tokio::test]
async fn a_team_gets_none_of_another_teams_imagetags() {
    let (base, e) = common::serve_peer().await;
    e.store.put_tag("acme", "nginx", "latest", &rustic_git::registry::Digest::of(b"m")).await.unwrap();
    let r = common::peer_get_as(&base, "umbrella", "/api/acme/nginx/imagetags").await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
}

/// `imagetagdelete` removes ONE tag row (`store.delete_tag`) and nothing else: the manifest it
/// pointed at still fetches by digest, and a sibling tag on the same manifest is untouched.
#[tokio::test]
async fn deleting_a_tag_leaves_the_manifest_and_other_tags_alone() {
    let (pub_base, peer_base, e) = common::serve_public_and_peer().await;
    let token = e.store.create_token("acme").await.unwrap();
    let m = manifest_bytes();
    let d = rustic_git::registry::Digest::of(&m);
    let c = reqwest::Client::new();
    for tag in ["latest", "v1"] {
        let r = c
            .put(format!("{pub_base}/v2/acme/nginx/manifests/{tag}"))
            .basic_auth("acme", Some(&token))
            .header("content-type", MEDIA)
            .body(m.clone())
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::CREATED);
    }

    let r = common::peer_post_as(&peer_base, "acme", "/api/acme/nginx/imagetagdelete", "latest").await;
    assert_eq!(r.status(), StatusCode::NO_CONTENT);

    let tags = e.store.tags("acme", "nginx").await.unwrap();
    assert_eq!(tags, vec!["v1".to_string()], "the deleted tag must be gone and the other left alone");

    // The manifest still fetches by digest, and the surviving tag still resolves it.
    for reference in [d.to_string(), "v1".to_string()] {
        let r = c
            .get(format!("{pub_base}/v2/acme/nginx/manifests/{reference}"))
            .basic_auth("acme", Some(&token))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK, "reading {reference}");
    }
    // The deleted tag itself no longer resolves.
    let r = c
        .get(format!("{pub_base}/v2/acme/nginx/manifests/latest"))
        .basic_auth("acme", Some(&token))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
}

/// `imagedelete` clears every row of the image's own database and every manifest object under its
/// own prefix — and, critically, a SECOND image belonging to the same owner is completely
/// untouched. An over-broad scan or prefix is exactly the bug this asserts against.
#[tokio::test]
async fn deleting_an_image_leaves_a_sibling_image_completely_intact() {
    let (pub_base, peer_base, e) = common::serve_public_and_peer().await;
    let token = e.store.create_token("acme").await.unwrap();
    let c = reqwest::Client::new();

    let m1 = manifest_bytes();
    let d1 = rustic_git::registry::Digest::of(&m1);
    let r = c
        .put(format!("{pub_base}/v2/acme/nginx/manifests/latest"))
        .basic_auth("acme", Some(&token))
        .header("content-type", MEDIA)
        .body(m1.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);

    // A second image, same owner, different name — the sibling that must survive.
    let m2 = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": MEDIA,
        "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": rustic_git::registry::Digest::of(b"cfg2").to_string(), "size": 4},
        "layers": [{"mediaType": "application/vnd.oci.image.layer.v1.tar+gzip", "digest": rustic_git::registry::Digest::of(b"layer2").to_string(), "size": 6}]
    }).to_string().into_bytes();
    let r = c
        .put(format!("{pub_base}/v2/acme/nginx-alpine/manifests/latest"))
        .basic_auth("acme", Some(&token))
        .header("content-type", MEDIA)
        .body(m2.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);
    let pulls_before = e.store.pulls("acme", "nginx-alpine", "latest").await.unwrap();

    let r = common::peer_post_as(&peer_base, "acme", "/api/acme/nginx/imagedelete", "").await;
    assert_eq!(r.status(), StatusCode::NO_CONTENT);

    // The deleted image: gone by every measure.
    assert!(!e.store.image_exists("acme", "nginx").await.unwrap());
    assert!(e.store.tags("acme", "nginx").await.unwrap().is_empty());
    let r = c
        .get(format!("{pub_base}/v2/acme/nginx/manifests/{d1}"))
        .basic_auth("acme", Some(&token))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);

    // The sibling image: every fact about it is exactly as it was.
    assert!(e.store.image_exists("acme", "nginx-alpine").await.unwrap());
    assert_eq!(e.store.tags("acme", "nginx-alpine").await.unwrap(), vec!["latest".to_string()]);
    assert_eq!(e.store.pulls("acme", "nginx-alpine", "latest").await.unwrap(), pulls_before);
    let r = c
        .get(format!("{pub_base}/v2/acme/nginx-alpine/manifests/latest"))
        .basic_auth("acme", Some(&token))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.bytes().await.unwrap().to_vec(), m2);

    // The catalog/images listing no longer shows the deleted image, and still shows the sibling.
    let r = common::peer_get_as(&peer_base, "acme", "/api/acme/images").await;
    let b: serde_json::Value = r.json().await.unwrap();
    let listed: Vec<&str> = b.as_array().unwrap().iter().map(|i| i["name"].as_str().unwrap()).collect();
    assert!(!listed.contains(&"nginx"), "{listed:?}");
    assert!(listed.contains(&"nginx-alpine"), "{listed:?}");
}

/// `imagedelete` removes the listing-index marker FIRST, unconditionally — before it even lists
/// `manifests/{owner}/{name}` to find objects to delete. Proven by giving the image zero
/// manifests (an empty prefix listing, the case that would otherwise make "marker removal" a
/// no-op nobody could tell apart from "never happened"): the marker is still gone afterward.
#[tokio::test]
async fn imagedelete_removes_the_marker_even_with_zero_manifests() {
    let (pub_base, peer_base, e) = common::serve_public_and_peer().await;
    use rustic_git::index::{self, Kind};
    use slatedb::object_store::{ObjectStore, ObjectStoreExt};

    // `touch_image` is crate-private, so push a manifest through the real API (the only public
    // path to "image exists" — and it also writes the marker via `refresh_image_marker`), then
    // remove the manifest object directly to reach "image_exists but zero objects under its
    // manifest prefix" without a second push endpoint to fake it through.
    let token = e.store.create_token("acme").await.unwrap();
    let m1 = manifest_bytes();
    let r = reqwest::Client::new()
        .put(format!("{pub_base}/v2/acme/nginx/manifests/latest"))
        .basic_auth("acme", Some(&token))
        .header("content-type", MEDIA)
        .body(m1)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);
    let prefix = slatedb::object_store::path::Path::from("manifests/acme/nginx".to_string());
    let mut listing = e.store.os.list(Some(&prefix));
    while let Some(o) = futures::StreamExt::next(&mut listing).await {
        e.store.os.delete(&o.unwrap().location).await.unwrap();
    }

    let pub_path = index::path(true, Kind::Img, "acme", "nginx");
    let priv_path = index::path(false, Kind::Img, "acme", "nginx");
    assert!(
        e.store.os.get(&pub_path).await.is_ok() || e.store.os.get(&priv_path).await.is_ok(),
        "marker not written before delete"
    );

    let r = common::peer_post_as(&peer_base, "acme", "/api/acme/nginx/imagedelete", "").await;
    assert_eq!(r.status(), StatusCode::NO_CONTENT);

    assert!(e.store.os.get(&pub_path).await.is_err(), "public marker survived a zero-manifest delete");
    assert!(e.store.os.get(&priv_path).await.is_err(), "private marker survived a zero-manifest delete");
    assert!(!e.store.image_exists("acme", "nginx").await.unwrap());
}

/// `imagedelete` must never remove blobs: only the sweeper may, per the invariant
/// `blobs::delete_blob` states ("no manifest delete removes a blob... that is the sweeper's job").
#[tokio::test]
async fn deleting_an_image_leaves_its_blobs_on_disk() {
    let (pub_base, peer_base, e) = common::serve_public_and_peer().await;
    let token = e.store.create_token("acme").await.unwrap();
    let c = reqwest::Client::new();

    // A layer, written directly to the object store (mirroring `put_manifest_bytes` above) rather
    // than through the full upload-session dance, which this test has no need to exercise.
    let layer = b"a layer's worth of bytes".to_vec();
    let ld = rustic_git::registry::Digest::of(&layer);
    {
        use slatedb::object_store::ObjectStoreExt;
        e.store
            .os
            .put(
                &rustic_git::registry::store::blob_path("acme", &ld),
                slatedb::object_store::PutPayload::from(layer.clone()),
            )
            .await
            .unwrap();
    }
    let m = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": MEDIA,
        "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": rustic_git::registry::Digest::of(b"cfg").to_string(), "size": 3},
        "layers": [{"mediaType": "application/vnd.oci.image.layer.v1.tar+gzip", "digest": ld.to_string(), "size": layer.len()}]
    }).to_string().into_bytes();
    let r = c
        .put(format!("{pub_base}/v2/acme/nginx/manifests/latest"))
        .basic_auth("acme", Some(&token))
        .header("content-type", MEDIA)
        .body(m)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);

    let r = common::peer_post_as(&peer_base, "acme", "/api/acme/nginx/imagedelete", "").await;
    assert_eq!(r.status(), StatusCode::NO_CONTENT);

    use slatedb::object_store::ObjectStoreExt;
    let still_there = e.store.os.head(&rustic_git::registry::store::blob_path("acme", &ld)).await;
    assert!(still_there.is_ok(), "the layer blob must survive an image delete");
}

/// A stranger's token — a caller who is not the image's own owner — gets 404 from both writes,
/// exactly as `imagetags` already does for reads.
#[tokio::test]
async fn a_stranger_gets_404_from_both_image_write_routes() {
    let (_pub_base, peer_base, e) = common::serve_public_and_peer().await;
    e.store.put_tag("acme", "nginx", "latest", &rustic_git::registry::Digest::of(b"m")).await.unwrap();

    let r = common::peer_post_as(&peer_base, "umbrella", "/api/acme/nginx/imagetagdelete", "latest").await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    let r = common::peer_post_as(&peer_base, "umbrella", "/api/acme/nginx/imagedelete", "").await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);

    // Untouched: the tag is still exactly what it was.
    assert_eq!(e.store.tags("acme", "nginx").await.unwrap(), vec!["latest".to_string()]);
}

const MEDIA: &str = "application/vnd.oci.image.manifest.v1+json";
fn manifest_bytes() -> Vec<u8> {
    serde_json::json!({
        "schemaVersion": 2,
        "mediaType": MEDIA,
        "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": rustic_git::registry::Digest::of(b"cfg").to_string(), "size": 3},
        "layers": [{"mediaType": "application/vnd.oci.image.layer.v1.tar+gzip", "digest": rustic_git::registry::Digest::of(b"layer").to_string(), "size": 5}]
    }).to_string().into_bytes()
}

/// docker sends `scope` TWICE — once for pull, once for pull,push — and a token endpoint that
/// deserializes one `scope: String` answers 400 before the handler runs, which shows up at the
/// client as "failed to fetch oauth token". This is the exact query docker 28 sent.
#[tokio::test]
async fn the_token_endpoint_accepts_a_repeated_scope() {
    let (base, e) = serve().await;
    let token = e.store.create_token("karthik1729").await.unwrap();
    let r = reqwest::Client::new()
        .get(format!(
            "{base}/v2/token?scope=repository%3Akarthik1729%2Fnginx%3Apull\
&scope=repository%3Akarthik1729%2Fnginx%3Apull%2Cpush&service=dev.kloudlite.io"
        ))
        .basic_auth("karthik1729", Some(&token))
        .send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let body: serde_json::Value = r.json().await.unwrap();
    // Both scopes are recorded, space separated, as the token response carries them.
    let scope = body["scope"].as_str().unwrap_or_default().to_string();
    assert!(scope.contains("pull,push"), "got {scope}");
    assert!(body["token"].as_str().is_some_and(|t| !t.is_empty()));
}

/// The 500 path used to be `internal_pub`'s plain-text "internal error", which broke CLAUDE.md's
/// rule that every `/v2` error is the OCI JSON envelope. Exercise `oci_internal` directly since
/// forcing a real internal failure through a handler needs faking store I/O.
#[tokio::test]
async fn oci_internal_returns_the_oci_envelope() {
    use axum::response::IntoResponse;
    let r = rustic_git::registry::oci_internal(rustic_git::err("boom")).into_response();
    assert_eq!(r.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body = axum::body::to_bytes(r.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let code = json["errors"][0]["code"].as_str().unwrap_or_default();
    assert_eq!(code, "UNKNOWN");
    assert!(json["errors"][0]["message"].as_str().is_some_and(|m| !m.is_empty()));
}
