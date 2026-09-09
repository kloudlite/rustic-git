//! `Replicated`: whether another node holds this worktree's final sync point by name — the one
//! fact placement, the dead-node sweep and `/v1` all read.

use super::*;


/// The volume's replica count from the shared watch store, never a GET: a stop must not depend on
/// the Volume being readable (a workspace whose subvolume broke could then never be stopped). An
/// unknown volume gets the CRD's own default, which is what the reconciler that creates the
/// replica children uses for a `Volume` written before the field existed.
pub(crate) fn replicas_of(ctx: &Arc<Ctx>, id: &str) -> u32 {
    ctx.volumes
        .get(&kube::runtime::reflector::ObjectRef::new(id))
        .map(|v| v.spec.replicas)
        .unwrap_or(crd::DEFAULT_REPLICAS)
}
