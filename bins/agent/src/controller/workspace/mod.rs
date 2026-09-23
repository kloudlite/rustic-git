//! The `Workspace` reconciler: profile, host key, worktree, attachment and the one pod. Split out
//! of `controller.rs` unchanged.
//!
//! The module map: `profile` (nix packages), `conditions`, `replicas`, `seed` (the git seed
//! container), `bench` (the second container's verdicts), `lifecycle` (stop, delete, migrate),
//! `status` (the status write, labels,
//! resolv.conf, host key). `apply_workspace` and the two reconcile entry points stay here.
//!
//! A BENCH is a Workspace with `spec.bench` set and nothing else special: same volume, same home,
//! same keys, same pod — plus a `bench` container and an idle clock. `crd::wants_pod` is what the
//! pod decision asks instead of `desiredState == Running`, and it is false for a bench that is
//! asleep or paused.

use super::stop::{replicated_condition, running_condition, stop_name, stop_push, StopPush};
use super::{my_node, conditions_eq, create_if_absent, delete_ignoring_404, heal_labels, owner_ref_of_kind, resolve_volume, settle, stopped_condition, wake_on_finish, write_status, Ctx, Done, Outcome, ReconcileErr, Resolved, RETRY, TICK};
use crate::binding;
use std::time::Duration;
use k8s_openapi::api::core::v1::Pod;
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

mod seed;
mod bench;
mod trees;
pub use trees::{tree_actions, TreeAction};
use bench::{bench_verdict_action, park_bench};
pub(crate) use profile::*;
pub use conditions::*;
pub(crate) use replicas::*;
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
    // A bench that is asleep or paused: the pod goes, nothing else does. Deliberately NOT
    // `stop_workspace` — an idle bench wakes on the next connection, and cutting a stop snapshot
    // every idle cycle would cost one every `benchIdleSecs` for a workspace nobody stopped. The
    // sync beat already keeps a recent cut for the replicas. Above the volume and namespace gates
    // for the same reason a stop is: removing a pod must not depend on either being healthy.
    if !crd::wants_pod(w) {
        return park_bench(w, prev, gen, ctx).await;
    }
    let vol = match super::timed(
        "resolve_volume",
        &wsname,
        resolve_volume(w, &w.spec.owner, &w.spec.team, &w.spec.region, &w.spec.storage, &prev.node_name.clone(), &prev.conditions.clone(), gen, crd::DEFAULT_REPLICAS, ctx),
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
        api_url: &ctx.api_url,
        git_ssh_host: &ctx.git_ssh_host,
        git_ssh_port: &ctx.git_ssh_port,
        shell_image: &ctx.shell_image,
    };
    // The space's environment, resolved and converged before the pod: resolv.conf in place, the
    // namespace-level grant, the legacy per-pod grant collected (`controller::space`).
    let space = super::space::converge_space(
        ctx,
        super::space::Pod {
            owner_ref: owner_ref.clone(),
            id: &w.name_any(),
            owner: &w.spec.owner,
            team: &w.spec.team,
            region: &w.spec.region,
            field: crd::retired_attach(w.meta(), w.spec.attached_environment.as_deref()),
            prev: &prev.conditions,
            gen,
        },
    )
    .await?;
    let env_ns = space.env_ns();
    let mut attached = match space {
        // Unknown cache: whatever the last converged pass recorded stands.
        super::space::Attached::Keep => prev.conditions.iter().find(|c| c.type_ == crd::ATTACHED).cloned(),
        super::space::Attached::Set(c) => c,
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
        if let Some(action) = ensure_ssh(w, &w.name_any(), &ns, &owner_ref, &mut prev, ctx).await? {
            return Ok(action);
        }
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
    // The third member is the tree rows: the Running arm is the only one that looks at the disk,
    // and an arm that did not must never claim a tree is gone, so every other one hands back what
    // status already carried.
    let (phase, pod_ref, trees) = match w.spec.desired_state {
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
            // `Some` exactly when `is_bench`: the image is the agent's configured one (a bench
            // follows it on every start, so it is not a spec field) and `benchIdleSecs` is stamped
            // in at create like every other `Mark::Live` value, so a setting change never reaches
            // a session already running. `kompressUrl` likewise.
            let settings = ctx.settings.load();
            let bench = crd::is_bench(w).then(|| (ctx.bench_image.as_str(), settings.bench_idle_secs, settings.kompress_url.as_str()));
            let pod = match k8s::workspace_pod(&w.spec, &id, &w.name_any(), &pod_ctx, init, bench) {
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
            // Before the ordinary readiness read: a bench reports idleness and a held folder
            // through its own container, and both look like "the pod is not ready" from here.
            if let (true, Some(p)) = (crd::is_bench(w), observed.as_ref()) {
                if let Some(action) = bench_verdict_action(w, p, &pods, &pod_name, &ns, gen, &prev, ctx).await? {
                    return Ok(action);
                }
            }
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
            // The tree step runs HERE and nowhere earlier: a tree is a snapshot of a live working
            // directory, so there is nothing to cut until the pod that owns it is up. Its rows go
            // into the same status write as the phase below.
            let trees = trees::reconcile_trees(w, &id, ctx).await?;
            // `ready`, not `running`: this string is deserialized into `model::WsState` by the
            // `/v1` projection, which spells the running state `Ready`. An unknown phase does not
            // error — it falls back to `Creating`, so a healthy workspace showed "Creating" in the
            // UI forever. `phase_names_the_doc_enum` pins the vocabulary.
            (crd::Phase::Ready, Some(format!("{ns}/{pod_name}")), trees)
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
        trees,
        ..prev
    };
    write_ws_status(w, st, ctx).await?;
    Ok(Action::await_change())
}
