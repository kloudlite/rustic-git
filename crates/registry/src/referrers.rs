//! The referrers index: which manifests declare another as their `subject`.
//!
//! Kept in the image's database rather than computed by listing manifests, because the answer must
//! be cheap on every pull of a signed image and a listing is not. Written by the manifest PUT that
//! creates the referrer, removed by the DELETE that removes it.
use super::store::{Digest, ImageExt};
use crate::Trusted;
use crate::App;
use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Extension,
};
use std::collections::HashMap;
use std::sync::Arc;

/// One row per (subject, referrer). The value is the index ENTRY — the descriptor a client
/// receives — so answering needs no manifest reads at all. Prefixed `image/referrer/` so it
/// cannot collide with the other key spaces sharing this database: the bare `image` and
/// `image/public` keys, `image/tag/`, `image/manifest-type/`, and `upload/`.
fn key(subject: &Digest, referrer: &Digest) -> Vec<u8> {
    format!("image/referrer/{subject}/{referrer}").into_bytes()
}
const PREFIX: &str = "image/referrer/";
fn subject_prefix(subject: &Digest) -> String {
    format!("{PREFIX}{subject}/")
}

/// Record `d` as a referrer, if its manifest names a subject. A manifest with no `subject` is not
/// an error and not a referrer — most manifests are that. Returns the subject digest indexed, if
/// any: `put_manifest` needs it to answer with `OCI-Subject` on the 201, per spec. Into the
/// push's batch rather than its own put, so the row lands with the manifest's others in one flush.
pub fn index(batch: &mut slatedb::WriteBatch, d: &Digest, bytes: &[u8]) -> Option<Digest> {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return None; // not JSON: nothing to index, and PUT already accepted the bytes
    };
    let subject = v.get("subject").and_then(|s| s.get("digest")).and_then(|d| d.as_str())?;
    let subject = Digest::parse(subject)?;
    let mut entry = serde_json::json!({
        "mediaType": v.get("mediaType").and_then(|m| m.as_str())
            .unwrap_or("application/vnd.oci.image.manifest.v1+json"),
        "digest": d.to_string(),
        "size": bytes.len(),
        "annotations": v.get("annotations").cloned().unwrap_or(serde_json::json!({})),
    });
    // Spec: omitted when absent — `json!` would emit `null`, which strict clients reject.
    let artifact_type = v.get("artifactType").and_then(|a| a.as_str())
        .or_else(|| v.get("config").and_then(|c| c.get("mediaType")).and_then(|m| m.as_str()));
    if let Some(t) = artifact_type {
        entry["artifactType"] = serde_json::Value::String(t.to_string());
    }
    batch.put(key(&subject, d), entry.to_string().into_bytes());
    Some(subject)
}

/// Remove `d` from wherever it appears as a referrer. Scans the whole index rather than keeping a
/// reverse map: a manifest delete is rare, and a reverse map is state that can disagree with this
/// one.
pub async fn unindex(app: &App, owner: &str, name: &str, d: &Digest) -> crate::Result<()> {
    let db = app.store.image_db(owner, name).await?;
    crate::store::delete_suffixed(&db, PREFIX, &format!("/{d}")).await
}

/// `GET /referrers/{digest}` — an image index of everything pointing at that digest. Empty is a
/// 200 with an empty `manifests`, never a 404 — including when the image itself does not exist,
/// which is also why an unknown image must not fall through to `image_db`: opening a database
/// creates it, and a GET must not conjure an image the caller never pushed.
pub async fn list(
    State(app): State<Arc<App>>,
    Extension(trusted): Extension<Trusted>,
    headers: HeaderMap,
    Path((owner, name, digest)): Path<(String, String, String)>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if let Err(r) = super::auth::allow(&app, &trusted, &headers, &owner, &name, false).await {
        return r;
    }
    let Some(d) = Digest::parse(&digest) else {
        return super::oci_err(StatusCode::BAD_REQUEST, "DIGEST_INVALID", "malformed digest");
    };
    let mut out = vec![];
    match app.store.image_exists(&owner, &name).await {
        Ok(true) => {
            let db = match app.store.image_db(&owner, &name).await {
                Ok(db) => db,
                Err(e) => return crate::oci_internal(e),
            };
            let mut it = match db.scan_prefix(subject_prefix(&d), ..).await {
                Ok(it) => it,
                Err(e) => return crate::oci_internal(e.into()),
            };
            loop {
                match it.next().await {
                    Ok(Some(kv)) => {
                        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&kv.value) {
                            out.push(v);
                        }
                    }
                    Ok(None) => break,
                    Err(e) => return crate::oci_internal(e.into()),
                }
            }
        }
        Ok(false) => {}
        Err(e) => return crate::oci_internal(e),
    }
    let filter = q.get("artifactType").cloned();
    if let Some(f) = &filter {
        out.retain(|v| v.get("artifactType").and_then(|a| a.as_str()) == Some(f.as_str()));
    }
    let body = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": out,
    });
    let mut r = (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/vnd.oci.image.index.v1+json")],
        body.to_string(),
    )
        .into_response();
    // Announcing the filter is required: a client must be able to tell a filtered answer from a
    // server that ignored the parameter.
    if filter.is_some() {
        r.headers_mut().insert(
            header::HeaderName::from_static("oci-filters-applied"),
            "artifactType".parse().unwrap(),
        );
    }
    r
}
