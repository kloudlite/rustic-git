//! The `Environment` reconciler: one volume, a namespace of StatefulSets, and the restore gate.
//! Split out of `controller.rs` unchanged.
//!
//! The module map: `run` (the running half of a reconcile), `stop`, `intercept` (when an
//! intercept takes hold or lets go), `services` (reading StatefulSets back, the abandoned
//! Endpoints), `mounts`. `apply_environment` stays here.

use super::stop::{replicated_condition, running_condition, stop_name, stop_push, StopPush};
use super::workspace::{cleared_node_dead, replaced};
use super::{my_node, delete_ignoring_404, ensure, forget_applied, heal_labels, kept_conditions, owner_ref_of_kind, resolve_volume, settle, write_status, conditions_eq, Ctx, Outcome, ReconcileErr, Resolved, API_NAMESPACE, API_SERVICE_ACCOUNT, TICK};
use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::core::v1::{Endpoints, LimitRange, Namespace, Pod, ResourceQuota, Service};
use k8s_openapi::api::discovery::v1::EndpointSlice;
use k8s_openapi::api::networking::v1::NetworkPolicy;
use k8s_openapi::api::rbac::v1::RoleBinding;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, OwnerReference};
use kube::api::{Patch, PatchParams};
use kube::runtime::controller::Action;
use kube::{Api, Resource, ResourceExt};
use kloudlite_workspaces::crd::{self, DesiredState};
use kloudlite_workspaces::k8s;
use kloudlite_workspaces::model;
use std::sync::Arc;
use std::time::Duration;

mod run;
pub(crate) use run::*;
mod services;
pub(crate) use services::*;
mod intercept;
pub(crate) use intercept::*;


mod stop;

mod mounts;
pub(crate) use stop::*;
pub(crate) use mounts::*;


/// The environment-side half of every attachment grant in this namespace, held against the
/// Workspace it names: gone, or attached elsewhere, and the grant goes. `/v1`'s `delete_ws` and
/// detach remove it themselves, best-effort with a warning — and a warning is where that ended
/// until 2026-09-11: nothing on the controller side ever revisited a grant the api failed to
/// remove, so it stood until the environment itself was deleted. Same shape as the Volume
/// collector added the same day: the api's cleanup is the fast path, the controller is the truth.
async fn prune_attach_grants(e: &crd::Environment, ctx: &Arc<Ctx>) -> Result<(), ReconcileErr> {
    let ns = crd::env_namespace(&e.name_any());
    let policies: Api<NetworkPolicy> = Api::namespaced(ctx.client.clone(), &ns);
    let list = match policies.list(&kube::api::ListParams::default()).await {
        Ok(l) => l.items,
        // No namespace yet is no grants yet.
        Err(kube::Error::Api(ae)) if ae.code == 404 => return Ok(()),
        Err(err) => return Err(ReconcileErr(err.to_string())),
    };
    let workspaces: Api<crd::Workspace> = Api::all(ctx.client.clone());
    for p in list {
        let name = p.name_any();
        let Some(ws) = name.strip_prefix("attach-") else { continue };
        let keep = match workspaces.get_opt(ws).await.map_err(|err| ReconcileErr(err.to_string()))? {
            Some(w) => crd::attached_environment(&w).as_deref() == Some(e.name_any().as_str()),
            None => false,
        };
        if !keep {
            tracing::info!(environment = %e.name_any(), workspace = %ws, "attach.grant.pruned");
            delete_ignoring_404(&policies, &name).await?;
        }
    }
    Ok(())
}
pub async fn apply_environment(e: &crd::Environment, ctx: &Arc<Ctx>) -> Result<Action, ReconcileErr> {
    // Above every write, exactly as `apply_workspace` does — see `my_node`.
    let me = my_node(ctx).await;
    if me.dead {
        return Ok(Action::requeue(TICK));
    }
    // A cleanup, not the environment's own convergence: a failure here is a warning, never a
    // failed reconcile — the 10:44 roll of 2026-09-11 lacked `list` on NetworkPolicies and every
    // environment on the fleet stopped converging for the eight minutes it took to notice.
    if let Err(err) = prune_attach_grants(e, ctx).await {
        tracing::warn!(environment = %e.name_any(), error = %err.0, "attach.grant.prune.failed");
    }
    let gen = e.meta().generation.unwrap_or(0);
    // `spec.owner` reaches `ensure_homecache`'s `{pool}/homecache/{owner}` here too. Only the
    // owner: `EnvironmentSpec.name` is display text that reaches no path and no argv — the
    // namespace and every pool path are built from `vol.name_any()`, not from it.
    if let Err(why) = model::validate_owner(&e.spec.owner) {
        let prev = e.status.clone().unwrap_or_default();
        return settle(
            Outcome::Permanent(why, "InvalidSpec"),
            e,
            "Environment",
            gen,
            // Pruned-on-omit, as above: keep the placement fields and the prior conditions.
            move |cond| {
                serde_json::json!({
                    "phase": crd::Phase::Error,
                    "nodeName": prev.node_name,
                    "volumeRef": prev.volume_ref,
                    "conditions": kept_conditions(&prev.conditions, cond),
                })
            },
            ctx,
        )
        .await;
    }
    heal_labels(&Api::<crd::Environment>::all(ctx.client.clone()), e, &e.spec.owner, "", "environment").await?;
    let prev = e.status.clone().unwrap_or_default();
    let owner_ref = owner_ref_of_kind(e)?;
    // Same resolution as a workspace, including the release-1 adoption — an environment is
    // team-owned, so it has no team of its own.
    let vol = match resolve_volume(
        e,
        &e.spec.owner,
        "",
        &e.spec.region,
        &e.spec.storage,
        &prev.node_name.clone(),
        &prev.conditions.clone(),
        gen,
        ctx,
    )
    .await?
    {
        Resolved::Ready(v) => *v,
        Resolved::Settled(a) => return Ok(a),
        // No StatefulSet may exist before the disk does: a pod bound to an unmaterialized subvolume
        // wedges forever on `path … does not exist`.
        Resolved::Wait { volume_ref, phase, cond, action } => {
            let st = crd::EnvironmentStatus {
                // An environment whose disk is being swapped is not being CREATED, and saying so
                // is alarming in the one moment a person is already nervous: an in-flight restore
                // keeps whatever phase this environment had. `Creating` is right only for a volume
                // that has never been materialized.
                phase: if e.spec.restore.is_some() && prev.phase != crd::Phase::Pending { prev.phase } else { phase },
                observed_generation: None,
                volume_ref: volume_ref.or(prev.volume_ref.clone()),
                conditions: vec![cond],
                ..prev
            };
            write_env_status(e, st, ctx).await?;
            return Ok(action);
        }
    };
    let id = vol.name_any();

    // The environment's OWN id, never the volume's: a restored environment resolves to the
    // SOURCE's volume (`resolve_volume`'s `shared` arm), and running it in the source's namespace
    // would collide every StatefulSet name with the source's.
    let ns = crd::env_namespace(&e.name_any());
    let deployments: Api<StatefulSet> = Api::namespaced(ctx.client.clone(), &ns);

    // Before anything else, including the stop path: an environment that is being restored has no
    // business converging its services against a disk that is about to be swapped underneath them.
    if let Some(action) = restore_gate(e, &vol, &ns, &deployments, gen, ctx).await? {
        return Ok(action);
    }

    if e.spec.desired_state == DesiredState::Stopped {
        return stop_environment(e, &vol, &ns, &deployments, prev, gen, ctx).await;
    }
    // Starts spread, exactly as a workspace's does — same decision, same one-caller rule: only the
    // owner, only when nothing on the volume is running. An environment has no `podRef`, so
    // `is_live_worktree` reads any non-`Stopped` phase as live: the decision belongs on the START
    // pass, which is the one moment its status still says `Stopped`, and that is what this gate is.
    if super::start_spread("Environment", &e.name_any(), &id, &vol, prev.phase, ctx).await?.is_some() {
        return Ok(Action::await_change());
    }
    run_environment(e, &vol, &ns, &deployments, &owner_ref, prev, gen, me.decommissioning, ctx).await
}


#[cfg(test)]
mod tests {
    use super::*;

    fn svc(folder: &str) -> model::Service {
        serde_json::from_value(serde_json::json!({
            "name": "db", "image": "mongo:7", "command": [], "env": {},
            "ports": [], "mounts": [{"path": "/data/db", "folder": folder}],
        }))
        .unwrap()
    }

    /// `create_dir_all` on an unvalidated folder IS the escape — it would happily mkdir -p outside
    /// the subvolume before a pod ever bound it as a subPath. `validate_mount` is tested in
    /// `model.rs`; this asserts the controller actually calls it, which is where the escape lives.
    #[test]
    fn a_traversing_folder_makes_no_directory_and_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let live = tmp.path().join("live");
        std::fs::create_dir_all(&live).unwrap();
        for folder in ["../../etc", "..", "a/b", "/abs", ""] {
            assert!(mkdir_env_mounts(&live, &[svc(folder)]).is_err(), "accepted {folder:?}");
        }
        assert!(!tmp.path().join("etc").exists(), "nothing was created outside the subvolume");
        assert!(std::fs::read_dir(live.join("volumes")).map(|mut d| d.next().is_none()).unwrap_or(true));
    }

    /// The ordinary folder is made, once, under `volumes/`.
    #[test]
    fn a_valid_folder_is_created_under_volumes() {
        let tmp = tempfile::tempdir().unwrap();
        let live = tmp.path().join("live");
        std::fs::create_dir_all(&live).unwrap();
        mkdir_env_mounts(&live, &[svc("dbdata"), svc("dbdata")]).unwrap();
        assert!(live.join("volumes/dbdata").is_dir());
    }

    /// Dedup is by folder, so the SECOND service on a folder never reached the check — and the
    /// first one's mkdir had already run by then. Every mount is validated before any is made
    /// (2026-09-12).
    #[test]
    fn an_invalid_mount_behind_a_valid_duplicate_is_still_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let live = tmp.path().join("live");
        std::fs::create_dir_all(&live).unwrap();
        assert!(mkdir_env_mounts(&live, &[svc("dbdata"), svc("../../etc")]).is_err());
        assert!(!live.join("volumes/dbdata").exists(), "nothing is created once any mount is refused");
    }

    fn ported(ports: &[u16]) -> model::Service {
        let mut s = svc("dbdata");
        s.ports = ports.to_vec();
        s
    }

    fn intercept(ports: &[(u16, u16)]) -> crd::Intercept {
        crd::Intercept {
            service: "db".into(),
            workspace: "ws-1".into(),
            ports: ports.iter().map(|(s, w)| crd::PortMap { service: *s, workspace: *w }).collect(),
        }
    }

    /// The slice matches the service's port to the workspace's by the port NAME `p{port}`, so a
    /// rewrite of a port the service does not declare renders a slice matching nothing — the real
    /// service scaled to zero and the traffic delivered nowhere. Refused up front instead.
    #[test]
    fn a_port_rewrite_is_checked_against_what_the_service_declares() {
        assert!(invalid_port_map(&ported(&[8080]), &intercept(&[(8080, 3000)])).is_none());
        assert!(invalid_port_map(&ported(&[8080]), &intercept(&[])).is_none(), "no rewrite forwards 1:1");
        assert!(invalid_port_map(&ported(&[8080]), &intercept(&[(9090, 3000)])).is_some(), "not a declared port");
        assert!(invalid_port_map(&ported(&[8080]), &intercept(&[(0, 3000)])).is_some(), "0 is not a port");
        assert!(invalid_port_map(&ported(&[8080]), &intercept(&[(8080, 0)])).is_some(), "0 is not a port");
    }

    /// The clock the grace measures against. The last case is the one that matters: a workspace
    /// whose node died has no pod and no `Ready=False` of its own, so the FIRST pass records now
    /// (nothing has been waited yet, the grace runs in full) and every later pass measures from
    /// that record rather than restarting the grace or borrowing an unrelated timestamp.
    #[test]
    fn the_outage_clock_prefers_the_pod_then_the_workspace_then_its_own_record() {
        assert_eq!(outage_since(Some(10), Some(20), Some(30), 100), 10, "the pod observed it first-hand");
        assert_eq!(outage_since(None, Some(20), Some(30), 100), 20, "no pod object: the workspace's own condition");
        assert_eq!(outage_since(None, None, Some(30), 100), 30, "a dead node stamps neither: our own record");
        assert_eq!(outage_since(None, None, None, 100), 100, "nothing anywhere: the outage starts now");
    }

    /// The dead-node sequence end to end, in the units the caller uses. Recording `now` on the
    /// first sight is what makes this terminate: hold for the grace, then fall back — not hold
    /// forever, and not fall back immediately off some older object's timestamp.
    #[test]
    fn a_dead_nodes_workspace_holds_for_the_grace_and_then_falls_back() {
        let first = outage_since(None, None, None, 1_000);
        assert_eq!(1_000 - first, 0, "nothing waited yet on the pass that discovers it");
        let held = 1_000 + INTERCEPT_GRACE_SECS - 1;
        assert!((0..INTERCEPT_GRACE_SECS).contains(&(held - outage_since(None, None, Some(first), held))));
        let expired = 1_000 + INTERCEPT_GRACE_SECS;
        assert!(!(0..INTERCEPT_GRACE_SECS).contains(&(expired - outage_since(None, None, Some(first), expired))));
    }
}
