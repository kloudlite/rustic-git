//! The workload roll primitive spec §7 needs: a fixed list of what may ever be rolled
//! (`KNOWN_CENTRAL`/`KNOWN_PER_REGION`), the "patch the restart annotation, only if the previous
//! roll has settled" write (`roll_readers`), and the read side (`list_workloads`) both `GET
//! /admin/workloads` and Task 8's `GET /admin/infra` share, so the two views never disagree about
//! what a Deployment's ready/desired even means.
//!
//! Two clusters, one call site: central workloads (`Scope::Central`) live on the SAME AKS cluster
//! this admin process runs on (`ApiState::aks`, in-cluster config — no kubeconfig to rotate);
//! per-region workloads (`Scope::Region`) live on that region's k3s (`ApiState::kube`, the same
//! client every CRD route already uses). `client_for` is the one place that decides which.
//!
//! ponytail: `Scope::Region` does not yet select a client PER region — there is only one region
//! `kube::Client` wired today (`ApiState::kube`), so every region resolves to it. Upgrade path:
//! once a region→client map exists (Task 6's `ClusterSettings` work), thread it through here
//! instead of always answering with `s.kube`.

use super::{aks, kube, not_found, ApiState};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use k8s_openapi::api::apps::v1::{DaemonSet, Deployment, StatefulSet};
use kube::api::{Api, Patch, PatchParams};

/// A roll only ever patches an existing pod-template annotation — never a pod delete, never a
/// scale. This is what `kubectl rollout restart` sends.
const RESTARTED_AT: &str = "rustic-git.io/restarted-at";
const ROLLED_BY: &str = "rustic-git.io/rolled-by";
const ROLLED_AT: &str = "rustic-git.io/rolled-at";
const ROLL_REASON: &str = "rustic-git.io/roll-reason";

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    StatefulSet,
    Deployment,
    DaemonSet,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Scope {
    Central,
    Region(String),
}

impl std::fmt::Display for Scope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Scope::Central => write!(f, "central"),
            Scope::Region(r) => write!(f, "region/{r}"),
        }
    }
}

// A plain string over the wire — "central" or the bare region id — not serde's default
// externally-tagged shape (`{"Central":null}` / `{"Region":"x"}`, and a unit variant next to a
// tuple variant is what makes the derive panic). `Display` uses "region/{r}" for tracing
// disambiguation; the JSON form matches `parse_scope` (`api/admin.rs`), which is the only reader.
impl serde::Serialize for Scope {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Scope::Central => s.serialize_str("central"),
            Scope::Region(r) => s.serialize_str(r),
        }
    }
}

/// The fixed list — never a free string. A roll target not named here is a 404, both from the
/// settings-save path (Task 6) and the manual route below.
pub const KNOWN_CENTRAL: &[(&str, Kind)] = &[
    ("rustic-git-srv", Kind::StatefulSet),
    ("rustic-git-api", Kind::Deployment),
    ("rustic-git-worker", Kind::Deployment),
    ("rustic-git-web", Kind::Deployment),
    ("rustic-git-admin", Kind::Deployment),
];

/// Per region: resolved against that region's client, same as `ClusterSettings`. `rustic-git-gateway`
/// is exclusively a per-region k3s Deployment (Step 1) — there is no central gateway entry.
pub const KNOWN_PER_REGION: &[(&str, Kind)] = &[
    ("rustic-git-agent", Kind::DaemonSet),
    ("rustic-git-gateway", Kind::Deployment),
];

/// `spec.template` lives under a different namespace per name, not per cluster — the agent's
/// DaemonSet is cluster-infra (`kube-system`), the gateway is its own namespace.
fn namespace(scope: &Scope, name: &str) -> &'static str {
    match scope {
        Scope::Central => "rustic-git",
        Scope::Region(_) if name == "rustic-git-agent" => "kube-system",
        Scope::Region(_) => "rustic-git-system",
    }
}

fn resolve(scope: &Scope, name: &str) -> Option<Kind> {
    let table = match scope {
        Scope::Central => KNOWN_CENTRAL,
        Scope::Region(_) => KNOWN_PER_REGION,
    };
    table.iter().find(|(n, _)| *n == name).map(|(_, k)| *k)
}

/// `Scope::Region` is resolved through `client_for_region`, which 404s a region name that names
/// no active `crd::Region` — `scope` is the one path segment `{scope}/{name}/roll` and the
/// settings routes let a caller type, so nothing else stands between an arbitrary string and a
/// PATCH against whatever `kube(s)` happens to point at today (review finding on Task 5).
async fn client_for<'a>(s: &'a ApiState, scope: &Scope) -> Result<&'a kube::Client, Response> {
    match scope {
        Scope::Central => aks(s),
        Scope::Region(region) => super::client_for_region(s, region).await,
    }
}

/// The one thing every kind exposes, read out of whichever typed object it actually is — kept
/// small and by-value rather than keeping the typed object around, since every caller (list, roll)
/// only ever wants these four things.
struct Info {
    ready: i64,
    desired: i64,
    image: Option<String>,
    last_roll: Option<LastRoll>,
}

#[derive(Clone, serde::Serialize)]
pub struct LastRoll {
    pub by: String,
    pub at: String,
    pub reason: String,
}

fn last_roll_of(annotations: &std::collections::BTreeMap<String, String>) -> Option<LastRoll> {
    Some(LastRoll {
        by: annotations.get(ROLLED_BY)?.clone(),
        at: annotations.get(ROLLED_AT)?.clone(),
        reason: annotations.get(ROLL_REASON)?.clone(),
    })
}

fn empty_map() -> std::collections::BTreeMap<String, String> {
    Default::default()
}

async fn fetch(client: &kube::Client, ns: &str, kind: Kind, name: &str) -> Result<Info, kube::Error> {
    match kind {
        Kind::Deployment => {
            let o = Api::<Deployment>::namespaced(client.clone(), ns).get(name).await?;
            let st = o.status.unwrap_or_default();
            let tmpl = o.spec.as_ref().map(|s| &s.template);
            Ok(Info {
                ready: st.ready_replicas.unwrap_or(0) as i64,
                desired: o.spec.as_ref().and_then(|s| s.replicas).unwrap_or(1) as i64,
                image: tmpl.and_then(|t| t.spec.as_ref()).and_then(|p| p.containers.first()).and_then(|c| c.image.clone()),
                last_roll: last_roll_of(
                    &tmpl.and_then(|t| t.metadata.as_ref()).and_then(|m| m.annotations.clone()).unwrap_or_else(empty_map),
                ),
            })
        }
        Kind::StatefulSet => {
            let o = Api::<StatefulSet>::namespaced(client.clone(), ns).get(name).await?;
            let st = o.status.unwrap_or_default();
            let spec = o.spec.unwrap_or_default();
            let tmpl = &spec.template;
            Ok(Info {
                ready: st.ready_replicas.unwrap_or(0) as i64,
                desired: spec.replicas.unwrap_or(1) as i64,
                image: tmpl.spec.as_ref().and_then(|p| p.containers.first()).and_then(|c| c.image.clone()),
                last_roll: last_roll_of(
                    &tmpl.metadata.as_ref().and_then(|m| m.annotations.clone()).unwrap_or_else(empty_map),
                ),
            })
        }
        Kind::DaemonSet => {
            let o = Api::<DaemonSet>::namespaced(client.clone(), ns).get(name).await?;
            let st = o.status.unwrap_or_default();
            let tmpl = &o.spec.map(|s| s.template);
            Ok(Info {
                ready: st.number_ready as i64,
                desired: st.desired_number_scheduled as i64,
                image: tmpl.as_ref().and_then(|t| t.spec.as_ref()).and_then(|p| p.containers.first()).and_then(|c| c.image.clone()),
                last_roll: last_roll_of(
                    &tmpl.as_ref().and_then(|t| t.metadata.as_ref()).and_then(|m| m.annotations.clone()).unwrap_or_else(empty_map),
                ),
            })
        }
    }
}

/// The merge patch every roll sends — the restart trigger plus the audit trail, on the SAME
/// patch, so the two are never observed out of sync with each other.
fn roll_patch(ts: &str, by: &str, reason: &str) -> serde_json::Value {
    serde_json::json!({"spec": {"template": {"metadata": {"annotations": {
        RESTARTED_AT: ts,
        ROLLED_BY: by,
        ROLLED_AT: ts,
        ROLL_REASON: reason,
    }}}}})
}

async fn apply_patch(client: &kube::Client, ns: &str, kind: Kind, name: &str, patch: &serde_json::Value) -> Result<(), kube::Error> {
    let pp = PatchParams::default();
    match kind {
        Kind::Deployment => {
            Api::<Deployment>::namespaced(client.clone(), ns).patch(name, &pp, &Patch::Merge(patch)).await?;
        }
        Kind::StatefulSet => {
            Api::<StatefulSet>::namespaced(client.clone(), ns).patch(name, &pp, &Patch::Merge(patch)).await?;
        }
        Kind::DaemonSet => {
            Api::<DaemonSet>::namespaced(client.clone(), ns).patch(name, &pp, &Patch::Merge(patch)).await?;
        }
    }
    Ok(())
}

pub enum RollReason {
    // Constructed by the settings-write handlers (`api::admin::settings`) for a changed
    // `Mark::Boot` field.
    Setting(&'static str),
    Manual(String),
}

impl RollReason {
    fn text(&self) -> String {
        match self {
            RollReason::Setting(field) => format!("setting:{field}"),
            RollReason::Manual(reason) => reason.clone(),
        }
    }
}

fn kube_err(e: kube::Error) -> Response {
    super::kube_err(e)
}

fn conflict(name: &str, ready: i64, desired: i64) -> Response {
    (StatusCode::CONFLICT, axum::Json(serde_json::json!({"name": name, "ready": ready, "desired": desired}))).into_response()
}

/// The read-only half of a roll: every named reader in `scope` must have `ready == desired`, or
/// this is a 409 naming the first one that isn't — and nothing is written, by construction, since
/// this function never calls `apply_patch`. Task 6's settings-write handlers call this ALONE, with
/// no patch yet, so a settings document is never persisted ahead of a roll that turns out to
/// conflict; `roll_readers` below calls it again immediately before writing, which is what makes
/// its own "409, nothing written" promise true for the manual-roll route too.
pub async fn precheck_readers(s: &ApiState, scope: &Scope, readers: &[&str]) -> Result<(), Response> {
    let client = client_for(s, scope).await?;
    for &name in readers {
        let kind = resolve(scope, name).ok_or_else(not_found)?;
        let ns = namespace(scope, name);
        let info = fetch(client, ns, kind, name).await.map_err(kube_err)?;
        if info.ready < info.desired {
            return Err(conflict(name, info.ready, info.desired));
        }
    }
    Ok(())
}

/// Roll every named reader within ONE scope, or roll none of them.
///
/// Precheck-then-write is what makes the settings-write path's "409, nothing written" promise
/// (spec §7) true here rather than merely documented: Task 6 calls `precheck_readers` BEFORE
/// persisting the settings document, and a conflict on reader 3 of 5 must not have already rolled
/// readers 1 and 2.
pub async fn roll_readers(s: &ApiState, scope: &Scope, readers: &[&str], reason: RollReason, by: &str) -> Result<(), Response> {
    precheck_readers(s, scope, readers).await?;
    let client = client_for(s, scope).await?;
    let mut targets = Vec::with_capacity(readers.len());
    for &name in readers {
        let kind = resolve(scope, name).ok_or_else(not_found)?;
        let ns = namespace(scope, name);
        targets.push((name, ns, kind));
    }
    let ts = chrono::Utc::now().to_rfc3339();
    let reason_text = reason.text();
    let patch = roll_patch(&ts, by, &reason_text);
    for (name, ns, kind) in targets {
        apply_patch(client, ns, kind, name, &patch).await.map_err(kube_err)?;
        // ponytail: tracing is the only admin audit sink today — no dedicated log store exists to
        // reuse. Route this through one if/when the quotas plan's decisions gain a real sink.
        tracing::info!(scope = %scope, workload = name, %by, reason = %reason_text, "workload rolled");
    }
    Ok(())
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkloadDoc {
    pub scope: Scope,
    pub name: String,
    pub kind: Kind,
    pub image: Option<String>,
    pub ready: i64,
    pub desired: i64,
    pub rollout_state: &'static str,
    pub last_roll: Option<LastRoll>,
}

/// One row, resolved the same way `roll_readers` resolves its targets — the manual roll route's
/// response, so it never has to re-walk the whole `KNOWN` list for the one workload it just
/// touched.
pub async fn workload_doc(s: &ApiState, scope: &Scope, name: &str) -> Result<WorkloadDoc, Response> {
    let kind = resolve(scope, name).ok_or_else(not_found)?;
    let client = client_for(s, scope).await?;
    doc(client, scope.clone(), name, kind).await.map_err(kube_err)
}

async fn doc(client: &kube::Client, scope: Scope, name: &str, kind: Kind) -> Result<WorkloadDoc, kube::Error> {
    let ns = namespace(&scope, name);
    let info = fetch(client, ns, kind, name).await?;
    Ok(WorkloadDoc {
        scope,
        name: name.to_string(),
        kind,
        image: info.image,
        ready: info.ready,
        desired: info.desired,
        rollout_state: if info.ready < info.desired { "RollingOut" } else { "Stable" },
        last_roll: info.last_roll,
    })
}

/// Every `KNOWN` entry: central once, plus one row per region for the per-region half. `regions`
/// is the caller's job to supply (from `crd::Region`) — this module has no opinion on where that
/// list comes from, only on what to do with it once it has it.
pub async fn list_workloads(s: &ApiState, regions: &[String]) -> Result<Vec<WorkloadDoc>, Response> {
    let mut rows = Vec::new();
    if let Ok(client) = aks(s) {
        for (name, kind) in KNOWN_CENTRAL {
            rows.push(doc(client, Scope::Central, name, *kind).await.map_err(kube_err)?);
        }
    }
    if let Ok(client) = kube(s) {
        for region in regions {
            for (name, kind) in KNOWN_PER_REGION {
                rows.push(doc(client, Scope::Region(region.clone()), name, *kind).await.map_err(kube_err)?);
            }
        }
    }
    Ok(rows)
}

#[cfg(test)]
mod scope_tests {
    use super::Scope;

    /// The wire form (`"central"` / the bare region id) is what the web reads and what a path
    /// segment carries back; the two must agree or a roll from the infrastructure tab would target
    /// a scope the listing never named.
    #[test]
    fn scope_round_trips_through_its_wire_form() {
        for scope in [Scope::Central, Scope::Region("centralindia-k3s".into())] {
            let wire: String = serde_json::from_str(&serde_json::to_string(&scope).unwrap()).unwrap();
            assert_eq!(crate::api::admin::parse_scope(&wire), scope);
        }
    }
}
