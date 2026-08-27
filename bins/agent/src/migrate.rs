//! One-shot startup migration, in the shape `engine::migrate_ws_to_vol` already established.
//!
//! Three things exist from before this change and cannot be reconciled into place on their own: a
//! `Volume` with no ownerReference (nothing would ever GC it), a parent with no `status.nodeName`
//! (the placement watch would claim it a second time, possibly on another node), and pushed history
//! that lives only in the registry (the history page reads CRs now).
//!
//! Idempotent by construction: every write is "set it to what it should be", so a restart mid-way
//! costs a second pass and nothing else — which is also what makes it safe to run while the
//! reconcilers are running. Nothing here deletes.

use crate::controller::{patch_status, Ctx, ReconcileErr};
use kube::api::{Api, ListParams, Patch, PatchParams, PostParams};
use kube::{Resource, ResourceExt};
use rustic_git_workspaces::crd;
use rustic_git_workspaces::k8s;
use std::sync::Arc;

pub async fn once(ctx: &Arc<Ctx>) -> Result<(), ReconcileErr> {
    let ws: Api<crd::Workspace> = Api::all(ctx.client.clone());
    for w in ws.list(&ListParams::default()).await?.items {
        let placed = w.status.as_ref().map(|s| s.node_name.clone());
        adopt(ctx, &w, "Workspace", &w.spec.owner, placed).await?;
    }
    let envs: Api<crd::Environment> = Api::all(ctx.client.clone());
    for e in envs.list(&ListParams::default()).await?.items {
        let placed = e.status.as_ref().map(|s| s.node_name.clone());
        adopt(ctx, &e, "Environment", &e.spec.owner, placed).await?;
    }
    Ok(())
}

/// Adopt one parent's `Volume` (same name as the parent) and backfill its placement + history.
async fn adopt<P>(
    ctx: &Arc<Ctx>,
    parent: &P,
    kind: &str,
    owner: &str,
    placed: Option<String>,
) -> Result<(), ReconcileErr>
where
    P: Resource<DynamicType = ()> + ResourceExt,
{
    let id = parent.name_any();
    let vols: Api<crd::Volume> = Api::all(ctx.client.clone());
    let Some(vol) = vols.get_opt(&id).await? else { return Ok(()) };
    // Only this node's objects: the volume's spec is the only place the pre-migration node lives.
    if vol.spec.node_name != ctx.node {
        return Ok(());
    }
    if vol.metadata.owner_references.as_ref().is_none_or(|r| r.is_empty()) {
        // The ONE place the agent patches a Volume's METADATA rather than its status: an object
        // written before the parent/child link existed has no other way to acquire it, and without
        // it a deleted parent leaves its disk behind forever.
        let owner_ref = crate::controller::owner_ref_of_kind(parent)?;
        let patch = serde_json::json!({"metadata": {"ownerReferences": [owner_ref]}});
        vols.patch(&id, &PatchParams::default(), &Patch::Merge(&patch)).await?;
        tracing::info!(volume = %id, %kind, "migration: adopted an orphan volume");
    }
    if placed.as_deref().unwrap_or_default().is_empty() {
        // From the VOLUME's spec, which is where the pre-migration node actually lives: the new
        // schema prunes the parent's own deprecated `spec.nodeName` on read, so it cannot be
        // trusted to still be there. `observedGeneration` is untouched — nothing was observed here.
        let status = serde_json::json!({
            "phase": crd::Phase::Pending,
            "nodeName": vol.spec.node_name,
            "compatibleNodes": [vol.spec.node_name],
            "volumeRef": id,
        });
        match kind {
            "Workspace" => patch_status(&Api::<crd::Workspace>::all(ctx.client.clone()), &id, kind, status).await?,
            _ => patch_status(&Api::<crd::Environment>::all(ctx.client.clone()), &id, kind, status).await?,
        }
        tracing::info!(object = %id, node = %vol.spec.node_name, "migration: backfilled placement from the volume");
    }
    backfill_history(ctx, &id, owner).await
}

/// One `SnapshotRequest` per registry commit record, `phase: done`, ids taken from the record.
///
/// Reads the record surface through the Engine's own `RegistryClient` — the agent already has it
/// pointed at the server tier. A registry that cannot be reached SKIPS this volume with a warning
/// rather than failing startup: the CRs above are the part that must not be missing, and a history
/// page that is briefly empty is recoverable on the next boot.
async fn backfill_history(ctx: &Arc<Ctx>, id: &str, owner: &str) -> Result<(), ReconcileErr> {
    let history = match ctx.engine.registry.get_history(owner, id).await {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(volume = %id, error = %e, "migration: registry unreachable, history not backfilled");
            return Ok(());
        }
    };
    let api: Api<crd::SnapshotRequest> = Api::all(ctx.client.clone());
    for rec in history {
        // Deterministic name from the record id: a re-run must not mint a second object for one
        // snapshot, so the API server's own uniqueness IS the guard and a 409 is success.
        let name = format!("snap-{}", rec.id.to_lowercase());
        let mut req = crd::SnapshotRequest::new(
            &name,
            crd::SnapshotRequestSpec { volume: id.to_string(), message: rec.message.clone() },
        );
        // No finalizer, unlike a live push: this request describes work that already finished, so
        // there is nothing for a delete to interrupt.
        req.metadata.labels = Some(std::collections::BTreeMap::from([
            (k8s::OWNER_LABEL.to_string(), owner.to_string()),
            (crd::VOLUME_LABEL.to_string(), id.to_string()),
        ]));
        match api.create(&PostParams::default(), &req).await {
            Ok(_) => {}
            Err(kube::Error::Api(s)) if s.code == 409 => continue,
            Err(e) => return Err(e.into()),
        }
        let status = serde_json::json!({
            "phase": crd::Phase::Done,
            "snapshotId": rec.id,
            "lineageTip": rec.id,
            "at": rec.created_at.to_rfc3339(),
            "conditions": [crd::condition("Ready", true, "Backfilled", "record read from the registry at migration", 0)],
        });
        patch_status(&api, &name, "SnapshotRequest", status).await?;
    }
    Ok(())
}
