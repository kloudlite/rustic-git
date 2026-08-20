//! Manifests and the tag map.
//!
//! Manifest BYTES are stored verbatim and returned verbatim. The digest is over those exact bytes,
//! so re-serializing a parsed manifest — even to identical-looking JSON — changes the digest and
//! breaks every client that verifies one. Nothing here parses a manifest except to read `subject`
//! for the referrers index.
use super::{
    auth, oci_err,
    store::{manifest_path, Digest},
};
use crate::http::Trusted;
use crate::App;
use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Extension,
};
use slatedb::object_store::{ObjectStoreExt, PutPayload};
use std::collections::HashMap;
use std::sync::Arc;

const MEDIA_TYPE_KEY_PREFIX: &str = "image/manifest-type/";

/// The largest manifest accepted. Manifests are lists of digests; anything approaching this is not
/// a manifest. `pub` so `routes.rs` can size the manifest route's `DefaultBodyLimit` off the same
/// number — axum's own default (2 MB) is smaller than this and would otherwise 413 a legal push
/// before `put_manifest` ever runs its own check below.
pub const MAX_MANIFEST: usize = 4 * 1024 * 1024;

/// A reference is either a digest or a tag. Tags are the same shape as any other name segment.
enum Reference {
    Digest(Digest),
    Tag(String),
}

fn reference(s: &str) -> Option<Reference> {
    if let Some(d) = Digest::parse(s) {
        return Some(Reference::Digest(d));
    }
    // OCI tag grammar: [a-zA-Z0-9_][a-zA-Z0-9._-]{0,127}
    let ok = s.len() <= 128
        && s.chars().next().is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-');
    ok.then(|| Reference::Tag(s.to_string()))
}

pub async fn put_manifest(
    State(app): State<Arc<App>>,
    Extension(trusted): Extension<Trusted>,
    headers: HeaderMap,
    Path((owner, name, reference_str)): Path<(String, String, String)>,
    body: Bytes,
) -> Response {
    if let Err(r) = auth::allow(&app, &trusted, &headers, &owner, &name, true).await {
        return r;
    }
    if body.len() > MAX_MANIFEST {
        return oci_err(StatusCode::from_u16(413).unwrap(), "SIZE_INVALID", "manifest too large");
    }
    let Some(r) = reference(&reference_str) else {
        return oci_err(StatusCode::BAD_REQUEST, "MANIFEST_INVALID", "malformed reference");
    };
    let d = Digest::of(&body);
    if let Reference::Digest(asked) = &r {
        if asked != &d {
            return oci_err(StatusCode::BAD_REQUEST, "DIGEST_INVALID", "content does not match digest");
        }
    }
    let media = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/vnd.oci.image.manifest.v1+json")
        .to_string();
    if let Err(e) = app.store.os.put(&manifest_path(&owner, &name, &d), PutPayload::from(body.clone())).await {
        return crate::http::internal_pub(e.into());
    }
    // The media type travels with the manifest: a GET must answer the same Content-Type the push
    // declared, and the bytes themselves are not re-parsed to recover it.
    let db = match app.store.image_db(&owner, &name).await {
        Ok(d) => d,
        Err(e) => return crate::http::internal_pub(e),
    };
    if let Err(e) = db
        .put(format!("{MEDIA_TYPE_KEY_PREFIX}{d}").into_bytes(), media.into_bytes())
        .await
    {
        return crate::http::internal_pub(e.into());
    }
    let subject = match super::referrers::index(&app, &owner, &name, &d, &body).await {
        Ok(s) => s,
        Err(e) => return crate::http::internal_pub(e),
    };
    if let Reference::Tag(t) = &r {
        if let Err(e) = app.store.put_tag(&owner, &name, t, &d).await {
            return crate::http::internal_pub(e);
        }
    } else if let Err(e) = app.store.touch_image(&owner, &name).await {
        return crate::http::internal_pub(e);
    }
    let mut resp = (
        StatusCode::CREATED,
        [
            (header::LOCATION, format!("/v2/{owner}/{name}/manifests/{d}")),
            (header::HeaderName::from_static("docker-content-digest"), d.to_string()),
        ],
    )
        .into_response();
    // Spec: a manifest with a `subject` MUST get `OCI-Subject` on the 201, so a client can tell
    // without a GET that the push was indexed as a referrer.
    if let Some(subject) = subject {
        resp.headers_mut().insert(
            header::HeaderName::from_static("oci-subject"),
            subject.to_string().parse().unwrap(),
        );
    }
    resp
}

pub async fn get_manifest(
    State(app): State<Arc<App>>,
    Extension(trusted): Extension<Trusted>,
    headers: HeaderMap,
    Path(p): Path<(String, String, String)>,
) -> Response {
    manifest_response(app, trusted, headers, p, true).await
}

pub async fn head_manifest(
    State(app): State<Arc<App>>,
    Extension(trusted): Extension<Trusted>,
    headers: HeaderMap,
    Path(p): Path<(String, String, String)>,
) -> Response {
    manifest_response(app, trusted, headers, p, false).await
}

async fn manifest_response(
    app: Arc<App>,
    trusted: Trusted,
    headers: HeaderMap,
    (owner, name, reference_str): (String, String, String),
    with_body: bool,
) -> Response {
    if let Err(r) = auth::allow(&app, &trusted, &headers, &owner, &name, false).await {
        return r;
    }
    let Some(r) = reference(&reference_str) else {
        return oci_err(StatusCode::NOT_FOUND, "MANIFEST_UNKNOWN", "no such manifest");
    };
    let d = match r {
        Reference::Digest(d) => d,
        Reference::Tag(t) => match app.store.tag(&owner, &name, &t).await {
            Ok(Some(d)) => {
                // The pull counter. GET by tag only — a HEAD is docker probing, and a GET by
                // digest is docker re-reading what the tag already resolved to; counting either
                // would inflate. Best-effort: a failed bump must not fail the pull it counts.
                if with_body {
                    let _ = app.store.bump_pulls(&owner, &name, &t).await;
                }
                d
            }
            Ok(None) => return oci_err(StatusCode::NOT_FOUND, "MANIFEST_UNKNOWN", "no such tag"),
            Err(e) => return crate::http::internal_pub(e),
        },
    };
    let bytes = match app.store.os.get(&manifest_path(&owner, &name, &d)).await {
        Ok(r) => match r.bytes().await {
            Ok(b) => b,
            Err(e) => return crate::http::internal_pub(e.into()),
        },
        Err(slatedb::object_store::Error::NotFound { .. }) => {
            return oci_err(StatusCode::NOT_FOUND, "MANIFEST_UNKNOWN", "no such manifest")
        }
        Err(e) => return crate::http::internal_pub(e.into()),
    };
    let media = match app.store.image_db(&owner, &name).await {
        Ok(db) => db
            .get(format!("{MEDIA_TYPE_KEY_PREFIX}{d}").into_bytes())
            .await
            .ok()
            .flatten()
            .map(|v| String::from_utf8_lossy(&v).to_string())
            .unwrap_or_else(|| "application/vnd.oci.image.manifest.v1+json".into()),
        Err(e) => return crate::http::internal_pub(e),
    };
    let hdrs = [
        (header::CONTENT_TYPE, media),
        (header::CONTENT_LENGTH, bytes.len().to_string()),
        (header::HeaderName::from_static("docker-content-digest"), d.to_string()),
    ];
    if with_body {
        (StatusCode::OK, hdrs, bytes).into_response()
    } else {
        (StatusCode::OK, hdrs).into_response()
    }
}

/// By tag: unlink the tag. By digest: remove the manifest AND every tag that pointed at it —
/// leaving a tag resolving to bytes that are gone would turn every pull of it into a 404 the owner
/// cannot explain.
pub async fn delete_manifest(
    State(app): State<Arc<App>>,
    Extension(trusted): Extension<Trusted>,
    headers: HeaderMap,
    Path((owner, name, reference_str)): Path<(String, String, String)>,
) -> Response {
    if let Err(r) = auth::allow(&app, &trusted, &headers, &owner, &name, true).await {
        return r;
    }
    let Some(r) = reference(&reference_str) else {
        return oci_err(StatusCode::NOT_FOUND, "MANIFEST_UNKNOWN", "no such manifest");
    };
    match r {
        Reference::Tag(t) => match app.store.tag(&owner, &name, &t).await {
            Ok(Some(_)) => match app.store.delete_tag(&owner, &name, &t).await {
                Ok(()) => StatusCode::ACCEPTED.into_response(),
                Err(e) => crate::http::internal_pub(e),
            },
            Ok(None) => oci_err(StatusCode::NOT_FOUND, "MANIFEST_UNKNOWN", "no such tag"),
            Err(e) => crate::http::internal_pub(e),
        },
        Reference::Digest(d) => {
            let tags = match app.store.tags(&owner, &name).await {
                Ok(t) => t,
                Err(e) => return crate::http::internal_pub(e),
            };
            for t in tags {
                if app.store.tag(&owner, &name, &t).await.ok().flatten().as_ref() == Some(&d) {
                    if let Err(e) = app.store.delete_tag(&owner, &name, &t).await {
                        return crate::http::internal_pub(e);
                    }
                }
            }
            if let Err(e) = super::referrers::unindex(&app, &owner, &name, &d).await {
                return crate::http::internal_pub(e);
            }
            match app.store.os.delete(&manifest_path(&owner, &name, &d)).await {
                Ok(()) => StatusCode::ACCEPTED.into_response(),
                Err(slatedb::object_store::Error::NotFound { .. }) => {
                    oci_err(StatusCode::NOT_FOUND, "MANIFEST_UNKNOWN", "no such manifest")
                }
                Err(e) => crate::http::internal_pub(e.into()),
            }
        }
    }
}

/// `GET /tags/list?n=&last=` — lexical order, `last` exclusive, `Link` when truncated.
pub async fn tags_list(
    State(app): State<Arc<App>>,
    Extension(trusted): Extension<Trusted>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if let Err(r) = auth::allow(&app, &trusted, &headers, &owner, &name, false).await {
        return r;
    }
    let all = match app.store.tags(&owner, &name).await {
        Ok(t) => t,
        Err(e) => return crate::http::internal_pub(e),
    };
    if all.is_empty() && !app.store.image_exists(&owner, &name).await.unwrap_or(false) {
        return oci_err(StatusCode::NOT_FOUND, "NAME_UNKNOWN", "no such image");
    }
    let (page, truncated) = super::paginate(&all, &q);
    let body = serde_json::json!({"name": format!("{owner}/{name}"), "tags": page});
    let mut r = axum::Json(body).into_response();
    if let Some(last) = truncated {
        let n = q.get("n").cloned().unwrap_or_default();
        r.headers_mut().insert(
            header::LINK,
            format!("</v2/{owner}/{name}/tags/list?n={n}&last={last}>; rel=\"next\"")
                .parse()
                .unwrap(),
        );
    }
    r
}
