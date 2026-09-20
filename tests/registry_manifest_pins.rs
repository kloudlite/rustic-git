//! R-2 T4: a failed manifest push must leave no pin behind. `manifests::put_manifest` is an axum
//! handler with no seam in `tests/common`'s HTTP harness to inject a store failure mid-request
//! (every wrapped-store test elsewhere in this suite wraps LIST, not a single conditional PUT), so
//! this drives `manifests::pin_probe_write` directly — the function `put_manifest` itself calls for
//! everything between the first pin and the final release (see its own doc in manifests.rs).
mod common;

use kloudlite_registry::{blob_state, manifests::{pin_probe_write, PinFailure}, store::{blob_path, manifest_path}, Digest};
use slatedb::object_store::{
    memory::InMemory, path::Path, ObjectStore, ObjectStoreExt, PutOptions, PutPayload,
};
use std::sync::Arc;

/// Wraps `InMemory` and fails exactly one path's `put_opts` (any mode) with a plain error —
/// everything else, including every `blob_state` CAS write, passes straight through. This is the
/// seam `tests/common`'s harness has no equivalent of: a failure exactly where the manifest bytes
/// land, nowhere else.
#[derive(Debug)]
struct FailPut {
    inner: InMemory,
    fail: Path,
}

impl std::fmt::Display for FailPut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "FailPut({})", self.fail)
    }
}

#[async_trait::async_trait]
impl ObjectStore for FailPut {
    async fn put_opts(
        &self,
        location: &Path,
        payload: PutPayload,
        opts: PutOptions,
    ) -> slatedb::object_store::Result<slatedb::object_store::PutResult> {
        if location == &self.fail {
            return Err(slatedb::object_store::Error::Generic {
                store: "FailPut",
                source: "simulated manifest write failure".into(),
            });
        }
        self.inner.put_opts(location, payload, opts).await
    }
    async fn put_multipart_opts(
        &self,
        location: &Path,
        opts: slatedb::object_store::PutMultipartOptions,
    ) -> slatedb::object_store::Result<Box<dyn slatedb::object_store::MultipartUpload>> {
        self.inner.put_multipart_opts(location, opts).await
    }
    async fn get_opts(
        &self,
        location: &Path,
        opts: slatedb::object_store::GetOptions,
    ) -> slatedb::object_store::Result<slatedb::object_store::GetResult> {
        self.inner.get_opts(location, opts).await
    }
    fn delete_stream(
        &self,
        locations: futures::stream::BoxStream<'static, slatedb::object_store::Result<Path>>,
    ) -> futures::stream::BoxStream<'static, slatedb::object_store::Result<Path>> {
        self.inner.delete_stream(locations)
    }
    fn list(
        &self,
        prefix: Option<&Path>,
    ) -> futures::stream::BoxStream<'static, slatedb::object_store::Result<slatedb::object_store::ObjectMeta>> {
        self.inner.list(prefix)
    }
    async fn list_with_delimiter(
        &self,
        prefix: Option<&Path>,
    ) -> slatedb::object_store::Result<slatedb::object_store::ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }
    async fn copy_opts(
        &self,
        from: &Path,
        to: &Path,
        opts: slatedb::object_store::CopyOptions,
    ) -> slatedb::object_store::Result<()> {
        self.inner.copy_opts(from, to, opts).await
    }
}

fn three_layer_manifest(owner: &str) -> (Vec<Digest>, Digest, Vec<u8>) {
    let layers: Vec<_> = [b"layer-a".to_vec(), b"layer-b".to_vec(), b"layer-c".to_vec()]
        .into_iter()
        .map(|b| Digest::of(&b))
        .collect();
    let body = serde_json::json!({
        "schemaVersion": 2,
        "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": layers[0].to_string(), "size": 1},
        "layers": layers.iter().map(|d| serde_json::json!({"mediaType": "application/vnd.oci.image.layer.v1.tar+gzip", "digest": d.to_string(), "size": 1})).collect::<Vec<_>>(),
    })
    .to_string()
    .into_bytes();
    let d = Digest::of(&body);
    let _ = owner;
    (layers, d, body)
}

async fn seed_blobs(os: &dyn ObjectStore, owner: &str, digests: &[Digest]) {
    for d in digests {
        os.put(&blob_path(owner, d), PutPayload::from(b"x".to_vec())).await.unwrap();
    }
}

/// A manifest push that fails writing the manifest BYTES (every layer pinned successfully first)
/// leaves zero pins on every layer it touched.
#[tokio::test]
async fn a_manifest_push_that_fails_after_pinning_leaves_no_pin() {
    let owner = "acme";
    let (layers, d, body) = three_layer_manifest(owner);
    let inner = InMemory::new();
    let os = Arc::new(FailPut { inner, fail: manifest_path(owner, "nginx", &d) });
    seed_blobs(os.as_ref(), owner, &layers).await;

    let result = pin_probe_write(os.as_ref(), owner, "nginx", &d, &layers, &axum::body::Bytes::from(body)).await;
    assert!(matches!(result, Err(PinFailure::Write(_))), "expected the manifest write to fail");

    for l in &layers {
        let record = blob_state::candidates(os.as_ref(), owner).await.unwrap().into_iter().find(|(dig, _, _)| dig == l).unwrap().1;
        assert!(record.active.unwrap().pins.is_empty(), "layer {l} still pinned after a failed manifest write");
    }
}

/// The pin-error variant: pinning the THIRD layer fails outright (its state record is CAS'd out
/// from under the push by making the record path itself refuse writes) — layers one and two, whose
/// pins DID land, must end with zero pins too.
#[tokio::test]
async fn a_manifest_push_whose_third_pin_fails_leaves_the_first_two_unpinned() {
    let owner = "acme";
    let (layers, d, body) = three_layer_manifest(owner);
    let inner = InMemory::new();
    // The third layer's OWN state record path — pin/unpin both write here, so every attempt for
    // this one digest fails, exhausting its 8 CAS retries the same way a real contended pin would.
    let fail = blob_state::state_path(owner, &layers[2]);
    let os = Arc::new(FailPut { inner, fail });
    seed_blobs(os.as_ref(), owner, &layers).await;

    let result = pin_probe_write(os.as_ref(), owner, "nginx", &d, &layers, &axum::body::Bytes::from(body)).await;
    assert!(matches!(result, Err(PinFailure::Pin(_))), "expected the third pin to fail");

    for l in &layers[..2] {
        let record = blob_state::candidates(os.as_ref(), owner).await.unwrap().into_iter().find(|(dig, _, _)| dig == l).unwrap().1;
        assert!(record.active.unwrap().pins.is_empty(), "layer {l} still pinned after a sibling pin failed");
    }
}
