//! The per-owner shared objects, owned by exactly one reconciler.
//!
//! They used to be re-ensured by the workspace reconciler AND the environment reconciler on every
//! pass — two writers for one object, which is how a namespace ends up recreated by whichever ran
//! last. An `OwnerBinding` says "this owner's work lives on this node", so it is the natural owner
//! of "this owner has namespaces on this node".
//!
//! ponytail: bindings are never deleted; a node-retirement path re-homes them later.

use crate::controller::{ensure, patch_status, settle, Ctx, Outcome, ReconcileErr, TICK};
use k8s_openapi::api::core::v1::{LimitRange, Namespace};
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

/// How long a waiter sleeps between `NamespaceReady` checks. Re-exported so the two parent
/// reconcilers cannot disagree about it.
pub const WAIT: std::time::Duration = TICK;

/// Every team this owner has a workspace in ON THIS NODE, plus the personal namespace.
///
/// The personal one is unconditional: a first workspace's reconcile waits on `NamespaceReady`, and
/// gating the namespace on a workspace that is itself waiting for the namespace is a deadlock.
///
/// The node filter is client-side because a list cannot select on two fields of two different
/// kinds at once; the label selector is what keeps the response proportional to one owner.
async fn teams_in_use(ctx: &Arc<Ctx>, owner: &str) -> Result<BTreeSet<String>, ReconcileErr> {
    let api: Api<crd::Workspace> = Api::all(ctx.client.clone());
    let lp = ListParams::default().labels(&format!("{}={owner}", k8s::OWNER_LABEL));
    let mut teams = BTreeSet::from([String::new()]);
    for w in api.list(&lp).await?.items {
        if w.status.as_ref().map(|s| s.node_name.as_str()) == Some(ctx.node.as_str()) {
            teams.insert(w.spec.team.clone());
        }
    }
    Ok(teams)
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
        ensure(&Api::<Namespace>::all(ctx.client.clone()), &k8s::namespace(&ns, owner, "workspace", None)).await?;
        ensure(
            &Api::<LimitRange>::namespaced(ctx.client.clone(), &ns),
            &k8s::limit_range(&ns, owner, "workspace", &crd::PodResources::default(), None),
        )
        .await?;
        let policies = Api::<NetworkPolicy>::namespaced(ctx.client.clone(), &ns);
        for p in k8s::default_policies(&ns, owner, &owner_ref) {
            ensure(&policies, &p).await?;
        }
        // Scope the API's Secret access to THIS namespace. The alternative is a cluster-wide
        // `secrets: create` for the API, which would include the agent's own credentials.
        ensure(
            &Api::<RoleBinding>::namespaced(ctx.client.clone(), &ns),
            &k8s::api_secret_binding(&ns, owner, &ctx.api_service_account, &ctx.api_namespace, Some(&owner_ref)),
        )
        .await?;
    }
    let status = serde_json::json!({
        "observedGeneration": gen,
        "conditions": [crd::condition(NAMESPACE_READY, true, "Converged", "namespaces exist on this node", gen)],
    });
    let api: Api<crd::OwnerBinding> = Api::all(ctx.client.clone());
    patch_status(&api, &b.name_any(), "OwnerBinding", status).await?;
    Ok(Action::await_change())
}

/// Whether the owner's binding on this node reports `NamespaceReady`. A missing binding is "not
/// ready", never an error: it is the ordinary gap between a claim and the binding reconcile.
pub async fn namespace_ready(ctx: &Arc<Ctx>, region: &str, owner: &str) -> Result<bool, ReconcileErr> {
    let api: Api<crd::OwnerBinding> = Api::all(ctx.client.clone());
    let Some(b) = api.get_opt(&binding_name(region, owner)).await? else { return Ok(false) };
    Ok(b.status
        .is_some_and(|s| s.conditions.iter().any(|c| c.type_ == NAMESPACE_READY && c.status == "True")))
}
