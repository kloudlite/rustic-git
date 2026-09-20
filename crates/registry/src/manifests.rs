//! Manifests and the tag map.
//!
//! Manifest BYTES are stored verbatim and returned verbatim. The digest is over those exact bytes,
//! so re-serializing a parsed manifest — even to identical-looking JSON — changes the digest and
//! breaks every client that verifies one. Nothing here parses a manifest except to read `subject`
//! for the referrers index.
use super::{blob_state, store::{manifest_prefix, ImageExt}};

/// Whether this image holds any sha512-digested manifest at all. One LIST, stopped at the first
/// object: the by-tag push path asks before paying a second full-body hash. `false` on a read
/// error is correct here and not a suppressed failure — it only means "do not take the optional
/// sha512 branch", and the sha256 answer the caller falls back to is the spec's default.
async fn has_sha512_manifest(app: &App, owner: &str, name: &str) -> bool {
    use futures::StreamExt;
    let prefix = manifest_prefix(owner, name).join("sha512");
    app.store.os.list(Some(&prefix)).next().await.is_some_and(|r| r.is_ok())
}
use super::{
    auth, oci_err,
    store::{manifest_path, Digest},
};
use std::collections::HashSet;
use crate::Trusted;
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

/// `image/manifest-meta/{digest}` → `{size}\n{pushed_ms}\n{declared bytes}`, written with the
/// manifest so the image page can list hundreds of tags without a GET per manifest. Absent for
/// manifests pushed before this row existed; readers fall back to the object.
pub fn manifest_meta_key(d: &Digest) -> Vec<u8> {
    format!("image/manifest-meta/{d}").into_bytes()
}

/// What the manifest says pulling it transfers: config plus every layer (or every entry of an
/// index), as declared. Read from the bytes, never stored by anything else, so it cannot disagree
/// with the manifest. A display hint, not a size the registry checks against the blobs.
pub fn declared_size(bytes: &[u8]) -> u64 {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(bytes) else { return 0 };
    let mut total = v.get("config").and_then(|c| c.get("size")).and_then(|s| s.as_u64()).unwrap_or(0);
    for key in ["layers", "manifests"] {
        if let Some(items) = v.get(key).and_then(|l| l.as_array()) {
            // saturating: an attacker-controlled manifest can list sizes near u64::MAX; this is
            // a size hint for display, not an allocation, so clamping beats panicking/wrapping.
            total = items
                .iter()
                .filter_map(|l| l.get("size")?.as_u64())
                .fold(total, |acc, s| acc.saturating_add(s));
        }
    }
    total
}

/// Whether every `config.digest`, `layers[].digest` and `manifests[].digest` parses. Only those
/// three — annotations and `subject` are read by nothing that deletes.
fn declared_digests_parse(v: &serde_json::Value) -> bool {
    let ok = |d: &serde_json::Value| match d.get("digest") {
        Some(s) => s.as_str().is_some_and(|s| Digest::parse(s).is_some()),
        None => true,
    };
    v.get("config").is_none_or(ok)
        && ["layers", "manifests"].iter().all(|k| v.get(k).and_then(|l| l.as_array()).is_none_or(|l| l.iter().all(ok)))
}

/// The largest manifest accepted. Manifests are lists of digests; anything approaching this is not
/// a manifest. `pub` so `routes.rs` can size the manifest route's `DefaultBodyLimit` off the same
/// number — axum's own default (2 MB) is smaller than this and would otherwise 413 a legal push
/// before `put_manifest` ever runs its own check below.
pub const MAX_MANIFEST: usize = 4 * 1024 * 1024;

/// How much of an oversized manifest body is swallowed before the connection is simply dropped.
const DRAIN_CEILING: usize = 64 * 1024 * 1024;

/// The manifest body, or `None` when it is over `MAX_MANIFEST` — and in that case the rest of it
/// has been READ AND DISCARDED (up to `DRAIN_CEILING`) before the caller answers. Refusing while
/// the body is still arriving is what axum's own body limit does, and hyper then closes the
/// connection with the request half-sent; ingress-nginx, still writing that body upstream, sees
/// a broken pipe and answers the client 502 instead of relaying the 413 — a limit the client
/// cannot tell from an outage. Draining first keeps the connection whole so the refusal gets
/// through.
async fn read_manifest(body: axum::body::Body) -> Option<Bytes> {
    use futures::StreamExt;
    let mut stream = body.into_data_stream();
    let (mut buf, mut seen) = (Vec::new(), 0usize);
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.ok()?;
        seen += chunk.len();
        if seen > DRAIN_CEILING {
            return None;
        }
        if seen <= MAX_MANIFEST {
            buf.extend_from_slice(&chunk);
        }
    }
    (seen <= MAX_MANIFEST).then(|| buf.into())
}

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

/// Unpin every digest this publication pinned, concurrently, logging (never failing the caller)
/// on any release that did not land. CHANGE 1: called on ALL FIVE exits after the first pin lands
/// (pin error, probe error, MANIFEST_BLOB_UNKNOWN, manifest write error, success) — the three
/// hand-written serial unpin loops this replaces each covered only their own exit, which is how
/// the manifest-write failure exit ended up releasing nothing at all.
async fn release_pins(os: &(dyn slatedb::object_store::ObjectStore + '_), owner: &str, pinned: &[Digest], publication: &str) {
    if pinned.is_empty() {
        return;
    }
    let results: Vec<(Digest, crate::Result<()>)> = futures::StreamExt::collect(futures::StreamExt::buffered(
        futures::StreamExt::map(futures::stream::iter(pinned.to_vec()), move |bd| async move {
            let r = blob_state::unpin(os, owner, &bd, publication).await;
            (bd, r)
        }),
        crate::gc::STAT_CONCURRENCY,
    ))
    .await;
    for (bd, r) in results {
        if let Err(e) = r {
            tracing::warn!(owner = %owner, digest = %bd, error = %e, "registry.blob.unpin.failed");
        }
    }
}

/// Why `pin_probe_write` did not land the manifest — mapped to the exact OCI response
/// `put_manifest` used to build inline at each of the four failing exits. `pub` only because
/// `pin_probe_write` is (see its own doc): a private handler's own error enum has no other reason
/// to leave this module.
pub enum PinFailure {
    /// A `blob_state::pin` call errored (CAS exhaustion, a store error).
    Pin(crate::Error),
    /// A HEAD after pinning errored (not merely "not found" — that is `BlobUnknown`).
    Probe(crate::Error),
    /// Every digest probed cleanly but at least one blob this manifest names is not here.
    BlobUnknown,
    /// The manifest bytes themselves failed to write.
    Write(crate::Error),
}

/// Everything between the first pin and the final release, extracted so it can be driven directly
/// against a wrapped `ObjectStore` in a test (R-2 T4) — `put_manifest` is an axum handler with no
/// seam in the existing HTTP test harness (`tests/common`) to inject a store failure mid-request,
/// so this function IS that seam: pin every digest (CHANGE 2, concurrent and never
/// short-circuiting), probe presence, write the manifest bytes, and release every pin ON EVERY
/// EXIT (CHANGE 1) — including success, which is why the caller has nothing left to release.
/// `pub` (not `pub(crate)`) so `tests/registry_manifests.rs` (a separate crate) can call it
/// directly — the same reason `gc::referenced` is `pub` per its own module doc.
pub async fn pin_probe_write(
    os: &(dyn slatedb::object_store::ObjectStore + '_),
    owner: &str,
    name: &str,
    d: &Digest,
    digests: &[Digest],
    body: &Bytes,
) -> std::result::Result<(), PinFailure> {
    let publication = blob_state::manifest_publication(owner, name, d);
    // CHANGE 2: concurrent, never short-circuiting. A `for` loop that returns on the FIRST pin
    // error is exactly how a pin leaks (R-2 / review Rust Important) — every earlier pin in the
    // loop was never released. `buffered` runs all of them, in input order, and only after every
    // one has answered does this decide what to do: build `pinned` from the successes, then (if
    // any failed) release everything that DID land and report the first error.
    let pin_results: Vec<(Digest, crate::Result<bool>)> = futures::StreamExt::collect(futures::StreamExt::buffered(
        futures::StreamExt::map(futures::stream::iter(digests.to_vec()), {
            let owner = owner.to_string();
            let publication = publication.clone();
            move |bd| {
                let owner = owner.clone();
                let publication = publication.clone();
                async move {
                    let r = blob_state::pin(os, &owner, &bd, &publication).await;
                    (bd, r)
                }
            }
        }),
        crate::gc::STAT_CONCURRENCY,
    ))
    .await;
    let pinned: Vec<Digest> = pin_results.iter().filter(|(_, r)| matches!(r, Ok(true))).map(|(bd, _)| bd.clone()).collect();
    if let Some((_, Err(e))) = pin_results.into_iter().find(|(_, r)| r.is_err()) {
        release_pins(os, owner, &pinned, &publication).await;
        return Err(PinFailure::Pin(e));
    }
    // Concurrent, not serial: a 40-layer manifest was up to 80 sequential HEADs before the
    // write. A digest is accepted as a blob only when this publication pinned its active
    // generation. An unpinned digest may still be a child manifest in an index.
    let probes: Vec<_> = digests
        .iter()
        .map(|bd| {
            let owner = owner.to_string();
            let name = name.to_string();
            let pinned = pinned.clone();
            let bd = bd.clone();
            async move {
            if pinned.iter().any(|p| p == &bd) {
                let Some(path) = blob_state::resolve(os, &owner, &bd).await? else {
                    return Ok(false);
                };
                return match os.head(&path).await {
                    Ok(_) => Ok(true),
                    Err(slatedb::object_store::Error::NotFound { .. }) => Ok(false),
                    Err(e) => Err(e.into()),
                };
            }
            match os.head(&manifest_path(&owner, &name, &bd)).await {
                Ok(_) => Ok(true),
                Err(slatedb::object_store::Error::NotFound { .. }) => Ok(false),
                Err(e) => Err(e.into()),
            }
            }
        })
        .collect();
    let present: Vec<crate::Result<bool>> =
        futures::StreamExt::collect::<Vec<crate::Result<bool>>>(futures::StreamExt::buffered(futures::stream::iter(probes), crate::gc::STAT_CONCURRENCY)).await;
    if let Some(error) = present.iter().find_map(|r| r.as_ref().err()) {
        let msg = error.to_string();
        release_pins(os, owner, &pinned, &publication).await;
        return Err(PinFailure::Probe(crate::err(msg)));
    }
    if present.iter().any(|ok| matches!(ok, Ok(false))) {
        release_pins(os, owner, &pinned, &publication).await;
        return Err(PinFailure::BlobUnknown);
    }
    // The media type travels with the manifest: a GET must answer the same Content-Type the push
    // declared, and the bytes themselves are not re-parsed to recover it.
    if let Err(e) = os.put(&manifest_path(owner, name, d), PutPayload::from(body.clone())).await {
        // CHANGE 1: this exit released NOTHING before R-2 — every earlier pin stayed held forever
        // (the sweep skips any blob with a live pin, and a retried push mints a fresh publication
        // nonce that never matches the leaked one).
        release_pins(os, owner, &pinned, &publication).await;
        return Err(PinFailure::Write(e.into()));
    }
    release_pins(os, owner, &pinned, &publication).await;
    Ok(())
}

pub async fn put_manifest(
    State(app): State<Arc<App>>,
    Extension(trusted): Extension<Trusted>,
    headers: HeaderMap,
    Path((owner, name, reference_str)): Path<(String, String, String)>,
    axum::extract::RawQuery(raw_query): axum::extract::RawQuery,
    body: axum::body::Body,
) -> Response {
    if let Err(r) = auth::allow(&app, &trusted, &headers, &owner, &name, true).await {
        return r;
    }
    let Some(body) = read_manifest(body).await else {
        return oci_err(StatusCode::PAYLOAD_TOO_LARGE, "SIZE_INVALID", "manifest too large");
    };
    // Parsed once, to READ — never re-emitted (the digest is over the bytes as sent). Anything
    // that is not a JSON OBJECT is refused here: `gc::referenced` cannot walk it for the blobs it
    // names and would otherwise abort every sweep for this owner, forever, on one bad push. A
    // bare `[]`, `"x"`, `3` or `null` parses but is no more walkable than garbage.
    let Some(mut v) =
        serde_json::from_slice::<serde_json::Value>(&body).ok().filter(serde_json::Value::is_object)
    else {
        return oci_err(StatusCode::BAD_REQUEST, "MANIFEST_INVALID", "manifest is not a JSON object");
    };
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
            // so prefer whichever digest the store already knows these bytes by. The sha512 is
            // hashed only when the sha256 object is absent: the common case pays one HEAD.
            let sha256 = Digest::of(&body);
            if app.store.os.head(&manifest_path(&owner, &name, &sha256)).await.is_ok() {
                sha256
            } else if has_sha512_manifest(&app, &owner, &name).await {
                // Gated on the image ACTUALLY holding a sha512 manifest: almost none do, and
                // hashing every by-tag push a second time (sha512 over the whole body) to then
                // HEAD an object that was never going to be there was pure cost on the hot push
                // path (2026-09-12). One bounded LIST decides it.
                match Digest::of_algo("sha512", &body) {
                    Some(sha512)
                        if app.store.os.head(&manifest_path(&owner, &name, &sha512)).await.is_ok() =>
                    {
                        sha512
                    }
                    _ => sha256,
                }
            } else {
                sha256
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
    // The digests the manifest DECLARES (layers, config, an index's manifests) must parse: GC's
    // reference walk skips anything `Digest::parse` refuses, so a manifest naming `sha256:XYZ`
    // would be accepted and then have that blob swept from under it.
    if !declared_digests_parse(&v) {
        return oci_err(StatusCode::BAD_REQUEST, "MANIFEST_INVALID", "a declared digest is not a valid digest");
    }
    let mut named = HashSet::new();
    super::gc::collect(&v, &mut named);
    // The walk grabs EVERY value keyed `digest` anywhere in the document, annotations included —
    // so a string that is not a digest is not a malformed manifest, it is a field this walk has no
    // business reading. Skipped rather than refused: the walk's over-collection is only safe
    // because it is advisory in both directions (GC keeps what it over-collects, and this only
    // decides what to probe for), and refusing here turned that leniency into a rejected push.
    let digests: Vec<Digest> = named.iter().filter_map(|s| Digest::parse(s)).collect();
    let mut tags: Vec<String> = Vec::new();
    match &r {
        Reference::Tag(t) => tags.push(t.clone()),
        Reference::Digest(_) => {
            for (k, v) in form_urlencoded::parse(raw_query.as_deref().unwrap_or("").as_bytes()) {
                if k != "tag" {
                    continue;
                }
                if reference(&v).is_none_or(|r| matches!(r, Reference::Digest(_))) {
                    return oci_err(StatusCode::BAD_REQUEST, "TAG_INVALID", "malformed tag parameter");
                }
                tags.push(v.into_owned());
            }
        }
    }
    let media = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/vnd.oci.image.manifest.v1+json")
        .to_string();
    if let Err(f) = pin_probe_write(&app.store.os, &owner, &name, &d, &digests, &body).await {
        return match f {
            PinFailure::Pin(e) | PinFailure::Probe(e) => crate::oci_internal(e),
            PinFailure::BlobUnknown => oci_err(StatusCode::NOT_FOUND, "MANIFEST_BLOB_UNKNOWN", "manifest references a blob this registry does not hold"),
            PinFailure::Write(e) => crate::oci_internal(e),
        };
    }
    // One re-runnable unit, so the fence arm can replay the whole thing: every row this push
    // writes goes in ONE batch, and a retry has to rebuild it (a `WriteBatch` is consumed by the
    // write that failed).
    let subject = match crate::fenced_retry(&app, &owner, &name, false, || {
        push_rows(&app, &owner, &name, &d, &media, &digests, &body, &tags)
    })
    .await
    {
        Ok(s) => s,
        Err(r) => return r,
    };
    // A re-push of the same digest may declare a new Content-Type; the cached answer would keep
    // serving the old one otherwise.
    app.store.manifests().remove(&format!("{owner}/{name}/{d}"));
    // Marker is a view, never the source of truth: log-and-continue rather than fail a push that
    // already landed the manifest and tag(s).
    if let Err(e) = app.store.refresh_image_marker(&owner, &name).await {
        tracing::warn!(owner = %owner, name = %name, reason = "put", error = %e, "registry.marker.refresh.failed");
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
        if let Ok(v) = header::HeaderValue::from_str(&subject.to_string()) {
            resp.headers_mut().insert(header::HeaderName::from_static("oci-subject"), v);
        }
    }
    resp
}

/// The rows a manifest push writes, in ONE batch and only after the bytes landed: the media type,
/// the blob rows, the referrer row, the tag(s), the manifest counters. One WAL flush instead of one
/// per row — a multi-arch push was N × 7 flush waits — and atomic, so a stranger resolving the tag
/// can never find a layer this image does not yet admit holding (the rule that used to be an
/// ordering between separate puts). Returns the manifest's `subject`, when it has one.
#[allow(clippy::too_many_arguments)]
async fn push_rows(
    app: &App,
    owner: &str,
    name: &str,
    d: &Digest,
    media: &str,
    digests: &[Digest],
    body: &Bytes,
    tags: &[String],
) -> crate::Result<Option<Digest>> {
    let db = app.store.image_db(owner, name).await?;
    // Read BEFORE the batch: the media-type row is written for every manifest and no other, so it
    // is what says whether this digest is new to the image — which is what the manifest counter
    // needs and what used to cost a full prefix LIST. A read error reads as "already there", which
    // only ever under-counts; the GC reconcile is what corrects drift either way.
    let existed =
        db.get(format!("{MEDIA_TYPE_KEY_PREFIX}{d}").into_bytes()).await.map(|v| v.is_some()).unwrap_or(true);
    let mut batch = slatedb::WriteBatch::new();
    batch.put(format!("{MEDIA_TYPE_KEY_PREFIX}{d}").into_bytes(), media.as_bytes());
    batch.put(
        manifest_meta_key(d),
        format!("{}\n{}\n{}", body.len(), crate::ownership::now_ms(), declared_size(body)).into_bytes(),
    );
    super::store::note_blobs(&mut batch, digests, &d.to_string());
    let subject = super::referrers::index(&mut batch, d, body);
    for t in tags {
        super::store::batch_tag(&mut batch, t, d);
    }
    super::store::batch_image(&mut batch);
    // Counters are the marker's inputs and the GC reconcile rewrites a drifted marker, so a
    // failure to compute them is logged, never a failed push.
    if let Err(e) = app.store.note_manifest_put(&mut batch, owner, name, existed).await {
        tracing::warn!(owner = %owner, name = %name, reason = "put", error = %e, "registry.counter.write.failed");
    }
    db.write(batch).await?;
    Ok(subject)
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
    // The tag this GET resolved, if any — the pull is counted where the bytes are actually
    // served, not here: a tag whose manifest object is gone 404s below, and counting at
    // resolution inflated the number by every one of those.
    let mut pulled_tag: Option<String> = None;
    let d = match r {
        Reference::Digest(d) => d,
        Reference::Tag(t) => match crate::fenced_retry(&app, &owner, &name, false, || {
            app.store.tag(&owner, &name, &t)
        })
        .await
        {
            Ok(Some(d)) => {
                // GET by tag only — a HEAD is docker probing, and a GET by digest is docker
                // re-reading what the tag already resolved to; counting either would inflate.
                if with_body {
                    pulled_tag = Some(t);
                }
                d
            }
            Ok(None) => return oci_err(StatusCode::NOT_FOUND, "MANIFEST_UNKNOWN", "no such tag"),
            Err(r) => return r,
        },
    };
    let cache_key = format!("{owner}/{name}/{d}");
    if let Some((bytes, media)) = app.store.manifests().get(&cache_key).cloned() {
        let hdrs = [
            (header::CONTENT_TYPE, media),
            (header::CONTENT_LENGTH, bytes.len().to_string()),
            (header::HeaderName::from_static("docker-content-digest"), d.to_string()),
        ];
        // A map increment only — no lock, no write — so a hundred concurrent pulls of one tag do
        // not queue behind each other here.
        if let Some(t) = &pulled_tag {
            app.store.bump_pulls(&owner, &name, t);
        }
        return if with_body { (StatusCode::OK, hdrs, bytes).into_response() } else { (StatusCode::OK, hdrs).into_response() };
    }
    let bytes = match app.store.os.get(&manifest_path(&owner, &name, &d)).await {
        Ok(r) => match r.bytes().await {
            Ok(b) => b,
            Err(e) => return crate::oci_internal(e.into()),
        },
        Err(slatedb::object_store::Error::NotFound { .. }) => {
            return oci_err(StatusCode::NOT_FOUND, "MANIFEST_UNKNOWN", "no such manifest")
        }
        Err(e) => return crate::oci_internal(e.into()),
    };
    let media = match crate::fenced_retry(&app, &owner, &name, false, || async {
        let db = app.store.image_db(&owner, &name).await?;
        Ok(db.get(format!("{MEDIA_TYPE_KEY_PREFIX}{d}").into_bytes()).await?)
    })
    .await
    {
        Ok(v) => v
            .map(|v| String::from_utf8_lossy(&v).to_string())
            .unwrap_or_else(|| "application/vnd.oci.image.manifest.v1+json".into()),
        Err(r) => return r,
    };
    app.store.manifests().insert(cache_key, (bytes.clone(), media.clone()));
    if let Some(t) = &pulled_tag {
        app.store.bump_pulls(&owner, &name, t);
    }
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
    // `image_db` creates what it opens; a delete aimed at nothing must not leave a phantom image
    // for the listing (and the worker's reconcile) to find.
    match crate::fenced_retry(&app, &owner, &name, false, || app.store.image_exists(&owner, &name)).await {
        Ok(true) => {}
        Ok(false) => return oci_err(StatusCode::NOT_FOUND, "MANIFEST_UNKNOWN", "no such manifest"),
        Err(r) => return r,
    }
    match r {
        // Unlinking a tag is idempotent, so the whole read-then-delete is what the fence arm
        // re-runs: a retry that only deleted would answer 202 for a tag that never existed.
        Reference::Tag(t) => match crate::fenced_retry(&app, &owner, &name, false, || async {
            match app.store.tag(&owner, &name, &t).await? {
                Some(_) => app.store.delete_tag(&owner, &name, &t).await.map(|()| true),
                None => Ok(false),
            }
        })
        .await
        {
            Ok(true) => StatusCode::ACCEPTED.into_response(),
            Ok(false) => oci_err(StatusCode::NOT_FOUND, "MANIFEST_UNKNOWN", "no such tag"),
            Err(r) => r,
        },
        Reference::Digest(d) => {
            match crate::fenced_retry(&app, &owner, &name, false, || async {
                for t in app.store.tags_pointing_at(&owner, &name, &d).await? {
                    app.store.delete_tag(&owner, &name, &t).await?;
                }
                Ok(())
            })
            .await
            {
                Ok(()) => {}
                Err(r) => return r,
            }
            if let Err(r) = crate::fenced_retry(&app, &owner, &name, false, || {
                super::referrers::unindex(&app, &owner, &name, &d)
            })
            .await
            {
                return r;
            }
            // The media-type row lives in the image DB, not the object store, so
            // it survives independently of the manifest object below — delete it
            // here or it's an orphan forever (never swept, never read again).
            if let Err(r) = crate::fenced_retry(&app, &owner, &name, false, || async {
                let db = app.store.image_db(&owner, &name).await?;
                db.delete(format!("{MEDIA_TYPE_KEY_PREFIX}{d}").into_bytes()).await?;
                db.delete(manifest_meta_key(&d)).await?;
                super::store::forget_manifest_blobs(&db, &d).await
            })
            .await
            {
                return r;
            }
            app.store.manifests().remove(&format!("{owner}/{name}/{d}"));
            match app.store.os.delete(&manifest_path(&owner, &name, &d)).await {
                Ok(()) => {
                    if let Err(e) = app.store.note_manifest_deleted(&owner, &name).await {
                        tracing::warn!(owner = %owner, name = %name, reason = "delete", error = %e, "registry.counter.write.failed");
                    }
                    StatusCode::ACCEPTED.into_response()
                }
                Err(slatedb::object_store::Error::NotFound { .. }) => {
                    oci_err(StatusCode::NOT_FOUND, "MANIFEST_UNKNOWN", "no such manifest")
                }
                Err(e) => crate::oci_internal(e.into()),
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
    let all = match crate::fenced_retry(&app, &owner, &name, false, || app.store.tags(&owner, &name)).await {
        Ok(t) => t,
        Err(r) => return r,
    };
    if all.is_empty() {
        // A fence here must not read as "no such image": swallowing it answered 404 NAME_UNKNOWN
        // for an image that is merely being served by another node now.
        match crate::fenced_retry(&app, &owner, &name, false, || app.store.image_exists(&owner, &name)).await {
            Ok(false) => return oci_err(StatusCode::NOT_FOUND, "NAME_UNKNOWN", "no such image"),
            Ok(true) => {}
            Err(r) => return r,
        }
    }
    let (page, truncated) = super::paginate(&all, &q);
    let body = serde_json::json!({"name": format!("{owner}/{name}"), "tags": page});
    let mut r = axum::Json(body).into_response();
    if let Some(last) = truncated {
        let n = q.get("n").cloned().unwrap_or_default();
        if let Ok(v) = header::HeaderValue::from_str(&format!("</v2/{owner}/{name}/tags/list?n={n}&last={last}>; rel=\"next\"")) {
            r.headers_mut().insert(header::LINK, v);
        }
    }
    r
}

#[cfg(test)]
mod declared_size_tests {
    /// Two near-u64::MAX layer sizes must saturate, not panic or wrap.
    #[test]
    fn declared_size_saturates_on_overflow() {
        let manifest = serde_json::json!({
            "config": {"size": 10u64},
            "layers": [{"size": u64::MAX - 1}, {"size": u64::MAX - 1}],
        });
        assert_eq!(super::declared_size(&serde_json::to_vec(&manifest).unwrap()), u64::MAX);
    }
}

#[cfg(test)]
mod declared_digest_tests {
    use super::declared_digests_parse;

    #[test]
    fn a_bad_layer_digest_is_refused_but_annotations_are_not_digests() {
        let good = serde_json::json!({
            "config": {"digest": format!("sha256:{}", "a".repeat(64))},
            "layers": [{"digest": format!("sha256:{}", "b".repeat(64))}],
            "annotations": {"digest": "not-a-digest"},
        });
        assert!(declared_digests_parse(&good));
        let bad = serde_json::json!({"layers": [{"digest": "sha256:XYZ"}]});
        assert!(!declared_digests_parse(&bad));
        let bad_index = serde_json::json!({"manifests": [{"digest": "md5:abc"}]});
        assert!(!declared_digests_parse(&bad_index));
    }
}
