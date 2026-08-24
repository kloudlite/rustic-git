mod common;
use rustic_git_registry::{gc, store::blob_path, store::ImageExt, uploads::UploadsExt, Digest};
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
        .put(&rustic_git_registry::store::manifest_path("acme", "nginx", &md), PutPayload::from(manifest))
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
            .put(&rustic_git_registry::store::manifest_path("acme", image, &md), PutPayload::from(m))
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
        .put(&rustic_git_registry::store::manifest_path("acme", "multi", &ixd), PutPayload::from(index))
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
        .put(&rustic_git_registry::store::manifest_path("acme", "artifact", &atd), PutPayload::from(attached))
        .await.unwrap();
    e.store.put_tag("acme", "artifact", "latest", &atd).await.unwrap();

    let n = gc::sweep_owner(&e.store, "acme", Duration::ZERO).await.unwrap();
    assert_eq!(n, 0, "both blobs are referenced, only through an index entry / subject field");
    assert!(e.store.os.head(&blob_path("acme", &iid)).await.is_ok());
    assert!(e.store.os.head(&blob_path("acme", &sud)).await.is_ok());
}

/// `put_manifest` writes whatever bytes a client PUTs without validating them as JSON, and a
/// truncated/corrupt object-store read can land here too. The rule is: any uncertainty about
/// what is referenced means delete nothing — so an unparseable manifest must abort the whole
/// sweep, not be silently skipped (which would judge every blob it names an orphan).
#[tokio::test]
async fn a_manifest_that_is_not_valid_json_aborts_the_sweep_and_deletes_nothing() {
    let e = common::env().await;
    let blob = b"a blob that would otherwise look orphaned".to_vec();
    let bd = Digest::of(&blob);
    e.store.os.put(&blob_path("acme", &bd), PutPayload::from(blob)).await.unwrap();

    let garbage = b"not json at all".to_vec();
    let gd = Digest::of(&garbage);
    e.store.os
        .put(&rustic_git_registry::store::manifest_path("acme", "broken", &gd), PutPayload::from(garbage))
        .await.unwrap();

    let result = gc::sweep_owner(&e.store, "acme", Duration::ZERO).await;
    assert!(result.is_err(), "an unparseable manifest must abort the sweep, not be skipped");
    assert!(e.store.os.head(&blob_path("acme", &bd)).await.is_ok(), "nothing was deleted");
}

/// `referenced()` and `sweep_owner` used to reassemble every digest as `format!("sha256:{hex}")`
/// from the path's last segment, ignoring the algo segment actually stored in the path. A
/// sha512-referenced blob would then never match the (wrongly sha256-prefixed) referenced set and
/// would be swept as an orphan — a data-loss bug for any sha512 layer. This proves the fix: a
/// manifest referencing a sha512 blob must protect it.
#[tokio::test]
async fn a_manifest_referencing_a_sha512_blob_protects_it_from_the_sweep() {
    let e = common::env().await;
    let layer = b"a layer hashed with sha512".to_vec();
    let ld = Digest::of_algo("sha512", &layer).unwrap();
    e.store.os.put(&blob_path("acme", &ld), PutPayload::from(layer)).await.unwrap();

    let manifest = serde_json::json!({
        "schemaVersion": 2,
        "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": ld.to_string(), "size": 1},
        "layers": [{"mediaType": "application/vnd.oci.image.layer.v1.tar+gzip", "digest": ld.to_string(), "size": 1}]
    }).to_string().into_bytes();
    let md = Digest::of(&manifest);
    e.store.os
        .put(&rustic_git_registry::store::manifest_path("acme", "nginx", &md), PutPayload::from(manifest))
        .await.unwrap();
    e.store.put_tag("acme", "nginx", "latest", &md).await.unwrap();

    let n = gc::sweep_owner(&e.store, "acme", Duration::ZERO).await.unwrap();
    assert_eq!(n, 0, "the sha512 layer is referenced and must survive");
    assert!(e.store.os.head(&blob_path("acme", &ld)).await.is_ok());
}

/// The mount race: grace protects a freshly uploaded blob, but not an OLD blob a client skips
/// re-uploading (a HEAD hit, or a cross-repo mount) and then references from a manifest written
/// after the manifest scan — the blob's own timestamp never moves. `sweep_owner` closes this by
/// re-reading `referenced()` after listing blobs and deleting only the intersection.
///
/// There is no clean seam inside `sweep_owner` to inject the manifest write between its own two
/// internal reads without contorting production code for a test hook, so this drives the two
/// `referenced()` phases directly (it is `pub` for exactly this) the same way `sweep_owner` does,
/// then confirms the end-to-end result through `sweep_owner` itself.
#[tokio::test]
async fn a_blob_referenced_between_the_two_manifest_reads_survives_the_mount_race() {
    let e = common::env().await;
    let aged = b"old base layer nobody re-uploads".to_vec();
    let ad = Digest::of(&aged);
    e.store.os.put(&blob_path("acme", &ad), PutPayload::from(aged)).await.unwrap();

    // Phase 1 (what sweep_owner's first read sees): nothing references the blob yet.
    let keep_before = gc::referenced(&e.store, "acme").await.unwrap();
    assert!(!keep_before.contains(&ad.to_string()), "not yet referenced by anything");

    // A mount lands between the two reads: a new image reuses the old blob by digest without
    // re-uploading it, so grace (which only looks at the blob's own timestamp) cannot see this.
    let m = serde_json::json!({
        "schemaVersion": 2,
        "config": {"digest": ad.to_string(), "size": 1},
        "layers": [{"digest": ad.to_string(), "size": 1}]
    }).to_string().into_bytes();
    let md = Digest::of(&m);
    e.store.os
        .put(&rustic_git_registry::store::manifest_path("acme", "mounted", &md), PutPayload::from(m))
        .await.unwrap();
    e.store.put_tag("acme", "mounted", "latest", &md).await.unwrap();

    // Phase 2 (what sweep_owner's second read sees): now referenced.
    let keep_after = gc::referenced(&e.store, "acme").await.unwrap();
    assert!(keep_after.contains(&ad.to_string()), "referenced by the mount's manifest");

    // Only the intersection of the two reads may be deleted, so this blob — orphaned in read 1,
    // referenced in read 2 — must survive a real sweep_owner call.
    let n = gc::sweep_owner(&e.store, "acme", Duration::ZERO).await.unwrap();
    assert_eq!(n, 0, "the mount's manifest protects the blob");
    assert!(e.store.os.head(&blob_path("acme", &ad)).await.is_ok());
}

/// CLAUDE.md calls the Redis-down fallback load-bearing: with Redis unreachable, every stream
/// call a lane makes must be inert (empty, no panic, no hang), and the GC lane — which
/// touches only the object store — must keep sweeping. `redis://127.0.0.1:1` is a port nothing
/// listens on; `Cache::connect` gives up on it in 250ms.
#[tokio::test]
async fn worker_lanes_are_inert_and_gc_still_sweeps_with_redis_down() {
    let cache = rustic_git_storage::cache::Cache::connect(Some("redis://127.0.0.1:1")).await;
    assert!(!cache.connected());
    cache.xgroup_create_mkstream("events", "merge-worker").await;
    assert!(cache.xreadgroup("events", "merge-worker", "t/0", 16).await.is_empty());
    assert!(cache.xautoclaim("events", "merge-worker", "t/0", 30_000, 16).await.is_empty());
    cache.xack("events", "merge-worker", "0-0").await;

    let e = common::env().await;
    assert!(!e.store.cache.connected());
    let orphan = b"nothing points at me".to_vec();
    let od = Digest::of(&orphan);
    e.store.os.put(&blob_path("acme", &od), PutPayload::from(orphan)).await.unwrap();
    assert_eq!(gc::sweep_owner(&e.store, "acme", Duration::ZERO).await.unwrap(), 1);
    assert_eq!(e.store.sweep_stale_uploads("acme", Duration::ZERO).await.unwrap(), 0);
}

/// When no blob is older than `grace` nothing can be deleted, so the manifests are not read at
/// all. Observable through the keep-biased rule: an unparseable manifest aborts a sweep that
/// reads it — and must NOT abort one that had no reason to.
#[tokio::test]
async fn a_sweep_with_nothing_old_enough_reads_no_manifests() {
    let e = common::env().await;
    let fresh = b"just uploaded".to_vec();
    e.store.os.put(&blob_path("acme", &Digest::of(&fresh)), PutPayload::from(fresh)).await.unwrap();
    let garbage = b"not json at all".to_vec();
    e.store.os
        .put(&rustic_git_registry::store::manifest_path("acme", "broken", &Digest::of(&garbage)), PutPayload::from(garbage))
        .await.unwrap();

    let n = gc::sweep_owner(&e.store, "acme", Duration::from_secs(3600)).await.unwrap();
    assert_eq!(n, 0);
    // And with everything old enough, the same manifest aborts the sweep as before.
    assert!(gc::sweep_owner(&e.store, "acme", Duration::ZERO).await.is_err());
}
