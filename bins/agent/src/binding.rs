//! The owner's NAMESPACES, ensured by exactly one reconciler.
//!
//! They used to be re-ensured by the workspace reconciler AND the environment reconciler on every
//! pass — two writers for one object, which is how a namespace ends up recreated by whichever ran
//! last. An `OwnerBinding` is that one writer: the object that says "this owner has namespaces,
//! LimitRanges, NetworkPolicies and RoleBindings", and nothing else.
//!
//! It is NOT node-scoped. It used to be — the owner's home was a btrfs subvolume on one node, so
//! the binding pinned every workspace of theirs to it. The home is a directory on a region-shared
//! NFS mount now, so every node reconciles every binding (the objects below are all
//! server-side-applied, which is convergent under concurrent appliers) and placement is the
//! claim's own business.
//!
//! ponytail: bindings are never deleted, and the node-retirement path (`decommission.rs`) does
//! not collect them — deliberately. A binding is an OWNER's namespaces, not a node's: draining
//! the last node an owner happened to run on must not delete the namespace their workspaces come
//! back to. The upgrade, if orphaned bindings ever cost anything, is an owner-deletion path in
//! `/v1`, not a node-side sweep.

use crate::controller::{conditions_eq, ensure, patch_status, settle, Ctx, Outcome, ReconcileErr};
use k8s_openapi::api::core::v1::{LimitRange, Namespace, ResourceQuota};
use k8s_openapi::api::networking::v1::NetworkPolicy;
use k8s_openapi::api::rbac::v1::RoleBinding;
use kube::api::{Api, ListParams};
use kube::runtime::controller::Action;
use kube::{Resource, ResourceExt};
use rustic_git_workspaces::crd::{self, binding_name, ws_namespace};
use rustic_git_workspaces::k8s;
use std::collections::BTreeSet;
use std::sync::Arc;

pub const NAMESPACE_READY: &str = "NamespaceReady";

/// Every team this owner has a workspace in ON THIS NODE, plus the personal namespace.
///
/// The personal one is unconditional: a first workspace's reconcile waits on `NamespaceReady`, and
/// gating the namespace on a workspace that is itself waiting for the namespace is a deadlock.
///
/// Both selectors are server-side — a label selector and a field selector are separate query
/// parameters, and `.status.nodeName` is `selectable` on the Workspace CRD — so the response is
/// this node's workspaces for this owner and nothing else.
///
/// Keying off the owner LABEL rather than `spec.owner` is what makes the query indexed at all; the
/// label is a view that `heal_labels` re-stamps on every node reconcile, so a Workspace written by
/// some other path is invisible here for at most one pass — and the Workspace watch on this
/// controller is what re-triggers the binding once it has been stamped.
async fn teams_in_use(ctx: &Arc<Ctx>, owner: &str) -> Result<BTreeSet<String>, ReconcileErr> {
    let api: Api<crd::Workspace> = Api::all(ctx.client.clone());
    let lp = ListParams::default()
        .labels(&format!("{}={owner}", k8s::OWNER_LABEL))
        .fields(&format!("status.nodeName={}", ctx.node));
    let mut teams = BTreeSet::from([String::new()]);
    for w in api.list(&lp).await?.items {
        // Re-checked locally: the field selector is only honoured by a CRD that declares
        // `.status.nodeName` selectable, and a cluster still on an older CRD would hand back every
        // node's workspaces — which would have this node build namespaces for someone else's.
        if w.status.as_ref().map(|s| s.node_name.as_str()) == Some(ctx.node.as_str()) {
            teams.insert(w.spec.team.clone());
        }
    }
    Ok(teams)
}

/// Write the status only when it actually says something new.
///
/// `crd::condition` stamps `lastTransitionTime` with `now`, so an unconditional write produces new
/// bytes on every pass, which fires this controller's own watch, which writes again: a hot loop
/// that never idles. `conditions_eq` ignores that timestamp for exactly this reason.
async fn write_binding_status(b: &crd::OwnerBinding, ctx: &Arc<Ctx>, gen: i64) -> Result<(), ReconcileErr> {
    let conds = vec![crd::condition(NAMESPACE_READY, true, "Converged", "namespaces exist on this node", gen)];
    if let Some(cur) = &b.status {
        if cur.observed_generation == Some(gen) && conditions_eq(&cur.conditions, &conds) {
            return Ok(());
        }
    }
    let api: Api<crd::OwnerBinding> = Api::all(ctx.client.clone());
    patch_status(
        &api,
        &b.name_any(),
        "OwnerBinding",
        serde_json::json!({"observedGeneration": gen, "conditions": conds}),
    )
    .await
}

pub async fn apply_binding(b: &crd::OwnerBinding, ctx: &Arc<Ctx>) -> Result<Action, ReconcileErr> {
    let gen = b.meta().generation.unwrap_or(0);
    let Some(owner_ref) = b.controller_owner_ref(&()) else {
        // Unreachable for an object that came off a watch, and permanent if it ever happens: no
        // retry invents a uid.
        return settle(
            Outcome::Permanent("binding has no uid".into(), "NoUid"),
            b,
            "OwnerBinding",
            gen,
            |c| serde_json::json!({"observedGeneration": gen, "conditions": [c]}),
            ctx,
        )
        .await;
    };
    let owner = &b.spec.owner;
    for team in teams_in_use(ctx, owner).await? {
        let ns = ws_namespace(owner, &team);
        // No ownerReference on the namespace or the LimitRange: the namespace is shared by every
        // workspace this user owns IN THIS TEAM, and an owner's quota ceiling must not vanish with
        // a binding rewrite. See `crd::ws_namespace`.
        ensure(&Api::<Namespace>::all(ctx.client.clone()), &k8s::namespace(&ns, owner, "workspace", None), ctx).await?;
        ensure(
            &Api::<LimitRange>::namespaced(ctx.client.clone(), &ns),
            &k8s::limit_range(&ns, owner, "workspace", &crd::PodResources::default(), None),
            ctx,
        )
        .await?;
        // The owner's ceiling, projected. A TEAM namespace gets the TEAM's quota: the working
        // copies in it are the team's, so the team's number is the one that bounds them.
        let q = rustic_git_workspaces::quota::effective(
            &ctx.client,
            if team.is_empty() { owner } else { &team },
            !team.is_empty(),
        )
        .await?;
        ensure(
            &Api::<ResourceQuota>::namespaced(ctx.client.clone(), &ns),
            &k8s::resource_quota(&ns, owner, "workspace", &q),
            ctx,
        )
        .await?;
        let policies = Api::<NetworkPolicy>::namespaced(ctx.client.clone(), &ns);
        for p in k8s::default_policies(&ns, owner, &owner_ref) {
            ensure(&policies, &p, ctx).await?;
        }
        // The one ingress hole: port 22 from the region's gateway. Written here rather than by the
        // workspace reconciler because the policy covers the whole SHARED namespace — an
        // ownerReference to any one workspace would revoke ssh for its siblings when it is deleted.
        ensure(&policies, &k8s::allow_gateway_ingress(&ns, owner, &owner_ref), ctx).await?;
        // Scope the API's Secret access to THIS namespace. The alternative is a cluster-wide
        // `secrets: create` for the API, which would include the agent's own credentials.
        let bindings = Api::<RoleBinding>::namespaced(ctx.client.clone(), &ns);
        ensure(
            &bindings,
            &k8s::api_secret_binding(&ns, owner, crate::controller::API_SERVICE_ACCOUNT, crate::controller::API_NAMESPACE, Some(&owner_ref)),
            ctx,
        )
        .await?;
        // And the agent's own: the host-key Secret it reads and creates in `ensure_ssh` is
        // granted here, per namespace, instead of `secrets` cluster-wide.
        ensure(&bindings, &k8s::agent_secret_binding(&ns, owner, &owner_ref), ctx).await?;
    }
    write_binding_status(b, ctx, gen).await?;
    Ok(Action::await_change())
}

/// Whether this workspace's OWN namespace exists on this node's view of the cluster.
///
/// The `OwnerBinding` condition alone is not enough: `teams_in_use` is scoped to THIS node's
/// workspaces, so node A can report `NamespaceReady=True` for an owner whose team-B namespace no
/// node has made yet, and a workspace claimed on B would then fail into `ensure_ssh`'s 60 s retry
/// instead of waiting here. Both halves are asked — the condition says the binding pass has run at
/// all, the namespace says it ran for THIS team. A missing binding or namespace is "not ready",
/// never an error: it is the ordinary gap between a claim and the binding reconcile.
pub async fn namespace_ready(ctx: &Arc<Ctx>, region: &str, owner: &str, team: &str) -> Result<bool, ReconcileErr> {
    let Some(b) = get_binding(ctx, region, owner).await? else { return Ok(false) };
    if !b.status.is_some_and(|s| s.conditions.iter().any(|c| c.type_ == NAMESPACE_READY && c.status == "True")) {
        return Ok(false);
    }
    let ns = ws_namespace(owner, team);
    let api: Api<Namespace> = Api::all(ctx.client.clone());
    Ok(api.get_opt(&ns).await?.is_some())
}

/// The owner's binding in this region, if any.
pub async fn get_binding(ctx: &Arc<Ctx>, region: &str, owner: &str) -> Result<Option<crd::OwnerBinding>, ReconcileErr> {
    let api: Api<crd::OwnerBinding> = Api::all(ctx.client.clone());
    Ok(api.get_opt(&binding_name(region, owner)).await?)
}
