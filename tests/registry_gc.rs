mod common;
use rustic_git::registry::{gc, store::blob_path, Digest};
use slatedb::object_store::{ObjectStoreExt, PutPayload};
use std::time::Duration;

#[tokio::test]
async fn an_unreferenced_blob_is_swept_and_a_referenced_one_is_not() {
    let e = common::env().await;
    let layer = b"referenced layer".to_vec();
    let ld = Digest::of(&layer);
    let orphan = b"nothing points at me".to_vec();
    let od = Digest::of(&orphan);
    e.store.os.put(&blob_path("acme", &ld), PutPayload::from(layer)).await.unwrap();
    e.store.os.put(&blob_path("acme", &od), PutPayload::from(orphan)).await.unwrap();

    let manifest = serde_json::json!({
        "schemaVersion": 2,
        "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": ld.to_string(), "size": 1},
        "layers": [{"mediaType": "application/vnd.oci.image.layer.v1.tar+gzip", "digest": ld.to_string(), "size": 1}]
    }).to_string().into_bytes();
    let md = Digest::of(&manifest);
    e.store.os
        .put(&rustic_git::registry::store::manifest_path("acme", "nginx", &md), PutPayload::from(manifest))
        .await.unwrap();
    e.store.put_tag("acme", "nginx", "latest", &md).await.unwrap();

    // Grace zero: everything is old enough to consider.
    let n = gc::sweep_owner(&e.store, "acme", Duration::ZERO).await.unwrap();
    assert_eq!(n, 1, "exactly the orphan");
    assert!(e.store.os.head(&blob_path("acme", &ld)).await.is_ok(), "the referenced layer survives");
    assert!(e.store.os.head(&blob_path("acme", &od)).await.is_err(), "the orphan is gone");
}

#[tokio::test]
async fn a_blob_inside_the_grace_window_survives() {
    let e = common::env().await;
    let fresh = b"just uploaded, manifest still coming".to_vec();
    let d = Digest::of(&fresh);
    e.store.os.put(&blob_path("acme", &d), PutPayload::from(fresh)).await.unwrap();
    let n = gc::sweep_owner(&e.store, "acme", Duration::from_secs(3600)).await.unwrap();
    assert_eq!(n, 0, "an in-flight push must not be swept out from under itself");
    assert!(e.store.os.head(&blob_path("acme", &d)).await.is_ok());
}

#[tokio::test]
async fn a_layer_two_images_share_survives_one_of_them_being_emptied() {
    let e = common::env().await;
    let shared = b"base layer".to_vec();
    let sd = Digest::of(&shared);
    e.store.os.put(&blob_path("acme", &sd), PutPayload::from(shared)).await.unwrap();
    for image in ["nginx", "api"] {
        let m = serde_json::json!({
            "schemaVersion": 2,
            "config": {"digest": sd.to_string(), "size": 1},
            "layers": [{"digest": sd.to_string(), "size": 1}]
        }).to_string().into_bytes();
        let md = Digest::of(&m);
        e.store.os
            .put(&rustic_git::registry::store::manifest_path("acme", image, &md), PutPayload::from(m))
            .await.unwrap();
        e.store.put_tag("acme", image, "latest", &md).await.unwrap();
    }
    // Empty one image entirely.
    e.store.delete_tag("acme", "nginx", "latest").await.unwrap();
    let n = gc::sweep_owner(&e.store, "acme", Duration::ZERO).await.unwrap();
    assert_eq!(n, 0);
    assert!(e.store.os.head(&blob_path("acme", &sd)).await.is_ok(), "the other image still needs it");
}

/// The digest walk is schema-agnostic on purpose — it does not special-case `config`/`layers`
/// like the tests above happen to use. This proves that: a blob named only through an index's
/// `manifests` entry, and one named only through `subject`, must both survive the sweep, or the
/// walk-every-"digest"-string approach is not actually buying what it claims to.
#[tokio::test]
async fn a_blob_referenced_only_via_an_index_entry_or_subject_survives() {
    let e = common::env().await;
    let via_index = b"platform-specific layer".to_vec();
    let iid = Digest::of(&via_index);
    let via_subject = b"referenced by a subject field".to_vec();
    let sud = Digest::of(&via_subject);
    e.store.os.put(&blob_path("acme", &iid), PutPayload::from(via_index)).await.unwrap();
    e.store.os.put(&blob_path("acme", &sud), PutPayload::from(via_subject)).await.unwrap();

    // An index (manifest list) naming a per-platform manifest only through "manifests[].digest".
    let index = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": [
            {"mediaType": "application/vnd.oci.image.manifest.v1+json", "digest": iid.to_string(), "size": 1, "platform": {"architecture": "amd64", "os": "linux"}}
        ]
    }).to_string().into_bytes();
    let ixd = Digest::of(&index);
    e.store.os
        .put(&rustic_git::registry::store::manifest_path("acme", "multi", &ixd), PutPayload::from(index))
        .await.unwrap();
    e.store.put_tag("acme", "multi", "latest", &ixd).await.unwrap();

    // A manifest naming another blob only through "subject.digest" (e.g. an attached artifact).
    let attached = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {"digest": sud.to_string(), "size": 1},
        "layers": [],
        "subject": {"mediaType": "application/vnd.oci.image.manifest.v1+json", "digest": sud.to_string(), "size": 1}
    }).to_string().into_bytes();
    let atd = Digest::of(&attached);
    e.store.os
        .put(&rustic_git::registry::store::manifest_path("acme", "artifact", &atd), PutPayload::from(attached))
        .await.unwrap();
    e.store.put_tag("acme", "artifact", "latest", &atd).await.unwrap();

    let n = gc::sweep_owner(&e.store, "acme", Duration::ZERO).await.unwrap();
    assert_eq!(n, 0, "both blobs are referenced, only through an index entry / subject field");
    assert!(e.store.os.head(&blob_path("acme", &iid)).await.is_ok());
    assert!(e.store.os.head(&blob_path("acme", &sud)).await.is_ok());
}
