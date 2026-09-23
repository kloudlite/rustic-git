//! Subagent trees: one reconcile step that makes `{ws}/.agents/{name}` match `spec.trees`.
//!
//! A tree is a writable btrfs snapshot of the workspace's own worktree, NESTED inside it. That
//! nesting is the whole design: `btrfs send` does not carry a nested subvolume, so a tree never
//! travels with a push, a sync point or a replica — a replica of the workspace holds an empty
//! `.agents/{name}` directory and nothing else — and it costs no quota beyond the bytes it
//! diverges by.
//!
//! The decision is a pure function over three inputs (the ask, what we last reported, what is on
//! disk) so the keep rules are readable and testable without btrfs; the pass applies it and
//! reports. On-disk is an INPUT rather than status alone because the crash window between `/v1`'s
//! DELETE and this pass leaves a subvolume no status row names.

use super::{Ctx, ReconcileErr};
use kloudlite_workspaces::crd;
use kube::ResourceExt;
use std::sync::Arc;


/// What one tree needs. Ordered `Cut` before `Delete` in the vec purely so a pass reads in the
/// order a person thinks about it; neither depends on the other.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TreeAction {
    Cut(String),
    Delete(String),
}


/// THE decision. Cut every asked-for tree that is not reported ready; delete every tree the ask
/// does not name, whether status still remembers it or only the disk does.
///
/// Keep-biased in the one direction that matters: a name is only deleted when `spec.trees` does
/// NOT carry it. A pass that read a stale spec would therefore delete a tree somebody just asked
/// for — which is why the caller reads the workspace it was handed by the watch and never a
/// cached listing.
pub fn tree_actions(spec: &[crd::TreeSpec], status: &[crd::TreeStatus], on_disk: &[String]) -> Vec<TreeAction> {
    let asked: std::collections::HashSet<&str> = spec.iter().map(|t| t.name.as_str()).collect();
    let ready: std::collections::HashSet<&str> =
        status.iter().filter(|t| t.ready).map(|t| t.name.as_str()).collect();
    let mut acts: Vec<TreeAction> = spec
        .iter()
        .filter(|t| !ready.contains(t.name.as_str()) || !on_disk.iter().any(|d| d == &t.name))
        .map(|t| TreeAction::Cut(t.name.clone()))
        .collect();
    // Status rows and the disk both name candidates; either alone would miss a crash window, and
    // a name in both must not be deleted twice.
    let mut gone: Vec<&str> =
        status.iter().map(|t| t.name.as_str()).chain(on_disk.iter().map(String::as_str)).filter(|n| !asked.contains(n)).collect();
    gone.sort_unstable();
    gone.dedup();
    acts.extend(gone.into_iter().map(|n| TreeAction::Delete(n.to_string())));
    acts
}


/// Converge this workspace's trees and report what is there. Called only once the pod is Running:
/// a tree is a snapshot of a LIVE working directory, and `/v1` already refuses the ask on a
/// stopped workspace, so anything reaching here has a worktree to cut from.
///
/// One failure never blocks another tree: each is reported on its own row, and a failed cut stays
/// `ready: false` with the reason so the next pass retries it. The whole step is best-effort about
/// the ERROR path and exact about the SUCCESS path — a tree reported ready is one this node
/// actually holds.
pub(crate) async fn reconcile_trees(
    w: &crd::Workspace,
    volume: &str,
    ctx: &Arc<Ctx>,
) -> Result<Vec<crd::TreeStatus>, ReconcileErr> {
    let id = w.name_any();
    let prev: Vec<crd::TreeStatus> = w.status.as_ref().map(|s| s.trees.clone()).unwrap_or_default();
    let (engine, vol, wt) = (ctx.engine.clone(), volume.to_string(), id.clone());
    let on_disk = tokio::task::spawn_blocking(move || engine.list_trees(&vol, &wt))
        .await
        .map_err(|e| ReconcileErr(e.to_string()))?
        .map_err(|e| ReconcileErr(e.0))?;
    let mut rows: Vec<crd::TreeStatus> = Vec::new();
    for act in tree_actions(&w.spec.trees, &prev, &on_disk) {
        match act {
            TreeAction::Cut(name) => {
                let (engine, vol, wt, n) = (ctx.engine.clone(), volume.to_string(), id.clone(), name.clone());
                let cut = tokio::task::spawn_blocking(move || engine.cut_tree(&vol, &wt, &n))
                    .await
                    .map_err(|e| ReconcileErr(e.to_string()))?;
                match cut {
                    Ok(()) => tracing::info!(workspace = %id, tree = %name, "tree.cut"),
                    Err(e) => {
                        tracing::warn!(workspace = %id, tree = %name, error = %e, "tree.cut.failed");
                        rows.push(crd::TreeStatus {
                            path: crd::tree_path(&name),
                            name,
                            ready: false,
                            reason: Some(e.0),
                        });
                    }
                }
            }
            TreeAction::Delete(name) => {
                let (engine, vol, wt, n) = (ctx.engine.clone(), volume.to_string(), id.clone(), name.clone());
                let dropped = tokio::task::spawn_blocking(move || engine.drop_tree(&vol, &wt, &n))
                    .await
                    .map_err(|e| ReconcileErr(e.to_string()))?;
                match dropped {
                    Ok(()) => tracing::info!(workspace = %id, tree = %name, "tree.dropped"),
                    // Kept, not reported: a tree the ask no longer names has no row to carry a
                    // reason, and the next pass tries again. Losing the bytes is the only outcome
                    // that would matter, and a failed delete loses nothing.
                    Err(e) => tracing::warn!(workspace = %id, tree = %name, error = %e, "tree.drop.failed"),
                }
            }
        }
    }
    // Every asked-for tree that is not already carrying a failure row is reported ready: the cut
    // either just succeeded or was already on disk and left alone.
    for t in &w.spec.trees {
        if !rows.iter().any(|r| r.name == t.name) {
            rows.push(crd::TreeStatus { name: t.name.clone(), path: crd::tree_path(&t.name), ready: true, reason: None });
        }
    }
    Ok(rows)
}
