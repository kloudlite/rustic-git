//! One-shot startup migration, in the shape `engine::migrate_ws_to_vol` already established.
//!
//! Three things exist from before this change and cannot be reconciled into place on their own: a
//! `Volume` with no ownerReference (nothing would ever GC it), a parent with no `status.nodeName`
//! (the placement watch would claim it a second time, possibly on another node), and pushed history
//! that lives only in the registry (the history page reads CRs now).
//!
//! Idempotent by construction: every write is "set it to what it should be", so a restart mid-way
//! costs a second pass and nothing else — which is also what makes it safe to run while the
//! reconcilers are running. Nothing here deletes, and nothing here is fatal: a failure warns and
//! leaves the object for the next boot, because a controller that will not start converges nothing
//! at all while one that skipped an object still converges the rest.

use crate::controller::{patch_status, Ctx, ReconcileErr};
use kube::api::{Api, ListParams, Patch, PatchParams, PostParams};
use kube::{Resource, ResourceExt};
use rustic_git_workspaces::crd;
use rustic_git_workspaces::k8s;
use std::sync::Arc;

// ponytail: this lists every Workspace and Environment in the cluster on every boot and re-posts
// one SnapshotRequest per registry record, absorbing an N-way 409 for the ones already migrated —
// fine at three workspaces and a handful of pushes. Upgrade: a `migrated` annotation checked before
// the registry read, or delete the module outright at Task 11 when nothing legacy is left.
pub async fn once(ctx: &Arc<Ctx>) {
    let ws: Api<crd::Workspace> = Api::all(ctx.client.clone());
    match ws.list(&ListParams::default()).await {
        Ok(list) => {
            for w in list.items {
                let placed = w.status.as_ref().map(|s| s.node_name.clone());
                warn_on_err(
                    &w.name_any(),
                    adopt(ctx, &w, "Workspace", &w.spec.owner, placed, w.spec.node_name.as_deref(), w.spec.volume_ref.as_deref()).await,
                );
            }
        }
        Err(e) => tracing::warn!(error = %e, "migration: could not list workspaces, skipping them"),
    }
    let envs: Api<crd::Environment> = Api::all(ctx.client.clone());
    match envs.list(&ListParams::default()).await {
        Ok(list) => {
            for e in list.items {
                let placed = e.status.as_ref().map(|s| s.node_name.clone());
                warn_on_err(
                    &e.name_any(),
                    adopt(ctx, &e, "Environment", &e.spec.owner, placed, e.spec.node_name.as_deref(), e.spec.volume_ref.as_deref()).await,
                );
            }
        }
        Err(e) => tracing::warn!(error = %e, "migration: could not list environments, skipping them"),
    }
}

fn warn_on_err(name: &str, r: Result<(), ReconcileErr>) {
    if let Err(e) = r {
        tracing::warn!(object = %name, error = %e, "migration: object skipped, will be retried next boot");
    }
}

/// Adopt one parent's `Volume` (same name as the parent) and backfill its placement + history.
async fn adopt<P>(
    ctx: &Arc<Ctx>,
    parent: &P,
    kind: &str,
    owner: &str,
    placed: Option<String>,
    spec_node: Option<&str>,
    spec_volume_ref: Option<&str>,
) -> Result<(), ReconcileErr>
where
    P: Resource<DynamicType = ()> + ResourceExt,
{
    let id = parent.name_any();
    let vols: Api<crd::Volume> = Api::all(ctx.client.clone());
    let Some(vol) = vols.get_opt(&id).await? else {
        // A legacy parent whose Volume has gone is stuck: it still looks legacy, so the claim
        // watch will not place it. Back-fill placement from its own deprecated `spec.nodeName` so
        // the reconciler picks it up and reports the missing Volume through `resolve_volume`,
        // instead of it sitting invisible forever.
        tracing::warn!(object = %id, %kind, volume_ref = ?spec_volume_ref, "migration: parent names a Volume that does not exist");
        if placed.as_deref().unwrap_or_default().is_empty() && spec_node == Some(ctx.node.as_str()) {
            let node = ctx.node.clone();
            write_placement(ctx, kind, &id, &node, spec_volume_ref.unwrap_or(&id)).await?;
        }
        return Ok(());
    };
    // Only this node's objects. The Volume's spec is the authority on where the disk actually is —
    // the parent's deprecated `spec.nodeName` still exists in the schema (Task 11 removes it) but
    // is not consulted while a Volume is there to answer.
    if vol.spec.node_name != ctx.node {
        tracing::warn!(object = %id, node = %vol.spec.node_name, "migration: volume lives on another node, skipping");
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
        let node = vol.spec.node_name.clone();
        write_placement(ctx, kind, &id, &node, &id).await?;
    }
    backfill_history(ctx, &id, owner).await
}

/// The placement half of the backfill: `observedGeneration` is untouched — nothing was observed
/// here — and `phase` rides along because every status write in this group carries one.
async fn write_placement(ctx: &Arc<Ctx>, kind: &str, id: &str, node: &str, volume_ref: &str) -> Result<(), ReconcileErr> {
    let status = serde_json::json!({
        "phase": crd::Phase::Pending,
        "nodeName": node,
        "compatibleNodes": [node],
        "volumeRef": volume_ref,
    });
    match kind {
        "Workspace" => patch_status(&Api::<crd::Workspace>::all(ctx.client.clone()), id, kind, status).await?,
        _ => patch_status(&Api::<crd::Environment>::all(ctx.client.clone()), id, kind, status).await?,
    }
    tracing::info!(object = %id, %node, "migration: backfilled placement");
    Ok(())
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
        // Deterministic name from volume + record id: a re-run must not mint a second object for
        // one snapshot, so the API server's own uniqueness IS the guard and a 409 is success. The
        // volume is in the name because record ids are only unique within one volume's history.
        let name = format!("snap-{id}-{}", rec.id).to_lowercase();
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
            // Falls THROUGH to the status write rather than skipping: a crash between the create
            // and its status leaves a `pending` request, which the snapshot reconciler would run as
            // a real push. Writing `done` again over `done` costs nothing; not writing it costs a
            // spurious snapshot.
            Err(kube::Error::Api(s)) if s.code == 409 => {}
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
