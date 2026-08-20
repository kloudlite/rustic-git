//! Referrers index — stubbed here so `manifests.rs` has something to call. Task 9 builds the real
//! index (subject digest -> referrers list) and fills these in.
use super::store::Digest;
use crate::App;

/// Task 9 fills this in: record `d` as a referrer of the manifest's `subject`, if any.
pub async fn index(_app: &App, _owner: &str, _name: &str, _d: &Digest, _bytes: &[u8]) -> crate::Result<()> {
    Ok(())
}

/// Task 9 fills this in: remove `d` from whatever referrers index it was recorded under.
pub async fn unindex(_app: &App, _owner: &str, _name: &str, _d: &Digest) -> crate::Result<()> {
    Ok(())
}
