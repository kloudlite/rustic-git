//! R-2 T3: the sweep judges a pin by its own time, not merely "is the list non-empty" — a pin left
//! behind by a push that died mid-flight (leaked, pre-R-2) must age out, never protect its blob
//! forever. Kept separate from `tests/registry_gc.rs` per the brief (that file's own assertions
//! are R-3's territory, untouched here).
mod common;

use kloudlite_registry::{blob_state, gc, store::blob_path, Digest};
use slatedb::object_store::{ObjectStoreExt, PutPayload};
use std::time::Duration;

fn now_millis() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// A blob with a pin whose `@time` is well before the sweep's cutoff (grace ZERO, so cutoff is
/// "now") is collected — the leaked-pin case R-2 exists to close.
#[tokio::test]
async fn the_sweep_collects_a_blob_held_only_by_an_expired_pin() {
    let e = common::env().await;
    let d = Digest::of(b"held by an expired pin");
    e.store.os.put(&blob_path("acme", &d), PutPayload::from(b"held by an expired pin".to_vec())).await.unwrap();
    blob_state::resolve(&e.store.os, "acme", &d).await.unwrap(); // adopts the legacy object, installed_at = now
    let expired_pin = format!("manifest/acme/nginx/{d}#deadbeef@{}", now_millis() - 3_600_000);
    blob_state::pin(&e.store.os, "acme", &d, &expired_pin).await.unwrap();

    let n = gc::sweep_owner(&e.store, "acme", Duration::ZERO).await.unwrap();
    assert_eq!(n, 1, "the blob held only by an expired pin should have been swept");
    assert!(e.store.os.head(&blob_path("acme", &d)).await.is_err());
}

/// The same setup, but the pin's `@time` is fresh (in the future relative to the sweep's cutoff) —
/// the blob survives.
#[tokio::test]
async fn the_sweep_keeps_a_blob_held_by_a_fresh_pin() {
    let e = common::env().await;
    let d = Digest::of(b"held by a fresh pin");
    e.store.os.put(&blob_path("acme", &d), PutPayload::from(b"held by a fresh pin".to_vec())).await.unwrap();
    blob_state::resolve(&e.store.os, "acme", &d).await.unwrap();
    let fresh_pin = format!("manifest/acme/nginx/{d}#deadbeef@{}", now_millis() + 3_600_000);
    blob_state::pin(&e.store.os, "acme", &d, &fresh_pin).await.unwrap();

    let n = gc::sweep_owner(&e.store, "acme", Duration::ZERO).await.unwrap();
    assert_eq!(n, 0, "a blob held by a fresh pin must survive");
    assert!(e.store.os.head(&blob_path("acme", &d)).await.is_ok());
}

/// A pin with no parsable `@time` at all — the shape an older build wrote, before this field
/// existed — is always live (keep-bias: uncertainty keeps). The blob survives even under a
/// grace-ZERO sweep.
#[tokio::test]
async fn the_sweep_keeps_a_blob_held_by_a_pin_with_no_time() {
    let e = common::env().await;
    let d = Digest::of(b"held by an old-format pin");
    e.store.os.put(&blob_path("acme", &d), PutPayload::from(b"held by an old-format pin".to_vec())).await.unwrap();
    blob_state::resolve(&e.store.os, "acme", &d).await.unwrap();
    let old_format_pin = format!("manifest/acme/nginx/{d}#deadbeef");
    blob_state::pin(&e.store.os, "acme", &d, &old_format_pin).await.unwrap();

    let n = gc::sweep_owner(&e.store, "acme", Duration::ZERO).await.unwrap();
    assert_eq!(n, 0, "a pin with no parsable time must be treated as live");
    assert!(e.store.os.head(&blob_path("acme", &d)).await.is_ok());
}
