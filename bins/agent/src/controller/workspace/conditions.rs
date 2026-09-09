//! The `Ready`/`Attached`/`Placed` conditions a workspace reconcile writes, and the rules for
//! carrying an earlier pass's conditions forward without erasing what another pass wrote.

use super::*;


/// The pod step's conditions, keeping whatever the packages step said about this profile — and the
/// `Attached` condition, which is not decoration: it is the ONLY record of which environment's
/// namespace holds this workspace's ingress half, and dropping it on a stop (or any pass that
/// rebuilds the list) strands that grant on the detach after it. The pod path recomputes `Attached`
/// and replaces this copy.
pub(crate) fn ws_conditions(prev: &crd::WorkspaceStatus, ready: Condition) -> Vec<Condition> {
    kept_conditions(&prev.conditions, ready)
}


/// The same, for the writes that have the previous condition list but not the whole status —
/// `resolve_volume` is shared with environments and takes it as a slice, and `settle`'s builders
/// only have what they captured.
///
/// EVERY workspace status write goes through one of these two. Three separate sites that built the
/// list literally each dropped `Attached` and stranded the same grant, which is why the invariant is
/// "no literal condition list on a workspace path" rather than three more fixes.
// pub so the keep-list is assertable from the integration suite — see reconcile.rs.
pub fn kept_conditions(prev: &[Condition], ready: Condition) -> Vec<Condition> {
    // Every type here is owned by a writer that is NOT this path: `PackagesReady` by the profile
    // step, `Attached` by the pod path, `Replicated` by `replicated_condition` (which the
    // per-volume sweep reads and never computes), `Decommissioning` by the drain notice. A wait
    // arm dropping one makes its reader see a value nobody computed.
    let keep = [crd::PACKAGES_READY, crd::ATTACHED, "Replicated", "Decommissioning"];
    let mut c: Vec<Condition> =
        prev.iter().filter(|c| keep.contains(&c.type_.as_str()) && c.type_ != ready.type_).cloned().collect();
    c.push(ready);
    c
}


/// One condition replaced by type, the rest kept in order. `Replicated` is rewritten on every
/// reconcile of a stopped parent, and a naive push would grow the list without bound.
pub(crate) fn replaced(prev: &[Condition], c: Condition) -> Vec<Condition> {
    let mut out: Vec<Condition> = prev.iter().filter(|p| p.type_ != c.type_).cloned().collect();
    out.push(c);
    out
}


/// Drop the dead-node sweep's `Degraded=True/NodeDead` from a parent's conditions.
///
/// The sweep only ever writes it from ANOTHER node, and nothing ever cleared it: a workspace
/// stopped on a node that then died kept `NodeDead` after the node came back Ready, and `/v1`
/// went on answering `start` with 409 "interrupted" forever (drill, 2026-09-03). The owner
/// reconciling its own object IS the proof its node is alive — the watch is field-selected on
/// `status.nodeName`, so nobody else reaches this code for this object.
pub(crate) fn cleared_node_dead(prev: &[Condition]) -> Vec<Condition> {
    prev.iter().filter(|c| !(c.type_ == "Degraded" && c.reason == "NodeDead")).cloned().collect()
}


/// `ws_conditions` with this pass's freshly resolved `Attached` — replacing the preserved copy,
/// which is the previous pass's answer, and dropping it entirely when nothing is attached.
pub(crate) fn with_attached(conds: Vec<Condition>, attached: Option<Condition>) -> Vec<Condition> {
    conds.into_iter().filter(|c| c.type_ != crd::ATTACHED).chain(attached).collect()
}
