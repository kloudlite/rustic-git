//! `GET/PUT /admin/settings/central` and `/admin/settings/clusters/{region}` — spec §4/§5/§7.
//!
//! Two scopes, two write mechanisms, because they live in two different places: central settings
//! are an object-store document only the SERVER TIER may write (this process forwards a validated
//! patch to its peer route, `PUT /api/admin/settings`, Task 4), while `ClusterSettings` is a CRD
//! this process already writes directly through `kube`, same as every other admin CRD route. Both
//! share one shape: validate ranges, diff the changed fields against a `Mark` table, precheck the
//! affected readers with NOTHING written yet, write, then roll the readers for real — the
//! "409, nothing written" promise from spec §7.

use super::super::workloads::{self, RollReason, Scope};
use super::*;
use kloudlite_core::settings::{
    range_err, validate_stored, Mark, StoredCentralSettings, CENTRAL_SETTINGS_KEY, CENTRAL_SETTING_META,
};
use slatedb::object_store::{path::Path as OsPath, ObjectStoreExt};
use std::collections::BTreeMap;

// ── central: peer client + object-store read ────────────────────────────

/// The one outbound call this admin process makes to the git tier — see the module doc. Kept as
/// its own small type (not just fields on `ApiState`) so `ApiState::with_peer` reads as "wire the
/// one thing", the same shape `with_kube`/`with_aks` already have.
pub struct PeerClient {
    pub client: reqwest::Client,
    /// e.g. `http://kloudlite:8081` — the peer Service, never the public one.
    pub upstream: String,
    pub secret: String,
}

impl PeerClient {
    /// `bins/api` has no `reqwest` dependency of its own (nothing else there makes an outbound
    /// HTTP call) — building the client here keeps that true rather than adding one just for
    /// this one call site.
    #[allow(clippy::expect_used)] // boot-time: built once at process start, never per request
    pub fn new(upstream: String, secret: String) -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .build()
            .expect("building an HTTP client cannot fail with these options"); // boot-time
        Self { client, upstream, secret }
    }
}

fn peer(s: &ApiState) -> Result<&PeerClient, Response> {
    s.peer.as_ref().ok_or_else(|| {
        (StatusCode::SERVICE_UNAVAILABLE, "no peer route to the server tier configured").into_response()
    })
}

fn object_store(s: &ApiState) -> Result<std::sync::Arc<dyn slatedb::object_store::ObjectStore>, Response> {
    s.keys
        .as_ref()
        .map(|store| store.os.clone())
        .ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, "object store not configured").into_response())
}

fn internal(e: impl std::fmt::Display) -> Response {
    tracing::error!(reason = "cluster-settings", error = %e, "request.failed");
    (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
}

/// The current document, or the all-default one if the key has never been written — same
/// contract as the server tier's own `current()` (`bins/server/src/router/admin_settings.rs`),
/// read here directly rather than round-tripped, because a GET needs no peer secret or claim
/// beyond what `refuse_without_claim` already checked (matching `_catalog`'s "readable anywhere").
async fn current_central(s: &ApiState) -> Result<StoredCentralSettings, Response> {
    let os = object_store(s)?;
    let key = OsPath::from(CENTRAL_SETTINGS_KEY);
    match os.get(&key).await {
        Ok(r) => {
            let bytes = r.bytes().await.map_err(internal)?;
            serde_json::from_slice(&bytes).map_err(internal)
        }
        Err(slatedb::object_store::Error::NotFound { .. }) => Ok(StoredCentralSettings::default()),
        Err(e) => Err(internal(e)),
    }
}

pub(crate) async fn get_central(State(s): State<Arc<ApiState>>) -> Result<Response, Response> {
    Ok(Json(current_central(&s).await?).into_response())
}

/// Which readers a changed CENTRAL boot field rolls. Both arms are dead today — `CENTRAL_SETTING_META`
/// marks every field `Mark::Live` (Task 4), and `logFormat`/`workerLanes` are not even fields on
/// `CentralSettings` (skipped per Task 4's ruling) — so `boot_fields` below never finds a match to
/// dispatch through this. Kept anyway: the table-driven diff is what makes marking a future field
/// `Mark::Boot` "flip one entry", not "write this whole path from scratch".
// ponytail: unreachable under today's meta table; the shape is what Task 6 was asked to build,
// the wiring for an actual boot central field is for whoever adds the first one.
async fn central_boot_readers(s: &ApiState, field: &str) -> Result<Vec<(Scope, &'static str)>, Response> {
    match field {
        "sshHost" | "sshPort" => {
            let regions = super::active_regions(s).await?;
            Ok(regions.into_iter().map(|r| (Scope::Region(r), "kloudlite-gateway")).collect())
        }
        "logFormat" | "workerLanes" => {
            Ok(workloads::KNOWN_CENTRAL.iter().map(|(name, _)| (Scope::Central, *name)).collect())
        }
        _ => Ok(vec![]),
    }
}

/// Which changed fields (patch differs from current, and is `Some`) are `Mark::Boot`.
/// `restore`: the patch is a whole snapshot, so a `None` there is a change too (back to default).
fn central_boot_fields(current: &StoredCentralSettings, patch: &StoredCentralSettings, restore: bool) -> Vec<&'static str> {
    let is_boot = |name: &str| CENTRAL_SETTING_META.iter().any(|(n, m)| *n == name && *m == Mark::Boot);
    let mut out = Vec::new();
    if is_boot("sshHost") && (restore || patch.ssh_host.is_some()) && patch.ssh_host != current.ssh_host {
        out.push("sshHost");
    }
    if is_boot("sshPort") && (restore || patch.ssh_port.is_some()) && patch.ssh_port != current.ssh_port {
        out.push("sshPort");
    }
    out
}

/// `PUT /admin/settings/central`. Steps 1-4 of the brief, in order: validate, precheck the boot
/// fields' readers with nothing written, forward to the server tier, roll for real.
pub(crate) async fn put_central(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Result<Response, Response> {
    let c = caller(&s, &headers).await?;
    // The note rides ALONGSIDE the fields rather than wrapping them: `StoredCentralSettings` sets
    // no `deny_unknown_fields`, so an extra key is ignored on the way in and the body stays the
    // same flat shape the server tier's own route already forwards.
    let note = super::require_note(body.get("note").and_then(|v| v.as_str()).unwrap_or_default())?;
    let patch: StoredCentralSettings =
        serde_json::from_value(body).map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e.to_string()).into_response())?;
    if let Err(msg) = validate_stored(&patch) {
        return Err((StatusCode::UNPROCESSABLE_ENTITY, msg).into_response());
    }
    let current = current_central(&s).await?;
    let changed_boot = central_boot_fields(&current, &patch, false);
    // Step 2: precheck every affected reader, across every scope it lives in — nothing is
    // forwarded to the server tier (so `cluster/settings` stays untouched) until every one of
    // them is settled.
    let mut roll_targets: Vec<(&'static str, Scope, &'static str)> = Vec::new();
    for field in &changed_boot {
        for (scope, reader) in central_boot_readers(&s, field).await? {
            // A reader still mid-roll refuses the save; the refusal is a decision the log must
            // carry, the same as the cluster-scope precheck.
            super::audited(
                &s,
                &c.name,
                "put-central-settings",
                "central",
                Some(note.clone()),
                workloads::precheck_readers(&s, &scope, &[reader]).await,
            )
            .await?;
            roll_targets.push((field, scope, reader));
        }
    }
    // Step 3: forward the validated body to the server tier's peer route. The peer secret proves
    // this IS the admin server; the caller's own bearer token (already proven superadmin by
    // `refuse_without_claim`) is forwarded so the server tier's own `require_superadmin` re-checks
    // it independently — belt and braces, matching `admin_settings.rs`'s own doc comment.
    let peer = peer(&s)?;
    let Some(token) = kloudlite_core::httpx::bearer_token(&headers) else {
        return Err((StatusCode::UNAUTHORIZED, "bearer token required").into_response());
    };
    let resp = peer
        .client
        .put(format!("{}/api/admin/settings", peer.upstream))
        .header(kloudlite_core::peer::PEER_HEADER, &peer.secret)
        .bearer_auth(token)
        .json(&patch)
        .send()
        .await
        // Never format `peer.secret`/the argv into this — matches `merge_worker.rs`'s
        // `local()`/`networked()` split for the same reason.
        .map_err(|e| {
            tracing::error!(scope = "central", reason = "write", error = %e, "settings.forward.failed");
            (StatusCode::BAD_GATEWAY, "could not reach the server tier").into_response()
        })?;
    let status = resp.status();
    let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
    if !status.is_success() {
        // Propagate the server tier's own refusal verbatim (its own 422 range message included) —
        // this route added no write of its own to roll back.
        return Ok((axum::http::StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY), Json(body))
            .into_response());
    }
    // Step 4: roll for real. A failure here is a transient API error on an already-committed
    // settings write — never rolled back, since the settings document (now written on the server
    // tier) is the source of truth per the Global Constraints — but it IS carried into the audit
    // result rather than dropped.
    // ponytail: step-2/step-4 TOCTOU on a reader starting its own manual roll mid-save; a global
    // "settings write" mutex in the admin process closes it if it's ever hit in practice, not
    // built ahead of evidence it happens.
    let targets = roll_targets.into_iter().map(|(f, sc, r)| (f, sc, vec![r])).collect();
    let result = roll_all(&s, "put-central-settings", &c.name, targets).await;
    super::audit(&s, &c.name, "put-central-settings", "central", Some(note), result).await;
    Ok(Json(body).into_response())
}

/// Roll every boot reader the write changed, and fold the outcome into the audit result.
///
/// The roll cannot be undone — the settings write has already landed, and the precheck above
/// already refused a reader that was mid-roll — but answering a plain "ok" hid the one fact an
/// operator would act on: a `Mark::Boot` reader still running the old value. Both the log line
/// and the audit row now name the fields whose roll failed (2026-09-12).
async fn roll_all(
    s: &ApiState,
    action: &str,
    by: &str,
    targets: Vec<(&'static str, Scope, Vec<&str>)>,
) -> String {
    let mut failed: Vec<&'static str> = Vec::new();
    for (field, scope, readers) in targets {
        if let Err(resp) = workloads::roll_readers(s, &scope, &readers, RollReason::Setting(field), by).await {
            tracing::error!(action, field, status = resp.status().as_u16(), "settings.roll.failed");
            failed.push(field);
        }
    }
    match failed.is_empty() {
        true => "ok".to_string(),
        false => format!("ok; roll failed: {}", failed.join(",")),
    }
}

/// `POST /admin/settings/central/revert`, the central twin of `revert_cluster` below. Unlike the
/// cluster route this names no index — `history[0]` is always the target ("undo the last write"),
/// since the central document has no per-region caller to disambiguate. Same shape as
/// `put_central`: precheck boot readers against the target values with nothing written yet,
/// forward to the server tier's own revert route (which does the actual swap-and-push-history),
/// then roll for real.
pub(crate) async fn revert_central(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Json(body): Json<super::NoteBody>,
) -> Result<Response, Response> {
    let c = caller(&s, &headers).await?;
    let note = super::require_note(&body.note)?;
    let current = current_central(&s).await?;
    let Some(snap) = current.history.first() else {
        return Err((StatusCode::UNPROCESSABLE_ENTITY, "no history to revert to").into_response());
    };
    let target: StoredCentralSettings = snap.into();
    let changed_boot = central_boot_fields(&current, &target, true);
    let mut roll_targets: Vec<(&'static str, Scope, &'static str)> = Vec::new();
    for field in &changed_boot {
        for (scope, reader) in central_boot_readers(&s, field).await? {
            // A reader still mid-roll refuses the save; the refusal is a decision the log must
            // carry, the same as the cluster-scope precheck.
            super::audited(
                &s,
                &c.name,
                "revert-central-settings",
                "central",
                Some(note.clone()),
                workloads::precheck_readers(&s, &scope, &[reader]).await,
            )
            .await?;
            roll_targets.push((field, scope, reader));
        }
    }
    let peer = peer(&s)?;
    let Some(token) = kloudlite_core::httpx::bearer_token(&headers) else {
        return Err((StatusCode::UNAUTHORIZED, "bearer token required").into_response());
    };
    let resp = peer
        .client
        .post(format!("{}/api/admin/settings/revert", peer.upstream))
        .header(kloudlite_core::peer::PEER_HEADER, &peer.secret)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| {
            tracing::error!(scope = "central", reason = "revert", error = %e, "settings.forward.failed");
            (StatusCode::BAD_GATEWAY, "could not reach the server tier").into_response()
        })?;
    let status = resp.status();
    let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
    if !status.is_success() {
        return Ok((axum::http::StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY), Json(body))
            .into_response());
    }
    let targets = roll_targets.into_iter().map(|(f, sc, r)| (f, sc, vec![r])).collect();
    let result = roll_all(&s, "revert-central-settings", &c.name, targets).await;
    super::audit(&s, &c.name, "revert-central-settings", "central", Some(note), result).await;
    Ok(Json(body).into_response())
}

// ── cluster: direct CRD write ────────────────────────────────────────────

/// Same range-error sentence `validate_stored` uses (`core::settings::range_err`), so the two
/// settings scopes' 422s read identically — the "identical validator" the brief asks for is this
/// shared formatter plus the same `stored ?? env ?? default` field shape, not a second copy of
/// `CentralSettings`' macro against a type `core` cannot know about.
pub(crate) fn validate_cluster_patch(patch: &crd::ClusterSettingsSpec) -> Result<(), String> {
    macro_rules! range {
        ($f:ident, $lo:expr, $hi:expr) => {
            if let Some(v) = patch.$f {
                if !($lo..=$hi).contains(&v) {
                    return Err(range_err(stringify!($f), $lo, $hi, v));
                }
            }
        };
    }
    range!(sync_secs, 10u64, 3600u64);
    range!(replica_secs, 30u64, 3600u64);
    range!(decommission_secs, 5u64, 600u64);
    range!(node_dead_secs, 60u64, 3600u64);
    range!(peer_send_timeout_secs, 60u64, 21600u64);
    range!(peer_serve_timeout_secs, 60u64, 21600u64);
    range!(peer_receive_slack, 0u64, 60u64);
    range!(nix_timeout_secs, 60u64, 7200u64);
    range!(default_replicas, 1u32, 5u32);
    range!(quota_gb_ceiling, 10u32, 5000u32);
    range!(trees_per_workspace, 1u32, 32u32);
    range!(trace_sample_ratio, 0.0f64, 1.0f64);
    range!(trace_probe_rate, 0.0f64, 1000.0f64);
    range!(trace_probe_burst, 1.0f64, 10000.0f64);
    range!(trace_promote_rate, 0.0f64, 1000.0f64);
    range!(trace_promote_burst, 1.0f64, 10000.0f64);
    // Unbounded pin string (constraints.md's exact carve-out): only non-emptiness is checked,
    // and only when the admin actually set it.
    if let Some(v) = &patch.nixpkgs {
        if v.trim().is_empty() {
            return Err("nixpkgs must not be empty when set".to_string());
        }
    }
    Ok(())
}

/// Merge `patch` onto `current`, field by field, only where the admin actually set something —
/// the same `Option` override shape `CentralSettings::merged_with`/`AgentSettings::merged_with`
/// already use, just against the CRD spec type directly since `ClusterSettingsSpec` already IS
/// the `Option`-shaped "what an admin has touched" document (there is no separate stored/live
/// split for this scope the way central has one).
fn merge_cluster_spec(mut current: crd::ClusterSettingsSpec, patch: &crd::ClusterSettingsSpec) -> crd::ClusterSettingsSpec {
    macro_rules! over {
        ($f:ident) => {
            if patch.$f.is_some() {
                current.$f = patch.$f.clone();
            }
        };
    }
    over!(sync_secs);
    over!(bench_idle_secs);
    over!(kompress_url);
    over!(trace_sample_ratio);
    over!(trace_probe_rate);
    over!(trace_probe_burst);
    over!(trace_promote_rate);
    over!(trace_promote_burst);
    over!(replica_secs);
    over!(decommission_secs);
    over!(node_dead_secs);
    over!(peer_send_timeout_secs);
    over!(peer_serve_timeout_secs);
    over!(peer_receive_slack);
    over!(nix_timeout_secs);
    over!(nixpkgs);
    over!(base_packages);
    over!(default_replicas);
    over!(quota_gb_ceiling);
    over!(trees_per_workspace);
    over!(default_image);
    over!(intercept_proxy_image);
    over!(git_init_image);
    over!(runtime_class);
    over!(stall_dumps);
    over!(member_removal_deletes);
    current
}

/// Changed AND `Mark::Boot`, paired with the readers `CLUSTER_SETTING_META` already names for
/// that field — unlike the central table, this one carries its readers inline (Task 1), so there
/// is no separate "which readers" dispatch to hand-maintain here.
/// `restore`: the patch is a whole snapshot, so a `None` there is a change too (back to default).
fn changed_cluster_boot_fields(
    current: &crd::ClusterSettingsSpec,
    patch: &crd::ClusterSettingsSpec,
    restore: bool,
) -> Vec<(&'static str, &'static [&'static str])> {
    let mut out = Vec::new();
    macro_rules! chk {
        ($f:ident, $wire:literal) => {
            if (restore || patch.$f.is_some()) && patch.$f != current.$f {
                if let Some((_, mark, readers)) = crd::CLUSTER_SETTING_META.iter().find(|(n, _, _)| *n == $wire) {
                    if *mark == Mark::Boot {
                        out.push(($wire, *readers));
                    }
                }
            }
        };
    }
    chk!(default_image, "defaultImage");
    chk!(intercept_proxy_image, "interceptProxyImage");
    chk!(git_init_image, "gitInitImage");
    chk!(runtime_class, "runtimeClass");
    out
}

fn history_from_annotations(ann: &BTreeMap<String, String>) -> Vec<crd::ClusterSettingsSpec> {
    ann.get(crd::SETTINGS_HISTORY_ANNOTATION)
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default()
}

/// Pushes `old` onto the front of the history annotation, capped at ten — the CR-annotation twin
/// of `core::settings::push_history`'s inline-array version for the central document.
fn push_cluster_history(old: &crd::ClusterSettingsSpec, ann: &mut BTreeMap<String, String>) {
    let mut hist = history_from_annotations(ann);
    hist.insert(0, old.clone());
    hist.truncate(10);
    if let Ok(s) = serde_json::to_string(&hist) {
        ann.insert(crd::SETTINGS_HISTORY_ANNOTATION.to_string(), s);
    }
}

fn default_cluster_settings() -> crd::ClusterSettings {
    crd::ClusterSettings::new("default", crd::ClusterSettingsSpec::default())
}

pub(crate) async fn get_cluster(State(s): State<Arc<ApiState>>, Path(region): Path<String>) -> Result<Response, Response> {
    let client = super::client_for_region(&s, &region).await?;
    let api: Api<crd::ClusterSettings> = Api::all(client.clone());
    let cs = api.get_opt("default").await.map_err(kube_err)?.unwrap_or_else(default_cluster_settings);
    Ok(Json(cs).into_response())
}

/// The shared body of `put_cluster` and `revert_cluster`: validate is the CALLER's job (a revert
/// replays a snapshot that was already valid when it was captured), everything from the boot diff
/// onward is identical either way.
struct ClusterWrite<'a> {
    caller_name: &'a str,
    action: &'static str,
    note: String,
    region: &'a str,
    /// A revert: the patch REPLACES the spec (a field the snapshot left unset returns to
    /// `env ?? default`) instead of merging onto it.
    restore: bool,
}

/// The spec and annotations a write lands: `current` onto history, stamped, then `patch` merged
/// (a PUT) or taken whole (a revert).
fn cluster_write(
    current: &crd::ClusterSettings,
    patch: &crd::ClusterSettingsSpec,
    restore: bool,
    by: &str,
    at: &str,
) -> crd::ClusterSettings {
    let mut ann = current.metadata.annotations.clone().unwrap_or_default();
    push_cluster_history(&current.spec, &mut ann);
    ann.insert(crd::SETTINGS_UPDATED_BY_ANNOTATION.to_string(), by.to_string());
    ann.insert(crd::SETTINGS_UPDATED_AT_ANNOTATION.to_string(), at.to_string());
    let spec = if restore { patch.clone() } else { merge_cluster_spec(current.spec.clone(), patch) };
    let mut apply = crd::ClusterSettings::new("default", spec);
    apply.metadata.annotations = Some(ann);
    apply
}

async fn apply_cluster_patch(
    s: &ApiState,
    w: ClusterWrite<'_>,
    client: kube::Client,
    current: crd::ClusterSettings,
    patch: crd::ClusterSettingsSpec,
) -> Result<Response, Response> {
    let ClusterWrite { caller_name, action, note, region, restore } = w;
    let api: Api<crd::ClusterSettings> = Api::all(client);
    let changed = changed_cluster_boot_fields(&current.spec, &patch, restore);
    if !changed.is_empty() {
        let mut readers: Vec<&str> = changed.iter().flat_map(|(_, rs)| rs.iter().copied()).collect();
        readers.sort_unstable();
        readers.dedup();
        // Step: precheck every affected reader — nothing below is written until every one is
        // settled (spec §7's "the CR is not touched").
        super::audited(
            s,
            caller_name,
            action,
            region,
            Some(note.clone()),
            workloads::precheck_readers(s, &Scope::Region(region.to_string()), &readers).await,
        )
        .await?;
    }
    let apply = cluster_write(&current, &patch, restore, caller_name, &chrono::Utc::now().to_rfc3339());
    let patched = super::audited(
        s,
        caller_name,
        action,
        region,
        Some(note.clone()),
        api.patch("default", &PatchParams::apply(crd::AGENT_FIELD_MANAGER_ADMIN).force(), &Patch::Apply(&apply))
            .await
            .map_err(kube_err),
    )
    .await?;
    // Same reasoning as the central path above — the precheck already refused a conflicting
    // roll, the CR write already landed, and the CR is the source of truth — but a failed roll
    // is named in the audit result instead of being swallowed.
    let targets = changed
        .into_iter()
        .map(|(field, readers)| (field, Scope::Region(region.to_string()), readers.to_vec()))
        .collect();
    let result = roll_all(s, action, caller_name, targets).await;
    super::audit(s, caller_name, action, region, Some(note), result).await;
    Ok(Json(patched).into_response())
}

pub(crate) async fn put_cluster(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(region): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Result<Response, Response> {
    let c = caller(&s, &headers).await?;
    // Same flat shape as `put_central`, for the same reason: `ClusterSettingsSpec` ignores the
    // extra key, so no wrapper type has to exist on either side of the wire.
    let note = super::require_note(body.get("note").and_then(|v| v.as_str()).unwrap_or_default())?;
    let patch: crd::ClusterSettingsSpec =
        serde_json::from_value(body).map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e.to_string()).into_response())?;
    if let Err(msg) = validate_cluster_patch(&patch) {
        return Err((StatusCode::UNPROCESSABLE_ENTITY, msg).into_response());
    }
    let client = super::client_for_region(&s, &region).await?.clone();
    let api: Api<crd::ClusterSettings> = Api::all(client.clone());
    let current = api.get_opt("default").await.map_err(kube_err)?.unwrap_or_else(default_cluster_settings);
    apply_cluster_patch(
        &s,
        ClusterWrite { caller_name: &c.name, action: "put-cluster-settings", note, region: &region, restore: false },
        client,
        current,
        patch,
    )
    .await
}

pub(crate) async fn revert_cluster(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path((region, n)): Path<(String, usize)>,
    Json(body): Json<super::NoteBody>,
) -> Result<Response, Response> {
    let c = caller(&s, &headers).await?;
    let note = super::require_note(&body.note)?;
    let client = super::client_for_region(&s, &region).await?.clone();
    let api: Api<crd::ClusterSettings> = Api::all(client.clone());
    let current = api.get_opt("default").await.map_err(kube_err)?.ok_or_else(not_found)?;
    let ann = current.metadata.annotations.clone().unwrap_or_default();
    let hist = history_from_annotations(&ann);
    let target = hist.get(n).cloned().ok_or_else(not_found)?;
    // A snapshot from before a range tightened must not slip past it on the way back.
    if let Err(msg) = validate_cluster_patch(&target) {
        return Err((StatusCode::UNPROCESSABLE_ENTITY, msg).into_response());
    }
    apply_cluster_patch(
        &s,
        ClusterWrite { caller_name: &c.name, action: "revert-cluster-settings", note, region: &region, restore: true },
        client,
        current,
        target,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Generic round trip over every `ClusterSettingsSpec` field: a field missing from
    /// `merge_cluster_spec`'s `over!` list (like `bench_idle_secs` was) is silently dropped by a
    /// PUT even though it passed validation and got recorded in history — this fails loudly
    /// instead. Built via JSON so a field added to the struct is picked up automatically rather
    /// than needing a new assertion here.
    #[test]
    fn merge_cluster_spec_carries_every_field() {
        // The field list comes from `CLUSTER_SETTING_META` — already held exhaustive against the
        // struct by `cluster_setting_meta_is_exhaustive` — not `ClusterSettingsSpec::default()`,
        // which serializes to `{}` (every field `None` and `skip_serializing_if`d).
        //
        // Both `base` and `patch` set every field (distinct values) so a revert exercises a real
        // history-snapshot shape — `merge_cluster_spec` only overrides a field when the patch
        // side is `Some`, so a `None` patch (an admin-never-set field) leaves it untouched on
        // purpose, and a base of all-`None` would make a real bug in the `over!` list look like
        // a pass.
        let field_json = |tag: u64| -> serde_json::Map<String, serde_json::Value> {
            crd::CLUSTER_SETTING_META
                .iter()
                .map(|(wire, _, _)| {
                    let v = match *wire {
                        n if n.starts_with("trace") => serde_json::json!(0.125 * tag as f64),
                        "nixpkgs" | "basePackages" | "defaultImage" | "interceptProxyImage" | "gitInitImage" | "runtimeClass" => {
                            serde_json::json!(format!("test-{wire}-{tag}"))
                        }
                        "stallDumps" | "memberRemovalDeletes" => serde_json::json!(tag.is_multiple_of(2)),
                        _ => serde_json::json!(1000 + tag),
                    };
                    (wire.to_string(), v)
                })
                .collect()
        };
        let base_obj = field_json(1);
        let patch_obj = field_json(2);
        let base: crd::ClusterSettingsSpec = serde_json::from_value(serde_json::Value::Object(base_obj.clone())).unwrap();
        let patch: crd::ClusterSettingsSpec = serde_json::from_value(serde_json::Value::Object(patch_obj.clone())).unwrap();

        let merged = merge_cluster_spec(base.clone(), &patch);
        let merged_json = serde_json::to_value(&merged).unwrap();
        for (key, expected) in &patch_obj {
            assert_eq!(
                merged_json.get(key),
                Some(expected),
                "field {key} did not survive merge_cluster_spec — add it to the `over!` list"
            );
        }

        // A revert re-applies the pre-change snapshot as the patch; every field must go back.
        let reverted = merge_cluster_spec(merged, &base);
        let reverted_json = serde_json::to_value(&reverted).unwrap();
        for (key, expected) in &base_obj {
            assert_eq!(
                reverted_json.get(key),
                Some(expected),
                "field {key} did not revert via merge_cluster_spec — add it to the `over!` list"
            );
        }
    }
}

#[cfg(test)]
mod cluster_tests {
    use super::*;

    /// Every `ClusterSettingsSpec` field survives a write and a revert: a field added to the
    /// struct but missed in `merge_cluster_spec` fails here. Keys are `CLUSTER_SETTING_META`'s, so
    /// the table being exhaustive (`cluster_setting_meta_is_exhaustive`) makes this one too.
    #[test]
    fn every_cluster_field_round_trips_through_write_and_revert() {
        let full = |bump: bool| -> crd::ClusterSettingsSpec {
            let v: serde_json::Map<_, _> = crd::CLUSTER_SETTING_META
                .iter()
                .map(|(name, _, _)| {
                    let x = match *name {
                        n if n.starts_with("trace") => serde_json::json!(if bump { 0.25 } else { 0.5 }),
                        "nixpkgs" | "basePackages" | "defaultImage" | "interceptProxyImage" | "gitInitImage" | "runtimeClass" => serde_json::json!(if bump { "b" } else { "a" }),
                        "stallDumps" | "memberRemovalDeletes" => serde_json::json!(bump),
                        _ => serde_json::json!(if bump { 61 } else { 62 }),
                    };
                    (name.to_string(), x)
                })
                .collect();
            serde_json::from_value(serde_json::Value::Object(v)).unwrap()
        };
        let wire = |s: &crd::ClusterSettingsSpec| serde_json::to_value(s).unwrap();
        let (base, next) = (full(false), full(true));
        assert_eq!(wire(&next).as_object().unwrap().len(), crd::CLUSTER_SETTING_META.len(), "a field did not parse");
        let changed = merge_cluster_spec(base.clone(), &next);
        assert_eq!(wire(&changed), wire(&next), "the write dropped a field");
        let mut ann = BTreeMap::new();
        push_cluster_history(&base, &mut ann);
        let reverted = merge_cluster_spec(changed, &history_from_annotations(&ann)[0]);
        assert_eq!(wire(&reverted), wire(&base), "history/revert dropped a field");
        // The real revert path: every field of the snapshot back, a newer one cleared, and a new entry.
        let cur = crd::ClusterSettings::new("default", next.clone());
        let written = cluster_write(&cur, &base, true, "a", "t");
        assert_eq!(wire(&written.spec), wire(&base), "revert did not restore the snapshot");
        let empty = cluster_write(&cur, &crd::ClusterSettingsSpec::default(), true, "a", "t");
        assert_eq!(wire(&empty.spec), serde_json::json!({}), "revert kept a field the snapshot never set");
        let hist = history_from_annotations(written.metadata.annotations.as_ref().unwrap());
        assert_eq!(hist.len(), 1);
        assert_eq!(wire(&hist[0]), wire(&next), "the pre-revert spec is a new history entry");
    }

    /// A revert to an entry predating `traceSampleRatio` resets it, and a boot field the snapshot
    /// never set still counts as changed so its readers are prechecked and rolled.
    #[test]
    fn revert_resets_newer_fields_and_prechecks_boot_readers() {
        let old = crd::ClusterSettingsSpec { sync_secs: Some(60), ..Default::default() };
        let cur = crd::ClusterSettingsSpec {
            sync_secs: Some(90),
            trace_sample_ratio: Some(0.9),
            stall_dumps: Some(true),
            default_image: Some("img:new".into()),
            ..Default::default()
        };
        let written = cluster_write(&crd::ClusterSettings::new("default", cur.clone()), &old, true, "a", "t");
        assert_eq!((written.spec.sync_secs, written.spec.trace_sample_ratio), (Some(60), None));
        // Unset resolves to the default, so a pre-stallDumps snapshot turns dumps back off.
        assert_eq!(written.spec.stall_dumps, None);
        let boot: Vec<_> = changed_cluster_boot_fields(&cur, &old, true).into_iter().map(|(f, _)| f).collect();
        assert_eq!(boot, vec!["defaultImage"]);
        assert!(changed_cluster_boot_fields(&cur, &old, false).is_empty(), "a PUT leaves an unset field alone");
    }

    #[test]
    fn trace_fields_are_range_checked() {
        let with = |r, rate, burst| crd::ClusterSettingsSpec { trace_sample_ratio: r, trace_probe_rate: rate, trace_probe_burst: burst, ..Default::default() };
        assert!(validate_cluster_patch(&with(Some(1.5), None, None)).unwrap_err().starts_with("trace_sample_ratio must be between 0 and 1"));
        assert!(validate_cluster_patch(&with(Some(-0.1), None, None)).is_err());
        assert!(validate_cluster_patch(&with(None, Some(1001.0), None)).is_err());
        assert!(validate_cluster_patch(&with(None, None, Some(0.0))).is_err());
        assert!(validate_cluster_patch(&with(Some(0.0), Some(0.0), Some(1.0))).is_ok());
    }
}
