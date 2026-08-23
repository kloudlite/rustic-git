//! Manifests and the tag map.
//!
//! Manifest BYTES are stored verbatim and returned verbatim. The digest is over those exact bytes,
//! so re-serializing a parsed manifest — even to identical-looking JSON — changes the digest and
//! breaks every client that verifies one. Nothing here parses a manifest except to read `subject`
//! for the referrers index.
use super::store::blob_path;
use super::{
    auth, oci_err,
    store::{manifest_path, Digest},
};
use std::collections::HashSet;
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
    axum::extract::RawQuery(raw_query): axum::extract::RawQuery,
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
    // Hash with the algorithm the CLIENT chose: a push by sha512 digest must verify against
    // sha512 and be stored under it, or every sha512 GET after a 201 would be a 404. A push by
    // tag has no claimed algorithm and gets the default.
    let d = match &r {
        Reference::Digest(asked) => match Digest::of_algo(&asked.algo, &body) {
            Some(actual) if &actual == asked => actual,
            _ => {
                return oci_err(StatusCode::BAD_REQUEST, "DIGEST_INVALID", "content does not match digest")
            }
        },
        Reference::Tag(_) => {
            // A by-tag push declares no algorithm, so sha256 is the default — but these exact
            // bytes may already be stored under ANOTHER algorithm (a client that pushed by
            // sha512 digest and now pushes the same manifest by tag). Repointing the tag at a
            // freshly minted sha256 would silently strip the identity the client already uses,
            // so prefer whichever digest the store already knows these bytes by.
            let sha256 = Digest::of(&body);
            match Digest::of_algo("sha512", &body) {
                Some(sha512)
                    if app.store.os.head(&manifest_path(&owner, &name, &sha256)).await.is_err()
                        && app.store.os.head(&manifest_path(&owner, &name, &sha512)).await.is_ok() =>
                {
                    sha512
                }
                _ => sha256,
            }
        }
    };
    // Every blob the manifest names must already be here, or the 201 would promise bytes the
    // registry does not hold (the spec's MANIFEST_BLOB_UNKNOWN). An index names MANIFESTS in
    // `manifests[].digest`, so "here" is either store. `subject` is exempt: a referrer may be
    // pushed before the thing it refers to.
    // ponytail: a sweep can still delete an old blob between this head and the put below — the
    // window is one request wide, down from "forever" when the mtime refresh silently failed on
    // S3. If it ever bites, write a `touch/{owner}/{algo}/{hex}` marker here and have
    // `gc::sweep_owner` treat the marker's mtime as the blob's.
    // `v` is pruned before the walk, not after: `subject` may legally point at a manifest that has
    // not been pushed yet, and a FOREIGN/nondistributable layer (a `urls` list, or a
    // foreign/nondistributable mediaType — Windows base images) is by spec fetched from elsewhere
    // and never held here. GC's walk stays unpruned: over-collecting there only keeps blobs alive.
    let mut v: serde_json::Value = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
    if let Some(m) = v.as_object_mut() {
        m.remove("subject");
        if let Some(layers) = m.get_mut("layers").and_then(|l| l.as_array_mut()) {
            layers.retain(|l| {
                let elsewhere = l.get("urls").and_then(|u| u.as_array()).is_some_and(|u| !u.is_empty());
                let foreign = l.get("mediaType").and_then(|t| t.as_str()).is_some_and(|t| {
                    t.contains(".foreign.") || t.contains(".nondistributable.")
                });
                !elsewhere && !foreign
            });
        }
    }
    let mut named = HashSet::new();
    super::gc::collect(&v, &mut named);
    for s in &named {
        let Some(bd) = Digest::parse(s) else {
            return oci_err(StatusCode::BAD_REQUEST, "MANIFEST_INVALID", "malformed digest in manifest");
        };
        let here = app.store.os.head(&blob_path(&owner, &bd)).await.is_ok()
            || app.store.os.head(&manifest_path(&owner, &name, &bd)).await.is_ok();
        if !here {
            return oci_err(StatusCode::NOT_FOUND, "MANIFEST_BLOB_UNKNOWN", "manifest references a blob this registry does not hold");
        }
    }
    let media = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/vnd.oci.image.manifest.v1+json")
        .to_string();
    if let Err(e) = app.store.os.put(&manifest_path(&owner, &name, &d), PutPayload::from(body.clone())).await {
        return crate::registry::oci_internal(e.into());
    }
    // The media type travels with the manifest: a GET must answer the same Content-Type the push
    // declared, and the bytes themselves are not re-parsed to recover it.
    let db = match app.store.image_db(&owner, &name).await {
        Ok(d) => d,
        Err(e) => return crate::registry::oci_internal(e),
    };
    if let Err(e) = db
        .put(format!("{MEDIA_TYPE_KEY_PREFIX}{d}").into_bytes(), media.into_bytes())
        .await
    {
        return crate::registry::oci_internal(e.into());
    }
    let subject = match super::referrers::index(&app, &owner, &name, &d, &body).await {
        Ok(s) => s,
        Err(e) => return crate::registry::oci_internal(e),
    };
    if let Reference::Tag(t) = &r {
        if let Err(e) = app.store.put_tag(&owner, &name, t, &d).await {
            return crate::registry::oci_internal(e);
        }
    } else {
        // A push BY DIGEST may still name tags, as `?tag=` query parameters (the spec's tag
        // param, possibly repeated). Each valid one points at this manifest; invalid ones are
        // refused rather than skipped, because a client that asked for a tag and did not get
        // it has been lied to by a 201.
        for (k, v) in form_urlencoded::parse(raw_query.as_deref().unwrap_or("").as_bytes()) {
            if k != "tag" {
                continue;
            }
            if reference(&v).is_none_or(|r| matches!(r, Reference::Digest(_))) {
                return oci_err(StatusCode::BAD_REQUEST, "TAG_INVALID", "malformed tag parameter");
            }
            if let Err(e) = app.store.put_tag(&owner, &name, &v, &d).await {
                return crate::registry::oci_internal(e);
            }
        }
        if let Err(e) = app.store.touch_image(&owner, &name).await {
            return crate::registry::oci_internal(e);
        }
    }
    // Marker is a view, never the source of truth: log-and-continue rather than fail a push that
    // already landed the manifest and tag(s).
    if let Err(e) = app.store.refresh_image_marker(&owner, &name).await {
        eprintln!("index refresh img {owner}/{name}: {e}"); // ponytail: eprintln
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
            Err(e) => return crate::registry::oci_internal(e),
        },
    };
    let bytes = match app.store.os.get(&manifest_path(&owner, &name, &d)).await {
        Ok(r) => match r.bytes().await {
            Ok(b) => b,
            Err(e) => return crate::registry::oci_internal(e.into()),
        },
        Err(slatedb::object_store::Error::NotFound { .. }) => {
            return oci_err(StatusCode::NOT_FOUND, "MANIFEST_UNKNOWN", "no such manifest")
        }
        Err(e) => return crate::registry::oci_internal(e.into()),
    };
    let media = match app.store.image_db(&owner, &name).await {
        Ok(db) => db
            .get(format!("{MEDIA_TYPE_KEY_PREFIX}{d}").into_bytes())
            .await
            .ok()
            .flatten()
            .map(|v| String::from_utf8_lossy(&v).to_string())
            .unwrap_or_else(|| "application/vnd.oci.image.manifest.v1+json".into()),
        Err(e) => return crate::registry::oci_internal(e),
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
                Err(e) => crate::registry::oci_internal(e),
            },
            Ok(None) => oci_err(StatusCode::NOT_FOUND, "MANIFEST_UNKNOWN", "no such tag"),
            Err(e) => crate::registry::oci_internal(e),
        },
        Reference::Digest(d) => {
            let tags = match app.store.tags(&owner, &name).await {
                Ok(t) => t,
                Err(e) => return crate::registry::oci_internal(e),
            };
            for t in tags {
                if app.store.tag(&owner, &name, &t).await.ok().flatten().as_ref() == Some(&d) {
                    if let Err(e) = app.store.delete_tag(&owner, &name, &t).await {
                        return crate::registry::oci_internal(e);
                    }
                }
            }
            if let Err(e) = super::referrers::unindex(&app, &owner, &name, &d).await {
                return crate::registry::oci_internal(e);
            }
            // The media-type row lives in the image DB, not the object store, so
            // it survives independently of the manifest object below — delete it
            // here or it's an orphan forever (never swept, never read again).
            match app.store.image_db(&owner, &name).await {
                Ok(db) => {
                    if let Err(e) = db.delete(format!("{MEDIA_TYPE_KEY_PREFIX}{d}").into_bytes()).await {
                        return crate::registry::oci_internal(e.into());
                    }
                }
                Err(e) => return crate::registry::oci_internal(e),
            }
            match app.store.os.delete(&manifest_path(&owner, &name, &d)).await {
                Ok(()) => StatusCode::ACCEPTED.into_response(),
                Err(slatedb::object_store::Error::NotFound { .. }) => {
                    oci_err(StatusCode::NOT_FOUND, "MANIFEST_UNKNOWN", "no such manifest")
                }
                Err(e) => crate::registry::oci_internal(e.into()),
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
        Err(e) => return crate::registry::oci_internal(e),
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
