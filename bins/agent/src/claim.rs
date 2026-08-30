//! Placement, as a reconciler.
//!
//! An object with an empty `status.nodeName` is UNPLACED. Each agent runs a second watch selecting
//! exactly those, and the first node whose claim lands wins. The claim is a status write and only a
//! status write: the API authored this object's spec, and a controller that edits a user's desired
//! state is the failure this whole design exists to remove.
//!
//! Two nodes for now — one session, one env — so the claim checks no free space at all.
//! ponytail: no capacity check in the claim; a pool big enough for nodes to fill unevenly needs one
//! here (node allocatable minus scheduled pod requests), which is a change to this function only.

use crate::controller::{replace_status, Ctx, ReconcileErr};
use kube::api::{Api, PostParams};
use kube::runtime::controller::Action;
use kube::{Resource, ResourceExt};
use rustic_git_workspaces::crd::{self, binding_name, OwnerBinding, OwnerBindingSpec};
use std::sync::Arc;

/// Whether THIS node may claim `object`, given the nodes already known to hold its data.
///
/// Empty `compatible` with no source means "nowhere holds it yet", which every node may claim. A
/// `cloneOf` is the exception the spec calls out: the new object holds nothing, but a local clone
/// needs the SOURCE's disk, so the source's memory decides.
fn may_claim(me: &str, compatible: &[String], source_compatible: Option<&[String]>) -> bool {
    if let Some(src) = source_compatible {
        return src.iter().any(|n| n == me);
    }
    compatible.is_empty() || compatible.iter().any(|n| n == me)
}

/// The nodes holding a `cloneOf` source's disk, when there is one. A source that has vanished
/// yields `Some([])` — nobody claims, and the object stays visible as unplaced rather than being
/// silently started somewhere with no data.
///
/// Resolved as a `Volume`, not as a `Workspace`: `clone_env` writes the ENVIRONMENT's id here, so a
/// workspace-only lookup never found it and no node ever claimed a cloned environment. Both kinds
/// own a Volume of the parent's own name, and its `spec.nodeName` is the disk's real location —
/// which is the only thing placement needs.
async fn source_nodes(
    ctx: &Arc<Ctx>,
    source: Option<&crd::VolumeSource>,
) -> Result<Option<Vec<String>>, ReconcileErr> {
    let Some(crd::VolumeSource::CloneOf { volume }) = source else { return Ok(None) };
    let api: Api<crd::Volume> = Api::all(ctx.client.clone());
    let nodes = match api.get_opt(volume).await? {
        Some(v) => vec![v.spec.node_name],
        None => vec![],
    };
    Ok(Some(nodes))
}

/// The `storage.source` of a parent, which is `Option` for release 1 (a legacy object has no
/// `storage` block at all — see `WorkspaceSpec::storage`).
fn storage_source(storage: Option<&crd::WorkspaceStorage>) -> Option<&crd::VolumeSource> {
    storage.and_then(|s| s.source.as_ref())
}

/// `union(existing, {me})` — a SET, computed and set, never appended.
///
/// A level-triggered reconciler re-runs by design, and an append grows the array every time. The
/// desired value is "every node known to hold this object's data, including me"; that is what gets
/// written, so re-running is a no-op instead of a leak.
pub(crate) fn with_me(existing: &[String], me: &str) -> Vec<String> {
    let mut out = existing.to_vec();
    if !out.iter().any(|n| n == me) {
        out.push(me.to_string());
    }
    out
}

/// The mirror of `with_me`: a node that no longer holds a replica removes itself from
/// `compatibleNodes` — the deselected half of the rendezvous, computed by the standby itself
/// since every node runs the same deterministic `replicate::targets` math. A no-op, not an error,
/// when the name is already absent (the common case on a retry after a 409).
pub(crate) fn without_me(existing: &[String], me: &str) -> Vec<String> {
    existing.iter().filter(|n| n.as_str() != me).cloned().collect()
}

/// A pre-migration object: it names its node in the DEPRECATED `spec.nodeName` and has never had a
/// `status.nodeName`, so it matches the unplaced watch while already being placed. Claiming it here
/// would hand it to whichever agent saw it first, ignoring the node its subvolume is actually on.
/// The startup migration is what moves these onto status.
/// What the claim decides for one object, given what it currently says about itself. `None` means
/// leave it alone; `Some(status)` is the status to write.
///
/// Split out because a 409 has to run the SAME decision against the re-read object: the peer that
/// beat us may have placed it (leave it), or may have only widened `compatibleNodes` (still ours to
/// claim). A second, subtly different decision on the retry path is how a loser talks itself into
/// overwriting a winner.
async fn decide(
    ctx: &Arc<Ctx>,
    node_name: &str,
    compatible: &[String],
    storage: Option<&crd::WorkspaceStorage>,
    phase: crd::Phase,
    gen: i64,
) -> Result<Option<serde_json::Value>, ReconcileErr> {
    if !node_name.is_empty() {
        // Already placed: the disk has not moved, so a later start reconciles here with no
        // placement step at all.
        return Ok(None);
    }
    let src = source_nodes(ctx, storage_source(storage)).await?;
    if !may_claim(&ctx.node, compatible, src.as_deref()) {
        return Ok(None);
    }
    Ok(Some(serde_json::json!({
        "phase": phase,
        "nodeName": ctx.node,
        "compatibleNodes": with_me(compatible, &ctx.node),
        "conditions": [crd::condition("Placed", true, "Claimed", &format!("claimed by {}", ctx.node), gen)],
    })))
}

/// Whether `{region, owner}` is already bound to a node that is not this one. An owner's
/// namespaces are built on the bound node and nowhere else (`binding.rs` lists only that node's
/// workspaces), so a fresh object claimed anywhere else would create pods into a namespace that
/// never exists — 404 forever. Checked only once `decide` says claim, so an object this node has no
/// business with (placed, or outside `compatibleNodes`) still costs no API read.
async fn bound_elsewhere(ctx: &Arc<Ctx>, region: &str, owner: &str) -> Result<bool, ReconcileErr> {
    let api: Api<OwnerBinding> = Api::all(ctx.client.clone());
    Ok(api.get_opt(&binding_name(region, owner)).await?.is_some_and(|b| b.spec.node_name != ctx.node))
}

/// One optimistic attempt, then — on 409 — one re-read and one more.
///
/// Two passes, not a loop: a third attempt against a peer that keeps winning is a hot loop over the
/// API server, and the peer's own write is a watch event that brings this object back anyway. So
/// the fallback is always `await_change()`, never a requeue.
const ATTEMPTS: usize = 2;

/// Everything the claim needs out of one object, whatever its kind.
struct Parts<'a> {
    node_name: String,
    compatible: Vec<String>,
    storage: Option<&'a crd::WorkspaceStorage>,
    region: &'a str,
    owner: &'a str,
}

/// The claim itself, for any kind that carries `Parts`. Written once because a second, subtly
/// different copy of the 409 arms is exactly how a loser talks itself into overwriting a winner.
async fn claim<K>(
    obj: &K,
    ctx: &Arc<Ctx>,
    kind: &'static str,
    phase: crd::Phase,
    parts: fn(&K) -> Parts<'_>,
) -> Result<Action, ReconcileErr>
where
    K: Resource<DynamicType = ()> + Clone + serde::Serialize + serde::de::DeserializeOwned + std::fmt::Debug,
{
    let api: Api<K> = Api::all(ctx.client.clone());
    let mut obj = obj.clone();
    for attempt in 0..ATTEMPTS {
        let p = parts(&obj);
        let Some(status) = decide(
            ctx,
            &p.node_name,
            &p.compatible,
            p.storage,
            phase,
            obj.meta().generation.unwrap_or(0),
        )
        .await?
        else {
            return Ok(Action::await_change());
        };
        if bound_elsewhere(ctx, p.region, p.owner).await? {
            return Ok(Action::await_change());
        }
        // Optimistic, carrying `metadata.resourceVersion`. NOT `patch_status`, which applies FORCED
        // and therefore never conflicts — with a forced apply two agents both "win" and the second
        // silently overwrites the first, which is the whole failure this write exists to prevent.
        match replace_status(&api, &obj, kind, status).await {
            Ok(()) => {
                // Only the WINNER binds. Binding an owner to a node that lost would send every
                // later workspace of theirs to the wrong pool.
                let (region, owner) = (p.region.to_string(), p.owner.to_string());
                ensure_binding(ctx, &region, &owner).await?;
                return Ok(Action::await_change());
            }
            Err(kube::Error::Api(s)) if s.code == 409 && attempt + 1 < ATTEMPTS => {
                // A peer wrote first. Re-read and re-decide rather than assuming it placed the
                // object: it may have written something else entirely.
                tracing::info!(%kind, object = %obj.name_any(), "placement write conflicted; re-reading");
                let name = obj.name_any();
                obj = api.get(&name).await?;
            }
            Err(kube::Error::Api(s)) if s.code == 409 => {
                tracing::info!(%kind, object = %obj.name_any(), "lost the placement race; a peer claimed it");
                return Ok(Action::await_change());
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(Action::await_change())
}

pub async fn claim_workspace(w: &crd::Workspace, ctx: &Arc<Ctx>) -> Result<Action, ReconcileErr> {
    claim(w, ctx, "Workspace", crd::Phase::Pending, |o| {
        let st = o.status.clone().unwrap_or_default();
        Parts {
            node_name: st.node_name,
            compatible: st.compatible_nodes,
            storage: o.spec.storage.as_ref(),
            region: &o.spec.region,
            owner: &o.spec.owner,
        }
    })
    .await
}

pub async fn claim_environment(e: &crd::Environment, ctx: &Arc<Ctx>) -> Result<Action, ReconcileErr> {
    // Environments have no clone-of-a-running-source path through placement: `clone_env` copies a
    // volume by id and the copy is materialized by the Volume controller, which needs the same disk
    // — the same rule, expressed through the same helper.
    claim(e, ctx, "Environment", crd::Phase::Creating, |o| {
        let st = o.status.clone().unwrap_or_default();
        Parts {
            node_name: st.node_name,
            compatible: st.compatible_nodes,
            storage: o.spec.storage.as_ref(),
            region: &o.spec.region,
            owner: &o.spec.owner,
        }
    })
    .await
}

/// The `{region, owner}` binding for this node, created atomically. A 409 means a peer got there
/// first and its answer is as good as ours — the binding is what makes the per-owner namespace
/// reconciler run, not a second placement decision.
pub async fn ensure_binding(ctx: &Arc<Ctx>, region: &str, owner: &str) -> Result<(), ReconcileErr> {
    let api: Api<OwnerBinding> = Api::all(ctx.client.clone());
    let name = binding_name(region, owner);
    let b = OwnerBinding::new(
        &name,
        OwnerBindingSpec {
            owner: owner.into(),
            region: region.into(),
            node_name: ctx.node.clone(),
            home_quota_gb: crd::DEFAULT_HOME_QUOTA_GB,
        },
    );
    match api.create(&PostParams::default(), &b).await {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(s)) if s.code == 409 => Ok(()),
        Err(e) => Err(e.into()),
    }
}
