//! Placement, as a reconciler.
//!
//! An object with an empty `status.nodeName` is UNPLACED. Each agent runs a second watch selecting
//! exactly those, and the first node whose claim lands wins. The claim is a status write and only a
//! status write: the API authored this object's spec, and a controller that edits a user's desired
//! state is the failure this whole design exists to remove.
//!
//! Two nodes for now — one session, one env — so the claim checks no free space at all.
//! ponytail: no capacity check in the claim; `placement::pick` (the same algorithm, still in
//! `rustic_git_workspaces::placement`) is what a second node of a role would consult, so growing
//! the pool is a deploy, not a rewrite.

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

/// The `compatibleNodes` of a `cloneOf` source, when there is one. A source that has vanished
/// yields `Some([])` — nobody claims, and the object stays visible as unplaced rather than being
/// silently started somewhere with no data.
async fn source_nodes(
    ctx: &Arc<Ctx>,
    source: Option<&crd::VolumeSource>,
) -> Result<Option<Vec<String>>, ReconcileErr> {
    let Some(crd::VolumeSource::CloneOf { volume }) = source else { return Ok(None) };
    let api: Api<crd::Workspace> = Api::all(ctx.client.clone());
    let nodes = match api.get_opt(volume).await? {
        Some(w) => w.status.map(|s| s.compatible_nodes).unwrap_or_default(),
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
fn with_me(existing: &[String], me: &str) -> Vec<String> {
    let mut out = existing.to_vec();
    if !out.iter().any(|n| n == me) {
        out.push(me.to_string());
    }
    out
}

/// A pre-migration object: it names its node in the DEPRECATED `spec.nodeName` and has never had a
/// `status.nodeName`, so it matches the unplaced watch while already being placed. Claiming it here
/// would hand it to whichever agent saw it first, ignoring the node its subvolume is actually on.
/// The startup migration is what moves these onto status.
fn is_legacy(spec_node: Option<&String>) -> bool {
    spec_node.is_some_and(|n| !n.is_empty())
}

pub async fn claim_workspace(w: &crd::Workspace, ctx: &Arc<Ctx>) -> Result<Action, ReconcileErr> {
    let st = w.status.clone().unwrap_or_default();
    if !st.node_name.is_empty() || is_legacy(w.spec.node_name.as_ref()) {
        // Already placed: the disk has not moved, so a later start reconciles here with no
        // placement step at all.
        return Ok(Action::await_change());
    }
    let src = source_nodes(ctx, storage_source(w.spec.storage.as_ref())).await?;
    if !may_claim(&ctx.node, &st.compatible_nodes, src.as_deref()) {
        return Ok(Action::await_change());
    }
    let gen = w.meta().generation.unwrap_or(0);
    let status = serde_json::json!({
        "phase": crd::Phase::Pending,
        "nodeName": ctx.node,
        "compatibleNodes": with_me(&st.compatible_nodes, &ctx.node),
        "conditions": [crd::condition("Placed", true, "Claimed", &format!("claimed by {}", ctx.node), gen)],
    });
    let api: Api<crd::Workspace> = Api::all(ctx.client.clone());
    // Optimistic, carrying `metadata.resourceVersion`. NOT `patch_status`, which applies FORCED and
    // therefore never conflicts — with a forced apply two agents both "win" and the second silently
    // overwrites the first, which is the whole failure this write exists to prevent. A 409 means a
    // peer claimed it; its write is a watch event that brings us back, so there is nothing to retry
    // and nothing to bind.
    match replace_status(&api, w, "Workspace", status).await {
        Ok(()) => {}
        Err(kube::Error::Api(s)) if s.code == 409 => {
            tracing::info!(workspace = %w.name_any(), "lost the placement race; a peer claimed it");
            return Ok(Action::await_change());
        }
        Err(e) => return Err(e.into()),
    }
    // Only the WINNER binds. Binding an owner to a node that lost would send every later workspace
    // of theirs to the wrong pool.
    ensure_binding(ctx, &w.spec.region, &w.spec.owner).await?;
    Ok(Action::await_change())
}

pub async fn claim_environment(e: &crd::Environment, ctx: &Arc<Ctx>) -> Result<Action, ReconcileErr> {
    let st = e.status.clone().unwrap_or_default();
    if !st.node_name.is_empty() || is_legacy(e.spec.node_name.as_ref()) {
        return Ok(Action::await_change());
    }
    // Environments have no clone-of-a-running-source path through placement: `clone_env` copies a
    // volume by id and the copy is materialized by the Volume controller, which needs the same
    // disk — the same rule, expressed through the same helper.
    let src = source_nodes(ctx, storage_source(e.spec.storage.as_ref())).await?;
    if !may_claim(&ctx.node, &st.compatible_nodes, src.as_deref()) {
        return Ok(Action::await_change());
    }
    let gen = e.meta().generation.unwrap_or(0);
    let status = serde_json::json!({
        "phase": crd::Phase::Creating,
        "nodeName": ctx.node,
        "compatibleNodes": with_me(&st.compatible_nodes, &ctx.node),
        "conditions": [crd::condition("Placed", true, "Claimed", &format!("claimed by {}", ctx.node), gen)],
    });
    let api: Api<crd::Environment> = Api::all(ctx.client.clone());
    match replace_status(&api, e, "Environment", status).await {
        Ok(()) => {}
        Err(kube::Error::Api(s)) if s.code == 409 => {
            tracing::info!(environment = %e.name_any(), "lost the placement race; a peer claimed it");
            return Ok(Action::await_change());
        }
        Err(err) => return Err(err.into()),
    }
    ensure_binding(ctx, &e.spec.region, &e.spec.owner).await?;
    Ok(Action::await_change())
}

/// The `{region, owner}` binding for this node, created atomically. A 409 means a peer got there
/// first and its answer is as good as ours — the binding is what makes the per-owner namespace
/// reconciler run, not a second placement decision.
pub async fn ensure_binding(ctx: &Arc<Ctx>, region: &str, owner: &str) -> Result<(), ReconcileErr> {
    let api: Api<OwnerBinding> = Api::all(ctx.client.clone());
    let name = binding_name(region, owner);
    let b = OwnerBinding::new(
        &name,
        OwnerBindingSpec { owner: owner.into(), region: region.into(), node_name: ctx.node.clone() },
    );
    match api.create(&PostParams::default(), &b).await {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(s)) if s.code == 409 => Ok(()),
        Err(e) => Err(e.into()),
    }
}
