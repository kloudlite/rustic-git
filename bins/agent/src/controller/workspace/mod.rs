//! The `Workspace` reconciler: profile, host key, home, worktree, attachment and the one pod.
//! Split out of `controller.rs` unchanged.
//!
//! The module map: `profile` (nix packages), `conditions`, `replicas`, `home` (the NFS home),
//! `seed` (the git seed container), `lifecycle` (stop, delete, migrate), `status` (the status
//! write, labels, resolv.conf, host key). `apply_workspace` and the two reconcile entry points
//! stay here.

use super::stop::{replicated_condition, running_condition, stop_name, stop_push, StopPush};
use super::{my_node, conditions_eq, create_if_absent, delete_ignoring_404, ensure, heal_labels, owner_ref_of_kind, resolve_volume, settle, stopped_condition, wake_on_finish, write_status, Ctx, Done, Outcome, ReconcileErr, Resolved, RETRY, TICK};
use crate::binding;
use std::time::Duration;
use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::api::networking::v1::NetworkPolicy;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, OwnerReference};
use kube::api::{Patch, PatchParams, PostParams};
use kube::runtime::controller::Action;
use kube::runtime::finalizer::{finalizer, Event as FinalizerEvent};
use kube::{Api, Resource, ResourceExt};
use kloudlite_workspaces::crd::{self, DesiredState};
use kloudlite_workspaces::k8s;
use kloudlite_workspaces::model;
use std::sync::Arc;

mod lifecycle;
pub use lifecycle::*;
mod status;
pub use status::*;


mod profile;

mod conditions;

mod replicas;

mod home;
mod seed;
pub(crate) use profile::*;
pub use conditions::*;
pub(crate) use replicas::*;
pub(crate) use home::*;
pub(crate) use seed::*;


/// EVERY workspace carries `WORKTREE_FINALIZER`, not just a shared-volume clone: a delete now has
/// to decide whether the Volume goes with the parent (no snapshots — ownerReference GC as before) or
/// survives it detached (a pushed snapshot outlives the workspace it came from), and that decision
/// can only be made while the parent's own spec and status are still readable.
///
/// Two windows this deliberately does not close. (1) The rollout: a parent deleted between the
/// upgrade landing and its first post-upgrade reconcile carries no finalizer yet, so GC takes its
/// Volume the old way, snapshots included — one pass per object closes it, and there is no way to
/// stamp a finalizer on an object that is already gone. (2) An unclaimed Terminating parent:
/// a node-death sweep that cleared `status.nodeName` leaves nothing watching it (every parent
/// watch is `status.nodeName`-selected), so it waits in Terminating until a node re-claims it —
/// which converges, because the claim path ignores `deletionTimestamp` and places a deleting
/// object like any other.
pub async fn reconcile_workspace(w: Arc<crd::Workspace>, ctx: Arc<Ctx>) -> Result<Action, ReconcileErr> {
    let api: Api<crd::Workspace> = Api::all(ctx.client.clone());
    let out = finalizer(&api, crd::WORKTREE_FINALIZER, w, |event| async {
        match event {
            FinalizerEvent::Cleanup(w) => cleanup_workspace_worktree(&w, &ctx).await,
            FinalizerEvent::Apply(w) => apply_workspace(&w, &ctx).await,
        }
    })
    .await;
    super::finalized(out)
}


/// The same wrapper for an environment, whose worktree is its own id — see `reconcile_workspace`.
pub async fn reconcile_environment(e: Arc<crd::Environment>, ctx: Arc<Ctx>) -> Result<Action, ReconcileErr> {
    let api: Api<crd::Environment> = Api::all(ctx.client.clone());
    let r = finalizer(&api, crd::WORKTREE_FINALIZER, e, |event| async {
        match event {
            FinalizerEvent::Cleanup(e) => {
                let volume = e.status.as_ref().and_then(|s| s.volume_ref.clone());
                cleanup_parent(&*e, volume, |e: &crd::Environment| e.status.as_ref().and_then(|s| s.volume_ref.clone()), &ctx).await
            }
            FinalizerEvent::Apply(e) => super::apply_environment(&e, &ctx).await,
        }
    })
    .await;
    super::finalized(r)
}


pub async fn apply_workspace(w: &crd::Workspace, ctx: &Arc<Ctx>) -> Result<Action, ReconcileErr> {
    // A deleting object has nothing to converge, and an apply that runs on one WRITES: a restore
    // deleted 1.4 s after it was created was still applied, attached itself to a Volume, and the
    // cleanup that followed never knew (2026-09-12). The finalizer wrapper routes the next event
    // to Cleanup; this pass does nothing at all.
    if w.meta().deletion_timestamp.is_some() {
        return Ok(Action::await_change());
    }
    // FIRST, above every write: see `my_node`. A partitioned agent that keeps reconciling erases
    // the sweep's `NodeDead` on the very next tick, which is how `/v1` came to accept `start` on a
    // node the cluster reads as dead.
    // Every await between here and the first status write is timed: a restore sat 58 s in this
    // pass with nothing logged (2026-09-11, `reconcile.done ms=57995`, no `reconcile.slow`), and
    // the three timed steps below were not where the time went.
    let wsname = w.name_any();
    let me = super::timed("my_node", &wsname, my_node(ctx)).await;
    if me.dead {
        return Ok(Action::requeue(TICK));
    }
    let gen = w.meta().generation.unwrap_or(0);
    // BEFORE `heal_labels`, and before anything reads the spec: the label patch happens to reject
    // a `/` today, which is the only thing standing between `spec.owner` and a root-run
    // `mkdir`/`chown` under the pool root. Do not rely on a cosmetic call failing first.
    if let Err(why) = model::validate_ws_spec(&w.spec) {
        let prev = w.status.clone().unwrap_or_default();
        return settle(
            Outcome::Permanent(why, "InvalidSpec"),
            w,
            "Workspace",
            gen,
            // `patch_status` is a forced server-side apply: a field omitted here is PRUNED, so an
            // invalid spec would erase the placement memory that says where this workspace lives.
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
    let ws_api = Api::<crd::Workspace>::all(ctx.client.clone());
    super::timed("heal_labels", &wsname, async {
        heal_labels(&ws_api, w, &w.spec.owner, &w.spec.team, "workspace").await?;
        heal_attached_label(&ws_api, w).await
    })
    .await?;
    let mut prev = w.status.clone().unwrap_or_default();
    // Stopping is a home push and a pod delete — it needs neither the disk nor the namespace. Run
    // it BEFORE those gates: a workspace whose Volume failed permanently would otherwise be
    // unstoppable, stuck reporting `creating` with a pod still running on a broken subvolume.
    if w.spec.desired_state == DesiredState::Stopped {
        return stop_workspace(w, prev, gen, ctx).await;
    }
    let vol = match super::timed(
        "resolve_volume",
        &wsname,
        resolve_volume(w, &w.spec.owner, &w.spec.team, &w.spec.region, &w.spec.storage, &prev.node_name.clone(), &prev.conditions.clone(), gen, ctx),
    )
    .await?
    {
        Resolved::Ready(v) => *v,
        Resolved::Settled(a) => return Ok(a),
        // Unobserved on purpose on every wait: this generation has not converged, so the next pass
        // re-runs instead of treating a half-built workspace as done.
        Resolved::Wait { volume_ref, phase, cond, action } => {
            let st = crd::WorkspaceStatus {
                phase,
                observed_generation: None,
                volume_ref: volume_ref.or(prev.volume_ref.clone()),
                conditions: ws_conditions(&prev, cond),
                ..prev
            };
            write_ws_status(w, st, ctx).await?;
            return Ok(action);
        }
    };
    let id = vol.name_any();
    // Starts spread: the owner is alive (it is running this reconcile) and only the owner may give
    // a volume away, so this is the one place the decision can be made at all. Gated on the START
    // pass — this workspace's own status still says `Stopped` — because this function
    // is also the 15s requeue of every workspace on the node, and a cluster-wide sibling listing
    // per workspace per tick is traffic for a decision whose answer is already no. `Stopped` and
    // nothing else, matching the environment's: a workspace parked in `Creating` has no bytes
    // anywhere to spread toward. A listing that could not be completed moves nothing — an unseen
    // sibling may be a running pod.
    if super::timed("start_spread", &wsname, super::start_spread("Workspace", &w.name_any(), &id, &vol, prev.phase, ctx)).await?.is_some() {
        // Nothing left to do here: this object is unplaced now and the new node's claim watch
        // picks it up. Await the change rather than requeueing at an object that is no longer ours.
        return Ok(Action::await_change());
    }
    // The namespace is the OwnerBinding reconciler's to make; this one only waits for it. Creating
    // it here as well is how it ended up with two writers.
    //
    // ponytail: a binding becoming ready wakes a waiting workspace only via its 15s requeue —
    // mapping one binding to every waiting Workspace of that owner is a list per binding event, and
    // the wait is bounded by one tick. Wire a `spec.owner`-indexed reflector if first-workspace
    // latency ever shows up as a complaint.
    if !super::timed("namespace_ready", &wsname, binding::namespace_ready(ctx, &w.spec.region, &w.spec.owner, &w.spec.team)).await? {
        let st = crd::WorkspaceStatus {
            phase: crd::Phase::Creating,
            observed_generation: None,
            volume_ref: Some(id),
            conditions: ws_conditions(
                &prev,
                crd::condition(binding::NAMESPACE_READY, false, "NamespaceNotReady", "waiting for the owner's namespace", gen),
            ),
            ..prev
        };
        write_ws_status(w, st, ctx).await?;
        return Ok(Action::requeue(TICK));
    }
    // The shared home replaces the home Volume (spec 2026-09-01): the agent makes the two mount
    // sources exist before kubelet needs them. `{pool}/homes/{owner}` is NFS — mkdir is the whole
    // materialize. The cache subvolume is local and disposable. Both idempotent, so every reconcile
    // may call them. No WS_HOMES_EXPORT on this node: park, fail closed — a pod started anyway
    // would hostPath an empty local dir and the person's dotfiles would silently not be theirs.
    let Some(export) = ctx.homes_export.as_deref() else {
        let st = crd::WorkspaceStatus {
            phase: crd::Phase::Creating,
            observed_generation: None,
            volume_ref: Some(id),
            conditions: ws_conditions(&prev, crd::condition("Ready", false, "HomeNotReady", "this node has no shared-home mount (WS_HOMES_EXPORT)", gen)),
            ..prev
        };
        write_ws_status(w, st, ctx).await?;
        return Ok(Action::requeue(TICK));
    };
    // `spawn_blocking`, exactly as the `ensure_homecache` call below: `mount_homes` runs
    // `timeout -s KILL 5 ls`, `umount -f -l` and `timeout -s KILL 60 nsenter … mount`, all
    // synchronous — up to ~65 s of a reactor thread that every other workspace on this node shares.
    let (pool, export_owned, owner) = (ctx.pool.clone(), export.to_string(), w.spec.owner.clone());
    super::timed("shared_home", &id, tokio::task::spawn_blocking(move || ensure_shared_home(&pool, &export_owned, &owner, k8s::SSH_UID as u32)))
        .await
        .map_err(|e| ReconcileErr(e.to_string()))?
        .map_err(ReconcileErr)?;
    let (engine, owner) = (ctx.engine.clone(), w.spec.owner.clone());
    super::timed("homecache", &id, tokio::task::spawn_blocking(move || engine.ensure_homecache(&owner, k8s::SSH_UID as u32)))
        .await
        .map_err(|e| ReconcileErr(e.to_string()))?
        .map_err(|e| ReconcileErr(e.0))?;

    // Who may ssh in arrives as `OwnerKeys`, rendered to this node by `controller::keys`. The pod
    // mounts that file as a `type: File` hostPath, so starting one before it exists is a pod the
    // kubelet refuses with an opaque mount error; park until the projection has reached this node.
    // Only the default image mounts it at all — a user's own image gets no sshd and no keys volume,
    // and parking one on a file it never reads would be a workspace that never starts.
    // Only where a pod is about to be started (none recorded yet: a first start, or after a
    // stop) or the file is missing — not on every pass, which was a GET and a status write per
    // reconcile per node.
    if kloudlite_workspaces::model::is_default_image(&w.spec.image)
        && (prev.pod_ref.is_none() || !std::path::Path::new(&k8s::keys_file(&ctx.pool, k8s::keys_owner(&w.spec))).exists())
    {
        super::timed("keys", &id, super::keys::converge_owner(ctx, k8s::keys_owner(&w.spec))).await;
    }
    if kloudlite_workspaces::model::is_default_image(&w.spec.image)
        && !std::path::Path::new(&k8s::keys_file(&ctx.pool, k8s::keys_owner(&w.spec))).exists()
    {
        let st = crd::WorkspaceStatus {
            phase: crd::Phase::Creating,
            observed_generation: None,
            volume_ref: Some(id),
            conditions: ws_conditions(
                &prev,
                crd::condition("Ready", false, "KeysNotReady", "this owner's keys projection has not reached this node yet", gen),
            ),
            ..prev
        };
        write_ws_status(w, st, ctx).await?;
        return Ok(Action::requeue(TICK));
    }

    // Snapshot-model worktree materialization: a workspace just claimed onto this node (or one
    // whose pod was never started here) has no `live/{id}` subvolume yet. `head` is `None` on a
    // brand-new workspace (bootstrap: an empty worktree) — Task 4 never WRITES `status.head`
    // itself, only preserves whatever is already there via `..prev`; the first writers are Task 5
    // (a snapshot records the new head) and Task 6 (a clone/restore grafts one on). Until one of
    // those lands, `head == None` is ambiguous between "genuinely bootstrap" and "this workspace's
    // own head just has not been recorded yet" — the guard below tells the two apart the same way
    // the claim itself does, by asking whether the VOLUME has any snapshots at all.
    // Lazy per-volume migration, resolve the effective head, checkout and quota — identical for a
    // Workspace and an Environment down to the guard conditions; see `worktree_gate`.
    let gate = super::worktree_gate(
        &w.name_any(),
        "Workspace",
        &vol,
        &w.spec.storage,
        prev.head.as_deref(),
        &w.spec.owner,
        owner_ref_of_kind(w)?,
        crd::SnapshotState::of_workspace(w),
        ctx,
    )
    .await?;
    match gate {
        super::WorktreeGate::Wait { reason: "NoSuchSnapshot", message, .. } => {
            // Permanent: only the caller can settle it, since `settle` needs the object itself.
            let prev = prev.clone();
            let vref = id.clone();
            return settle(
                Outcome::Permanent(message, "NoSuchSnapshot"),
                w,
                "Workspace",
                gen,
                move |cond| {
                    serde_json::json!({
                        "phase": crd::Phase::Error,
                        "volumeRef": vref,
                        "conditions": ws_conditions(&prev, cond),
                    })
                },
                ctx,
            )
            .await;
        }
        super::WorktreeGate::Wait { reason, message, action } => {
            let st = crd::WorkspaceStatus {
                phase: crd::Phase::Creating,
                observed_generation: None,
                volume_ref: Some(id.clone()),
                conditions: ws_conditions(&prev, crd::condition("Ready", false, reason, &message, gen)),
                ..prev
            };
            write_ws_status(w, st, ctx).await?;
            return Ok(action);
        }
        super::WorktreeGate::Ready => {}
    }
    // First graft: this pass checked out the clone's snapshot, and nothing else will ever write
    // it as `head` (a clone never gets Task 5's push-time `advance_head` unless it pushes
    // itself) — the preserve pattern, same as `snapshot::advance_head`.
    if prev.head.is_none() {
        if let Some(commit) = super::clone_commit(&w.spec.storage) {
            let prev2 = prev.clone();
            write_ws_status(w, crd::WorkspaceStatus { head: Some(commit.to_string()), ..prev2 }, ctx).await?;
            prev.head = Some(commit.to_string());
        }
    }

    let ns = crd::ws_namespace(&w.spec.owner, &w.spec.team);
    let owner_ref = owner_ref_of_kind(w)?;
    let pod_ctx = k8s::PodContext {
        pool: &ctx.pool,
        node_name: &vol.spec.node_name,
        owner_ref: owner_ref.clone(),
        runtime_class: ctx.runtime_class.as_deref(),
        default_image: &ctx.default_image,
        // A workspace pod is never the builder — that shape exists only on an Environment.
        system: None,
        registry_host: &ctx.registry_host,
    };
    // Resolve the attachment before writing anything: a missing or cross-region environment is
    // reported and treated as unattached, never as a half-applied grant.
    let (env, refusal) = match w.spec.attached_environment.as_deref() {
        None => (None, None),
        Some(env_id) => match Api::<crd::Environment>::all(ctx.client.clone()).get_opt(env_id).await? {
            None => (None, Some(("EnvironmentNotFound", format!("environment {env_id} is gone")))),
            // A different region is a different cluster: there is no route and no DNS to grant.
            Some(e) if e.spec.region != w.spec.region => {
                (None, Some(("RegionMismatch", format!("environment {env_id} is in {}", e.spec.region))))
            }
            Some(e) => (Some((crd::env_namespace(env_id), e)), None),
        },
    };
    let env_ns = env.as_ref().map(|(ns, _)| ns.clone());
    // Per-WORKSPACE, like the pod that mounts it: `id` is the shared VOLUME for a clone, and
    // writing this file under the volume's name leaves every clone's pod stuck FailedMount on a
    // resolv.conf that does not exist under its own name.
    // Same rule: `create_dir_all` + `read_to_string` + `write`, on the shared home's NFS mount in
    // the worst case, on every workspace pass.
    let (pool, ws_id, ns_owned, env_ns_owned) =
        (ctx.pool.clone(), w.name_any(), ns.clone(), env_ns.clone());
    tokio::task::spawn_blocking(move || write_resolv_conf(&pool, &ws_id, &ns_owned, env_ns_owned.as_deref()))
        .await
        .map_err(|e| ReconcileErr(e.to_string()))??;
    let policies: Api<NetworkPolicy> = Api::namespaced(ctx.client.clone(), &ns);
    match &env {
        Some((env_ns, e)) => {
            ensure(&policies, &k8s::attach_egress(&ns, &w.name_any(), env_ns, &w.spec.owner, &pod_ctx.owner_ref), ctx).await?;
            // The environment-side half cannot be owned by this Workspace: an ownerReference may
            // not cross namespaces. It is owned by the ENVIRONMENT instead, so deleting the
            // environment collects it, and a detach deletes it by name.
            let env_ref = owner_ref_of_kind(e)?;
            let in_env: Api<NetworkPolicy> = Api::namespaced(ctx.client.clone(), env_ns);
            // `ws_id`: these policies select the workspace POD by `WORKSPACE_LABEL`, which names
            // the workspace, and siblings share the namespace — keyed by the shared volume a
            // clone's grant would select its source's pod instead of its own.
            ensure(&in_env, &k8s::attach_ingress(env_ns, &ns, &w.name_any(), &w.spec.owner, &env_ref), ctx).await?;
        }
        // Detach is this same pass with the field cleared, so the workspace-side half goes by name
        // — but ONLY when an attachment was ever recorded (2026-09-12). Every workspace that has
        // never been attached issued this DELETE on every single reconcile, and there has never
        // been anything there to delete. The condition is the same record the re-attach cleanup
        // below reads, and it survives a detach (it goes False, it is not removed).
        None if prev.conditions.iter().any(|c| c.type_ == crd::ATTACHED) => {
            delete_ignoring_404(&policies, &k8s::attach_policy_name(&w.name_any())).await?
        }
        None => {}
    }
    // The environment-side half lives in a namespace this spec no longer names, so a detach — or a
    // re-attach to a DIFFERENT environment — would strand it there until that environment is
    // deleted. Which namespace it was in is not lost: the previous pass wrote the environment id
    // into the `Attached` condition's message, and that is where it is read back from. A grant left
    // behind is dormant only until something re-adds an egress with the same workspace id.
    //
    // ponytail: a True condition is the only address kept, so an attach that created the ingress
    // and then died before its status write leaves no record and this pass collects nothing. The
    // environment's own delete collects it; upgrade path is a label on the ingress and a
    // list-by-label sweep in the janitor, if that window ever costs anything.
    let now = env.as_ref().map(|_| w.spec.attached_environment.as_deref().unwrap_or(""));
    let was = prev
        .conditions
        .iter()
        .find(|c| c.type_ == crd::ATTACHED && c.status == "True")
        .map(|c| c.message.clone())
        .filter(|was| now != Some(was.as_str()));
    if let Some(was) = was {
        let old: Api<NetworkPolicy> = Api::namespaced(ctx.client.clone(), &crd::env_namespace(&was));
        delete_ignoring_404(&old, &k8s::attach_policy_name(&w.name_any())).await?;
    }
    let mut attached = match (&env_ns, &refusal) {
        // The message is the BARE environment id and must stay that: the next pass parses it back
        // out of status to find a grant left in an environment this spec no longer names.
        (Some(_), _) => {
            Some(crd::condition(crd::ATTACHED, true, "Converged", w.spec.attached_environment.as_deref().unwrap_or(""), gen))
        }
        (None, Some((reason, msg))) => Some(crd::condition(crd::ATTACHED, false, reason, msg, gen)),
        // Not attached at all says nothing: an absent condition, not a False one.
        (None, None) => None,
    };
    // Before the pod, never after: a container started on a stale profile is a workspace whose
    // tools silently disagree with its spec.
    if w.spec.desired_state == DesiredState::Running {
        // Per-WORKSPACE, matching the pod's `var/kloudlite/profiles/{workspace}` subPath: packages are
        // `spec.packages` of THIS workspace, and two clones of one volume may ask for different
        // ones. Keyed by the shared volume, a clone mounts a profile that was never built for it.
        if let Some(action) = ensure_profile(w, &w.name_any(), gen, &mut prev, ctx).await? {
            return Ok(action);
        }
        // Same rule as the profile: the pod mounts this, so it exists first or sshd dies on boot.
        // Same rule: the host key is this WORKSPACE's identity (pinned in the user's known_hosts),
        // and the pod mounts `ws-ssh-{workspace}`. Keying it by the shared volume would give every
        // clone of one volume the same host key AND leave the pod's secret mount unresolvable.
        ensure_ssh(w, &w.name_any(), &ns, &owner_ref, &mut prev, ctx).await?;
    }

    let pods: Api<Pod> = Api::namespaced(ctx.client.clone(), &ns);
    // Reality, not intent: a pod created before this feature shipped has no `attach` volume and no
    // `/etc/resolv.conf` mount, and `create_if_absent` never replaces it — so the file this pass
    // wrote and both policies it applied resolve nothing at all. Reporting `Attached=True` there is
    // a lie the user cannot see through, so the live pod decides. An absent pod is not a refusal:
    // the one created below carries the mount.
    if env_ns.is_some() && !pod_carries_the_attach_mount(&pods, &w.name_any()).await? {
        attached = Some(crd::condition(
            crd::ATTACHED,
            false,
            "PodPredatesAttachment",
            "this pod was created before attachment existed and has no resolv.conf mount; stop and start the workspace once",
            gen,
        ));
    }
    let (phase, pod_ref) = match w.spec.desired_state {
        DesiredState::Running => {
            // The seed rides on the VOLUME's source: what the disk was asked to be made from is
            // the one place that answers "does this need cloning", legacy objects included.
            let init = match vol.spec.source.as_ref() {
                None => None,
                Some(s) => {
                    match k8s::git_init_container(s, &ctx.git_init_image, &ctx.git_ssh_host, &ctx.git_ssh_port) {
                        Ok(c) => c,
                        // A name that can never be cloned is permanent, and no pod is started for
                        // it: the alternative is a pod whose init container fails forever.
                        Err(why) => {
                            let prev = prev.clone();
                            return settle(
                                Outcome::Permanent(why, "InvalidSource"),
                                w,
                                "Workspace",
                                gen,
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
                    }
                }
            };
            let pod = match k8s::workspace_pod(&w.spec, &id, &w.name_any(), &pod_ctx, init) {
                Ok(p) => p,
                // Unreachable while `validate_ws_spec` runs at the top of this function; kept
                // because the builder is the boundary and must be able to say no on its own.
                Err(why) => {
                    let prev = prev.clone();
                    return settle(
                        Outcome::Permanent(why, "InvalidName"),
                        w,
                        "Workspace",
                        gen,
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
            };
            // The capacity gate for the START, as against `claim`'s for the PLACEMENT. Only when
            // the pod does not exist yet: a running pod's capacity is already spent, and refusing
            // here would tear down nothing while parking a healthy workspace at `Creating`.
            // A `Pending` pod nobody can explain is the failure this removes — the workspace now
            // says the node is full, and stays claimable the moment room appears.
            let pod_name = w.name_any();
            if pods.get_opt(&pod_name).await?.is_none()
                && !crate::claim::room_to_start(ctx, &pod_name, crate::claim::workspace_want(&w.spec.resources)).await?
            {
                let st = crd::WorkspaceStatus {
                    phase: crd::Phase::Creating,
                    observed_generation: None,
                    volume_ref: Some(id.clone()),
                    pod_ref: None,
                    conditions: super::with_drain_notice(
                        &prev.conditions,
                        replaced(
                            &with_attached(
                                ws_conditions(
                                    &prev,
                                    crd::condition(
                                        "Ready",
                                        false,
                                        "NoCapacity",
                                        &format!("{} has no room for this workspace right now; it starts as soon as room frees up", ctx.node),
                                        gen,
                                    ),
                                ),
                                attached.clone(),
                            ),
                            running_condition(&prev.conditions, gen),
                        ),
                        me.decommissioning,
                        gen,
                    ),
                    ..prev
                };
                write_ws_status(w, st, ctx).await?;
                return Ok(Action::requeue(TICK));
            }
            create_if_absent(&pods, &pod).await?;
            // Applying a pod is not a pod running. Read it back: a pod can sit Pending on an
            // unschedulable node or CrashLoopBackOff on a bad image, and reporting Ready straight
            // from the apply made a broken workspace indistinguishable from a working one.
            // The pod is named after the WORKSPACE (`k8s::workspace_pod`'s doc): for a shared-volume
            // clone `id` is the source VOLUME, and reading readiness or reporting `podRef` by `id`
            // would point this workspace at its source's pod — the gateway dials `podRef`, so an
            // ssh to the clone would land in the source's shell.
            let observed = pods.get_opt(&pod_name).await?;
            if !observed.as_ref().is_some_and(|p| own_ready_pod(p, &ctx.node)) {
                // A clone that keeps failing is the one pod fault a person can act on (a key not
                // yet authorised, a repo this key may not read), and a bare `PodNotReady` hid it
                // behind a workspace that simply never came up.
                let (reason, message) = observed
                    .as_ref()
                    .and_then(seed_failure)
                    .unwrap_or_else(|| ("PodNotReady".to_string(), "pod is not ready yet".to_string()));
                let st = crd::WorkspaceStatus {
                    phase: crd::Phase::Creating,
                    observed_generation: None,
                    volume_ref: Some(id.clone()),
                    pod_ref: Some(format!("{ns}/{pod_name}")),
                    // `Replicated=False/Running` goes in the SAME write that records the pod:
                    // from the moment a pod exists here, no other node is an option whatever the
                    // copies hold, and a stale `True` left over from the last stop is exactly the
                    // answer placement must never read.
                    conditions: super::with_drain_notice(
                        &prev.conditions,
                        replaced(
                            &with_attached(
                                ws_conditions(&prev, crd::condition("Ready", false, &reason, &message, gen)),
                                attached.clone(),
                            ),
                            running_condition(&prev.conditions, gen),
                        ),
                        me.decommissioning,
                        gen,
                    ),
                    ..prev
                };
                write_ws_status(w, st, ctx).await?;
                return Ok(Action::requeue(TICK));
            }
            // `ready`, not `running`: this string is deserialized into `model::WsState` by the
            // `/v1` projection, which spells the running state `Ready`. An unknown phase does not
            // error — it falls back to `Creating`, so a healthy workspace showed "Creating" in the
            // UI forever. `phase_names_the_doc_enum` pins the vocabulary.
            (crd::Phase::Ready, Some(format!("{ns}/{pod_name}")))
        }
        // Handled at the top of this function, before the Volume and namespace gates — stopping IS
        // deleting the pod, and it must not depend on either being healthy.
        DesiredState::Stopped => unreachable!("stopped is handled before the gates"),
    };
    // Same rule as the `PodNotReady` write above: a running workspace is `Replicated=False/Running`
    // for as long as it runs, and `None` for the paths that record no pod at all.
    let conditions = with_attached(
        ws_conditions(&prev, crd::condition("Ready", true, "Converged", "workspace matches spec", gen)),
        attached,
    );
    let conditions = match pod_ref {
        Some(_) => replaced(&conditions, running_condition(&prev.conditions, gen)),
        None => conditions,
    };
    // F7: the drain notice, on the running workspace's own status. The decommission beat used to
    // write it and this very rewrite erased it 15 s later, so the node annotation said `running=1`
    // while the workspace it was waiting on carried nothing at all.
    let conditions = super::with_drain_notice(&prev.conditions, conditions, me.decommissioning, gen);
    let st = crd::WorkspaceStatus {
        phase,
        observed_generation: Some(gen),
        volume_ref: Some(id),
        pod_ref,
        conditions,
        ..prev
    };
    write_ws_status(w, st, ctx).await?;
    Ok(Action::await_change())
}
