//! Every status write in this controller, and the object-applied bookkeeping around them.
//!
//! One module because they share one invariant: a status write that produces new bytes fires this
//! controller's own watch, which writes again — `settled_status_eq` and `conditions_eq` are what
//! make a converged pass idle instead of hot-looping. Split out of `controller.rs` unchanged.

use super::{Ctx, ReconcileErr};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use kube::api::{Patch, PatchParams, PostParams};
use kube::runtime::controller::Action;
use kube::{Api, Resource, ResourceExt};
use rustic_git_workspaces::crd;
use std::sync::Arc;
use std::time::Duration;

/// Why a reconcile could not finish, and therefore what to do about it.
///
/// Today every failure is `Action::requeue(RETRY)`, which makes a spec that can never work look
/// exactly like a registry that is briefly down — the same line in the log, forever, at one a
/// minute. The new `storage.source` inputs make that untenable: a `cloneOf` naming a workspace that
/// does not exist, a `restoreOf` whose snapshot no `done` request carries, a Volume pinned to
/// another node — none of these get better by being retried.
pub enum Outcome {
    /// Nothing will change this without a new spec. Write the condition, stop.
    Permanent(String, &'static str),
    /// The world is briefly unavailable. Return `Err` and take `error_policy`'s backoff.
    Transient(ReconcileErr),
}

impl From<kube::Error> for Outcome {
    /// An API-server error is transient by default — a 5xx, a timeout, a lost connection. A 404 on
    /// a REFERENCE (a `cloneOf` source, say) is permanent, but only the caller knows which
    /// reference it was reading, so that classification is made at the call site, not here.
    fn from(e: kube::Error) -> Self {
        Outcome::Transient(ReconcileErr(e.to_string()))
    }
}

/// The one status write for all three kinds: patch unless `same` says nothing the caller cares
/// about moved. The skip is not an optimization — a status write triggers this controller's own
/// watch event, so writing an unchanged status is a hot loop.
///
/// Each caller passes its OWN field list rather than deriving one: `lastTransitionTime` is
/// re-stamped every pass (hence `conditions_eq`), and per-kind fields a builder does not set must
/// not count as a change either. A whole-status compare would make a permanently-failed object
/// write on every reconcile — the very loop this exists to prevent.
pub(crate) async fn write_status<K, S>(
    obj: &K,
    kind: &str,
    cur: Option<&S>,
    st: &S,
    ctx: &Arc<Ctx>,
    same: impl Fn(&S, &S) -> bool,
) -> Result<(), ReconcileErr>
where
    K: Resource<DynamicType = ()> + Clone + serde::de::DeserializeOwned + std::fmt::Debug,
    S: serde::Serialize,
{
    if cur.is_some_and(|c| same(c, st)) {
        return Ok(());
    }
    let api: Api<K> = Api::all(ctx.client.clone());
    patch_status(&api, &obj.name_any(), kind, serde_json::to_value(st).map_err(|e| ReconcileErr(e.to_string()))?).await
}

/// Status equality that ignores `lastTransitionTime`: a condition re-stamped with `now` is not a
/// change, and treating it as one is the classic controller hot loop — a status write that triggers
/// its own watch event and reconciles again. That is an outage, not a warning.
pub(crate) fn conditions_eq(a: &[Condition], b: &[Condition]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(x, y)| {
            x.type_ == y.type_
                && x.status == y.status
                && x.reason == y.reason
                && x.message == y.message
                && x.observed_generation == y.observed_generation
        })
}

/// `conditions_eq`, for a status that is only a `serde_json::Value` — which is what `settle`'s
/// per-kind builders hand back. Compares `phase` and the conditions, ignoring `lastTransitionTime`;
/// every other field a builder writes is copied from the object's own previous status.
fn settled_status_eq<K: serde::Serialize>(obj: &K, next: &serde_json::Value) -> bool {
    fn shape(v: &serde_json::Value) -> serde_json::Value {
        let mut conds = v.get("conditions").cloned().unwrap_or(serde_json::Value::Null);
        if let Some(arr) = conds.as_array_mut() {
            for c in arr {
                if let Some(o) = c.as_object_mut() {
                    o.remove("lastTransitionTime");
                }
            }
        }
        serde_json::json!({"phase": v.get("phase"), "conditions": conds})
    }
    serde_json::to_value(obj)
        .ok()
        .and_then(|v| v.get("status").cloned())
        .is_some_and(|cur| shape(&cur) == shape(next))
}

/// An OPTIMISTIC status write: `replace_status` carrying the object's current
/// `metadata.resourceVersion`, so a concurrent writer makes this a 409.
///
/// The counterpart to `patch_status`, and the difference is the whole point. `patch_status` applies
/// FORCED, which is right for a write only one node can make (its own node's objects) and wrong for
/// the one write two nodes race: a forced apply has no precondition, never conflicts, and lets both
/// claimants believe they won. Use this for the claim; use `patch_status` for everything else.
///
/// It returns the raw `kube::Error` rather than a `ReconcileErr` so callers can branch on
/// `Api(s).code == 409` structurally — sniffing "409" out of a formatted string is how a message
/// change silently turns "a peer won" back into "retry forever". `?` still works from a reconcile,
/// via `From<kube::Error> for ReconcileErr`.
///
/// `status` must carry `phase`: the CRD schema declares it required, and a write without it is
/// rejected by the API server.
///
/// The body is the OBJECT AS FETCHED with its status replaced, because `replace_status` is a PUT of
/// a whole object and the object already carries the `metadata.resourceVersion` that makes the PUT
/// a precondition. The spec that rides along is ignored by the `/status` subresource — that is what
/// the subresource is for — so this still cannot edit desired state.
pub async fn replace_status<K>(api: &Api<K>, obj: &K, kind: &str, status: serde_json::Value) -> Result<(), kube::Error>
where
    K: Resource + Clone + serde::Serialize + serde::de::DeserializeOwned + std::fmt::Debug,
{
    let name = obj.meta().name.clone().unwrap_or_default();
    let mut body = serde_json::to_value(obj).map_err(kube::Error::SerdeError)?;
    body["apiVersion"] = serde_json::json!(format!("{}/{}", crd::GROUP, crd::VERSION));
    body["kind"] = serde_json::json!(kind);
    body["status"] = status;
    let next: K = serde_json::from_value(body).map_err(kube::Error::SerdeError)?;
    api.replace_status(&name, &PostParams::default(), &next).await?;
    Ok(())
}

/// Turn an `Outcome` into the reconcile's answer, writing the condition on the permanent path.
///
/// `await_change()` on permanent, deliberately: the object is wrong and the next thing that can
/// help is a human or a new spec, both of which arrive as watch events.
///
/// `reason` is a CamelCase token, never a sentence — `meta/v1.Condition` requires it and
/// `kubectl wait --for=condition=…` matches on it. The `write` closure exists because each kind's
/// status has a different shape; every call site passes a one-line builder for its own status.
pub async fn settle<K, F>(
    outcome: Outcome,
    obj: &K,
    kind: &str,
    gen: i64,
    write: F,
    ctx: &Arc<Ctx>,
) -> Result<Action, ReconcileErr>
where
    K: Resource<DynamicType = ()> + ResourceExt + Clone + serde::Serialize + serde::de::DeserializeOwned + std::fmt::Debug,
    F: FnOnce(Condition) -> serde_json::Value,
{
    match outcome {
        Outcome::Permanent(msg, reason) => {
            let cond = crd::condition("Ready", false, reason, &msg, gen);
            let next = write(cond);
            // A permanently-broken object reconciles on every watch event it causes, so writing an
            // unchanged status re-stamps `lastTransitionTime` and wakes itself: a hot loop that only
            // ever ends when someone fixes the spec. Same no-op guard as every other status writer.
            if settled_status_eq(obj, &next) {
                return Ok(Action::await_change());
            }
            tracing::warn!(kind = %kind, name = %obj.name_any(), reason = %reason, error = %msg, "permanent failure; not retrying");
            let api: Api<K> = Api::all(ctx.client.clone());
            patch_status(&api, &obj.name_any(), kind, next).await?;
            Ok(Action::await_change())
        }
        Outcome::Transient(e) => Err(e),
    }
}

/// Server-side apply on the `/status` subresource. Apply, not Merge: the field manager owns exactly
/// the status fields it sets, so two writers cannot silently clobber each other.
pub async fn patch_status<K>(api: &Api<K>, name: &str, kind: &str, status: serde_json::Value) -> Result<(), ReconcileErr>
where
    K: Clone + serde::de::DeserializeOwned + std::fmt::Debug,
{
    let body = serde_json::json!({
        "apiVersion": format!("{}/{}", crd::GROUP, crd::VERSION),
        "kind": kind,
        "status": status,
    });
    api.patch_status(name, &PatchParams::apply(crd::AGENT_FIELD_MANAGER).force(), &Patch::Apply(&body)).await?;
    Ok(())
}

/// How long `ensure` trusts its last apply. Bounds the one thing the skip costs: a child deleted by
/// hand, or scaled by a path that forgot to `forget_applied`, is re-applied within this.
const APPLY_RESYNC: Duration = Duration::from_secs(600);

/// Server-side apply of a whole child object: level-triggered convergence in one call, and the one
/// thing that makes "someone deleted the StatefulSet by hand" a self-healing event.
///
/// Skipped when the body hashes to what this process last applied under this name, less than
/// `APPLY_RESYNC` ago. A converged parent reconciles on every event of every child — each pod
/// transition re-applied ~10 objects per workspace and 8 + 4·S per environment, all no-ops on the
/// server and all PATCHes on the API server's ledger.
/// ponytail: the memory is per-process and time-bounded, not watch-driven — a child deleted by
/// hand comes back on the next apply after `APPLY_RESYNC`, not on its delete event. Any path that
/// changes a child OUTSIDE `ensure` (a scale, a delete) must `forget_applied` it first.
/// H4: a Pod's `volumes[].hostPath` is immutable, so once a home (or any volume) migrates to the
/// commit model's worktree layout mid-flight (H1/H3: the home beat now migrates every ready home
/// on every pass, whether or not its pod has been recreated yet), re-applying that pod's spec here
/// with the NEW path 422s against the still-running pod's OLD one — until something deletes the
/// pod so a fresh Apply can create it with the new hostPath. Not auto-deleted here on purpose: see
/// `deploy/k3s/README.md`'s commit-model cutover section for the explicit
/// `kubectl delete pods -l rustic-git.io/kind=...` step that closes this window operator-side.
pub(crate) async fn ensure<K>(api: &Api<K>, obj: &K, ctx: &Ctx) -> Result<(), ReconcileErr>
where
    K: Resource + Clone + serde::Serialize + serde::de::DeserializeOwned + std::fmt::Debug,
    K::DynamicType: Default,
{
    let name = obj.meta().name.clone().ok_or_else(|| ReconcileErr("child object has no name".into()))?;
    let key = applied_key(&K::kind(&Default::default()), obj.meta().namespace.as_deref(), &name);
    let hash = {
        use std::hash::{Hash, Hasher};
        let mut h = std::hash::DefaultHasher::new();
        serde_json::to_vec(obj).map_err(|e| ReconcileErr(e.to_string()))?.hash(&mut h);
        h.finish()
    };
    let fresh = ctx
        .applied
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(&key)
        .is_some_and(|(h, at)| *h == hash && at.elapsed() < APPLY_RESYNC);
    if fresh {
        return Ok(());
    }
    api.patch(&name, &PatchParams::apply(crd::AGENT_FIELD_MANAGER).force(), &Patch::Apply(obj)).await?;
    ctx.applied.lock().unwrap_or_else(|p| p.into_inner()).insert(key, (hash, std::time::Instant::now()));
    Ok(())
}

fn applied_key(kind: &str, ns: Option<&str>, name: &str) -> String {
    format!("{kind}/{}/{name}", ns.unwrap_or_default())
}

/// Drop `ensure`'s memory of one child, so the next pass applies it again whatever the hash says.
/// Called wherever a child is changed by something other than `ensure` — its absence there is a
/// service that stays scaled to zero after a restore, or never comes back after a stop.
pub(crate) fn forget_applied(ctx: &Ctx, kind: &str, ns: &str, name: &str) {
    ctx.applied.lock().unwrap_or_else(|p| p.into_inner()).remove(&applied_key(kind, Some(ns), name));
}

/// Create a child only when it is missing — for objects an apply cannot legally change: Pods.
///
/// NOT `ensure`. A Pod is immutable once created: re-applying its spec is refused with "pod updates
/// may not change fields other than `spec.containers[*].image`", so a server-side apply on every
/// reconcile turns the SECOND pass into a permanent error and the object never converges. That is
/// exactly what happened when the readiness gate started requeueing — the first pass created the
/// pod, and every pass after it failed.
///
/// Convergence for a Pod is therefore "exists or does not". A spec change that matters (a new
/// image, a different slot) has to delete and recreate, which is a restart of the user's workspace
/// and belongs to an explicit action, not to a reconcile that happens to notice drift.
/// ponytail: no drift detection on the pod spec; a changed `image` or `resources` needs a stop and
/// start to take effect.
pub(crate) async fn create_if_absent<K>(api: &Api<K>, obj: &K) -> Result<(), ReconcileErr>
where
    K: Resource + Clone + serde::Serialize + serde::de::DeserializeOwned + std::fmt::Debug,
    K::DynamicType: Default,
{
    let name = obj.meta().name.clone().ok_or_else(|| ReconcileErr("child object has no name".into()))?;
    if api.get_opt(&name).await?.is_some() {
        return Ok(());
    }
    match api.create(&kube::api::PostParams::default(), obj).await {
        Ok(_) => Ok(()),
        // Lost a race with our own earlier pass, or with the kubelet recreating it. Already done.
        Err(kube::Error::Api(s)) if s.code == 409 => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// A 404 is the desired state already reached, not an error — a stop that races a delete, or a
/// reconcile replayed after a restart.
pub(crate) async fn delete_ignoring_404<K>(api: &Api<K>, name: &str) -> Result<(), ReconcileErr>
where
    K: Clone + serde::de::DeserializeOwned + std::fmt::Debug,
{
    match api.delete(name, &Default::default()).await {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(s)) if s.code == 404 => Ok(()),
        Err(e) => Err(e.into()),
    }
}
