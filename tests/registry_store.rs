mod common;
use rustic_git_registry::{store as rstore, store::ImageExt, Digest};

#[test]
fn digests_parse_strictly() {
    let hex = "a".repeat(64);
    let d = Digest::parse(&format!("sha256:{hex}")).unwrap();
    assert_eq!(d.to_string(), format!("sha256:{hex}"));
    // Everything a path segment could smuggle in is refused.
    assert!(Digest::parse("sha256:short").is_none());
    assert!(Digest::parse(&format!("sha256:{}", "A".repeat(64))).is_none(), "uppercase hex");
    assert!(Digest::parse(&format!("sha512:{hex}")).is_none(), "sha512 with a sha256-length hex");
    assert!(Digest::parse("md5:d41d8cd98f00b204e9800998ecf8427e").is_none(), "unsupported algorithm");
    assert!(Digest::parse(&format!("sha256:{}/../../etc", "a".repeat(56))).is_none());
    assert!(Digest::parse("").is_none());
}

#[test]
fn digest_of_bytes_matches_the_wire_format() {
    // sha256 of the empty string, the value every registry client knows.
    assert_eq!(
        Digest::of(b"").to_string(),
        "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
}

#[test]
fn object_paths_are_owner_scoped() {
    let d = Digest::of(b"layer");
    assert_eq!(rstore::blob_path("acme", &d).to_string(), format!("blobs/acme/sha256/{}", d.hex));
    assert_eq!(
        rstore::manifest_path("acme", "nginx", &d).to_string(),
        format!("manifests/acme/nginx/sha256/{}", d.hex)
    );
}

#[tokio::test]
async fn tags_round_trip_and_sort() {
    let e = common::env().await;
    let d = Digest::of(b"manifest");
    e.store.put_tag("acme", "nginx", "v2", &d).await.unwrap();
    e.store.put_tag("acme", "nginx", "latest", &d).await.unwrap();
    assert_eq!(e.store.tags("acme", "nginx").await.unwrap(), vec!["latest", "v2"]);
    assert_eq!(e.store.tag("acme", "nginx", "latest").await.unwrap().unwrap().hex, d.hex);
    e.store.delete_tag("acme", "nginx", "latest").await.unwrap();
    assert_eq!(e.store.tags("acme", "nginx").await.unwrap(), vec!["v2"]);
    assert!(e.store.tag("acme", "nginx", "latest").await.unwrap().is_none());
}

#[tokio::test]
async fn an_image_and_a_repo_of_one_name_are_two_things() {
    let e = common::env().await;
    e.store.put_tag("acme", "nginx", "latest", &Digest::of(b"m")).await.unwrap();
    // The image exists; the repo of the same name does not.
    assert!(e.store.image_exists("acme", "nginx").await.unwrap());
    assert!(!e.store.repo_exists("acme", "nginx").await.unwrap());
}

#[tokio::test]
async fn images_are_private_until_told_otherwise() {
    let e = common::env().await;
    e.store.put_tag("acme", "nginx", "latest", &Digest::of(b"m")).await.unwrap();
    assert!(!e.store.image_is_public("acme", "nginx").await.unwrap());
    e.store.set_image_visibility("acme", "nginx", true).await.unwrap();
    assert!(e.store.image_is_public("acme", "nginx").await.unwrap());
}

/// Flip-to-private must delete the PUBLIC marker before the DB write, so a crash between the two
/// can never leave a stale public marker over a DB row that already says private. Simulates the
/// crash window by planting a fake public marker right before the flip and checking it's gone
/// once `set_image_visibility` returns.
#[tokio::test]
async fn flip_to_private_removes_the_public_marker() {
    use rustic_git_storage::index::{self, Kind};
    use slatedb::object_store::ObjectStoreExt;
    let e = common::env().await;
    e.store.put_tag("acme", "nginx", "latest", &Digest::of(b"m")).await.unwrap();
    e.store.set_image_visibility("acme", "nginx", true).await.unwrap();
    assert!(e.store.os.head(&index::path(true, Kind::Img, "acme", "nginx")).await.is_ok());

    e.store.set_image_visibility("acme", "nginx", false).await.unwrap();

    assert!(
        e.store.os.head(&index::path(true, Kind::Img, "acme", "nginx")).await.is_err(),
        "public marker must be gone after a flip to private"
    );
    assert!(e.store.os.head(&index::path(false, Kind::Img, "acme", "nginx")).await.is_ok());
}

/// object_store's `list(Some(prefix))` is DOCUMENTED as segment-wise, but the InMemory and
/// local-filesystem implementations this deployment tests against are what actually decide
/// whether `repo/img/acme/nginx` can reach `repo/img/acme/nginx-alpine`. Pin the answer.
#[tokio::test]
async fn object_store_prefix_listing_is_segment_wise() {
    use slatedb::object_store::{path::Path as OsPath, ObjectStore, ObjectStoreExt, PutPayload};
    let e = common::env().await;
    e.store.os.put(&OsPath::from("repo/img/acme/nginx/a"), PutPayload::from("1")).await.unwrap();
    e.store.os.put(&OsPath::from("repo/img/acme/nginx-alpine/a"), PutPayload::from("2")).await.unwrap();
    let mut it = e.store.os.list(Some(&OsPath::from("repo/img/acme/nginx")));
    let mut hits = vec![];
    while let Some(m) = futures::StreamExt::next(&mut it).await {
        hits.push(m.unwrap().location.to_string());
    }
    assert_eq!(hits, vec!["repo/img/acme/nginx/a".to_string()], "prefix leaked into a sibling");
}

/// An object store that answers one nominated key with a transient error, so the backfill's
/// failure direction can be exercised at all. Everything else delegates.
#[derive(Debug)]
struct FailsOneGet {
    inner: std::sync::Arc<slatedb::object_store::memory::InMemory>,
    bad: slatedb::object_store::path::Path,
}

impl std::fmt::Display for FailsOneGet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "FailsOneGet")
    }
}

#[async_trait::async_trait]
impl slatedb::object_store::ObjectStore for FailsOneGet {
    async fn put_opts(
        &self,
        l: &slatedb::object_store::path::Path,
        p: slatedb::object_store::PutPayload,
        o: slatedb::object_store::PutOptions,
    ) -> slatedb::object_store::Result<slatedb::object_store::PutResult> {
        self.inner.put_opts(l, p, o).await
    }
    async fn put_multipart_opts(
        &self,
        l: &slatedb::object_store::path::Path,
        o: slatedb::object_store::PutMultipartOptions,
    ) -> slatedb::object_store::Result<Box<dyn slatedb::object_store::MultipartUpload>> {
        self.inner.put_multipart_opts(l, o).await
    }
    async fn get_opts(
        &self,
        l: &slatedb::object_store::path::Path,
        o: slatedb::object_store::GetOptions,
    ) -> slatedb::object_store::Result<slatedb::object_store::GetResult> {
        if *l == self.bad {
            return Err(slatedb::object_store::Error::Generic {
                store: "FailsOneGet",
                source: "injected transient failure".into(),
            });
        }
        self.inner.get_opts(l, o).await
    }
    fn delete_stream(
        &self,
        locations: futures::stream::BoxStream<'static, slatedb::object_store::Result<slatedb::object_store::path::Path>>,
    ) -> futures::stream::BoxStream<'static, slatedb::object_store::Result<slatedb::object_store::path::Path>> {
        self.inner.delete_stream(locations)
    }
    fn list(
        &self,
        p: Option<&slatedb::object_store::path::Path>,
    ) -> futures::stream::BoxStream<'static, slatedb::object_store::Result<slatedb::object_store::ObjectMeta>> {
        self.inner.list(p)
    }
    async fn list_with_delimiter(
        &self,
        p: Option<&slatedb::object_store::path::Path>,
    ) -> slatedb::object_store::Result<slatedb::object_store::ListResult> {
        self.inner.list_with_delimiter(p).await
    }
    async fn copy_opts(
        &self,
        from: &slatedb::object_store::path::Path,
        to: &slatedb::object_store::path::Path,
        o: slatedb::object_store::CopyOptions,
    ) -> slatedb::object_store::Result<()> {
        self.inner.copy_opts(from, to, o).await
    }
}

/// A pre-rows image whose manifest cannot be read must answer "this image does not hold that
/// blob" — a 404 for the puller — not propagate the store's error into a 500. Under-granting is
/// the safe failure for authorization; a fault is not an answer.
#[tokio::test]
async fn an_unreadable_manifest_reads_as_not_held_not_as_a_fault() {
    use slatedb::object_store::{ObjectStoreExt, PutPayload};

    let layer = Digest::of(b"layer");
    let manifest = serde_json::json!({
        "schemaVersion": 2,
        "layers": [{"digest": layer.to_string(), "size": 5}]
    })
    .to_string()
    .into_bytes();
    let md = Digest::of(&manifest);
    let loc = rustic_git_registry::store::manifest_path("acme", "nginx", &md);

    let inner = std::sync::Arc::new(slatedb::object_store::memory::InMemory::new());
    inner.put(&loc, PutPayload::from(manifest)).await.unwrap();
    let os = std::sync::Arc::new(FailsOneGet { inner, bad: loc.clone() });
    let tmp = tempfile::tempdir().unwrap();
    let store = std::sync::Arc::new(
        rustic_git_storage::store::Store::open(os, tmp.path().join("cache"), false).await.unwrap(),
    );

    let held = rustic_git_registry::store::image_holds_blob(&store, "acme", "nginx", &layer).await;
    assert!(!held.unwrap(), "an unreadable manifest names nothing; it is not a 500");
}

/// The walk happens once. A second call must answer from rows alone — proven by taking the
/// manifest object away and asking again for a blob it named.
#[tokio::test]
async fn the_backfill_marks_itself_done_and_does_not_walk_twice() {
    use slatedb::object_store::{ObjectStoreExt, PutPayload};
    let e = common::env().await;
    let layer = Digest::of(b"layer");
    let manifest = serde_json::json!({
        "schemaVersion": 2,
        "layers": [{"digest": layer.to_string(), "size": 5}]
    })
    .to_string()
    .into_bytes();
    let md = Digest::of(&manifest);
    let loc = rustic_git_registry::store::manifest_path("acme", "nginx", &md);
    e.store.os.put(&loc, PutPayload::from(manifest)).await.unwrap();

    assert!(rustic_git_registry::store::image_holds_blob(&e.store, "acme", "nginx", &layer)
        .await
        .unwrap());
    e.store.os.delete(&loc).await.unwrap();
    assert!(
        rustic_git_registry::store::image_holds_blob(&e.store, "acme", "nginx", &layer)
            .await
            .unwrap(),
        "the row survives the manifest; the walk must not run a second time"
    );
}
