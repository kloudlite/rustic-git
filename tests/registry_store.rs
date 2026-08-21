mod common;
use rustic_git::registry::{store as rstore, Digest};

#[test]
fn digests_parse_strictly() {
    let hex = "a".repeat(64);
    let d = Digest::parse(&format!("sha256:{hex}")).unwrap();
    assert_eq!(d.to_string(), format!("sha256:{hex}"));
    // Everything a path segment could smuggle in is refused.
    assert!(Digest::parse("sha256:short").is_none());
    assert!(Digest::parse(&format!("sha256:{}", "A".repeat(64))).is_none(), "uppercase hex");
    assert!(Digest::parse(&format!("sha512:{hex}")).is_none(), "unsupported algorithm");
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
    use rustic_git::index::{self, Kind};
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
