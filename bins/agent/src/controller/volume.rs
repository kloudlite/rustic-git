//! The `Volume` reconciler: the one CRD whose node lives in SPEC, and the only place btrfs work is
//! started. `resolve_volume` and `ensure_child_volume` are the parents' shared entry point into it
//! — a workspace and an environment both own exactly one volume with identical semantics.
//! Split out of `controller.rs` unchanged.

use super::{
    conditions_eq, kept_conditions, my_node, replace_status, running_contains, settle, wake_on_finish, write_status, Ctx, Done,
    Outcome, ReconcileErr, Work, RETRY, TICK,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, OwnerReference};
use rustic_git_workspaces::k8s;
use kube::api::{Patch, PatchParams, PostParams};
use kube::runtime::controller::Action;
use kube::runtime::finalizer::{finalizer, Event as FinalizerEvent};
use kube::{Api, Resource, ResourceExt};
use rustic_git_workspaces::crd::{self, Phase, VolumeSource};
use rustic_git_workspaces::engine::Engine;
use std::sync::Arc;

/// Heal the listing labels from the spec.
///
/// `spec.owner` is the truth; `rustic-git.io/owner` is a VIEW of it that exists because label
/// selectors are indexed by every API server and an arbitrary spec field is not. Same rule the
/// registry states for its `index/` markers: a view for listings, never authorization, reconciled
/// by the owner.
///
/// Without this the view is only as good as whoever wrote the object. An object created by any
/// path that does not stamp the label — a restored backup, a migration, an operator with kubectl —
/// is owned correctly and yet invisible to `/v1`'s list forever, which makes "the CRD is the source
/// of truth" false in the one place a user would notice.
pub(crate) async fn heal_labels<K>(api: &Api<K>, obj: &K, owner: &str, team: &str, kind: &str) -> Result<(), ReconcileErr>
where
    K: Resource + Clone + serde::Serialize + serde::de::DeserializeOwned + std::fmt::Debug,
    K::DynamicType: Default,
{
    let cur = obj.meta().labels.as_ref();
    let ok = |k: &str, v: &str| cur.and_then(|l| l.get(k)).map(String::as_str) == Some(v);
    if ok(k8s::OWNER_LABEL, owner) && ok(k8s::KIND_LABEL, kind) && ok(k8s::TEAM_LABEL, team) {
        return Ok(());
    }
    let patch = serde_json::json!({
        "metadata": { "labels": { k8s::OWNER_LABEL: owner, k8s::KIND_LABEL: kind, k8s::TEAM_LABEL: team } }
    });
    api.patch(&obj.name_any(), &PatchParams::default(), &Patch::Merge(&patch)).await?;
    tracing::info!(name = %obj.name_any(), %owner, "healed listing labels from spec");
    Ok(())
}

pub fn owner_ref_of_kind<K: Resource<DynamicType = ()>>(obj: &K) -> Result<OwnerReference, ReconcileErr> {
    obj.controller_owner_ref(&()).ok_or_else(|| ReconcileErr("object has no uid".into()))
}

pub(crate) async fn reconcile_volume(v: Arc<crd::Volume>, ctx: Arc<Ctx>) -> Result<Action, ReconcileErr> {
    let api: Api<crd::Volume> = Api::all(ctx.client.clone());
    finalizer(&api, crd::SUBVOLUME_FINALIZER, v, |event| async {
        match event {
            // Deleting a Volume blocks until cleanup_local has run: containers gone first (GC via
            // ownerReferences), then the subvolume, then the object disappears. That ordering is
            // what makes audit H5 (a deleted workspace resurrected by an in-flight job)
            // unexpressible rather than patched.
            FinalizerEvent::Cleanup(v) => cleanup_volume(&v, &ctx).await,
            FinalizerEvent::Apply(v) => apply_volume(&v, &ctx).await,
        }
    })
    .await
    .map_err(|e| ReconcileErr(e.to_string()))
}

pub async fn apply_volume(v: &crd::Volume, ctx: &Arc<Ctx>) -> Result<Action, ReconcileErr> {
    // Above every write — see `my_node`. The `returned` re-run below is exactly the pass that
    // rewrote the sweep's `Available=False/NodeDead` away on a node that had not actually come back.
    if my_node(ctx).await.dead {
        return Ok(Action::requeue(TICK));
    }
    let gen = v.meta().generation.unwrap_or(0);
    let uid = v.uid().unwrap_or_default();
    let observed = v.status.as_ref().and_then(|s| s.observed_generation) == Some(gen);
    let restored_to = v.status.as_ref().and_then(|s| s.restored_to.clone());
    let restored_at = v.status.as_ref().and_then(|s| s.restore_requested_at.clone());
    // A wish already granted is not a wish. The PAIR, never the snapshot id alone: restoring the
    // same snapshot twice is a legitimate thing to ask for, and comparing ids alone makes the
    // second ask a silent no-op. Same guard the parent applies, deliberately — the parent decides
    // when it is SAFE to restore, this decides whether there is anything to restore, and neither
    // trusts the other to have checked.
    let restore = v.spec.restore_to.clone().filter(|w| !crd::wish_granted(w, restored_to.as_deref(), restored_at.as_deref()));

    // A pass that stamped `observedGeneration` and then lost its process before writing the
    // terminal status (an agent roll mid-operation) leaves `Working` behind with nothing left to
    // re-run it: `observed` is true, so step 1 below would `await_change` forever and the volume
    // stays Working while its data is perfectly fine. Falling through re-runs the pass, which
    // ends in the terminal write the lost one never made. `observed` itself stays TRUE on
    // purpose — it is what keeps `materialize` off, so this recovery re-applies the quota and
    // reports, and never re-runs a create or a clone against a volume that already exists.
    let stranded = v.status.as_ref().is_some_and(|s| s.phase == Phase::Working) && !running_contains(ctx, &uid);
    // The dead-node sweep marks a volume `Unavailable` with a STATUS write, which leaves
    // `observedGeneration` current. When the owner comes back, its volumes are exactly as they
    // were and nothing bumps the spec, so without this the pass below never re-runs and every
    // workspace on the node waits on "not materialized" forever (seen live, 2026-09-02). This
    // reconciler is field-selected on the pin, so reaching here already means the volume is ours.
    let returned = v.status.as_ref().is_some_and(|s| s.phase == Phase::Unavailable);

    // 1. Nothing asked for. Pushing is a `Snapshot` with its own reconciler now, so a
    //    materialized volume at its current generation has nothing left for this pass to do.
    if observed && !stranded && !returned && !running_contains(ctx, &uid) {
        return Ok(Action::await_change());
    }

    // 2. An operation for this volume exists: drain it if finished, otherwise let it run. A handle
    //    started for an OLDER generation is still drained here rather than abandoned — it holds the
    //    volume's flock, so starting a second one would block on it anyway.
    let (finished, still_running) = {
        let mut running = ctx.running.lock().unwrap_or_else(|p| p.into_inner());
        match running.get(&uid) {
            Some((_, h)) if h.is_finished() => (running.remove(&uid), false),
            Some(_) => (None, true),
            None => (None, false),
        }
    };
    if still_running {
        write_volume_status(v, progressing(v, gen), ctx).await?;
        return Ok(Action::requeue(TICK));
    }
    if let Some((started_gen, handle)) = finished {
        let outcome = handle.await.unwrap_or_else(|e| Err(format!("operation panicked: {e}")));
        return match outcome {
            Ok(done) => {
                let mut st = crd::VolumeStatus {
                    phase: done.phase,
                    restored_to: done.restored_to.clone().or(restored_to.clone()),
                    restore_requested_at: match &done.restored_to {
                        // Stamped together or not at all: a `restoredTo` without the wish that
                        // asked for it makes every later wish look already-granted.
                        Some(_) => v.spec.restore_to.as_ref().map(|w| w.requested_at.clone()),
                        None => restored_at.clone(),
                    },
                    // The generation the work actually ran for, not the current one: a spec edited
                    // mid-operation must not be reported as observed by an operation that never
                    // saw it. When they differ this leaves the object unobserved, so the next pass
                    // starts the new work — which is the intended behaviour.
                    observed_generation: Some(started_gen),
                    subvolume_present: true,
                    conditions: vec![],
                };
                st.conditions = vec![crd::condition("Ready", true, "Converged", "volume is materialized", gen)];
                if let Some(why) = &done.quota_unenforced {
                    st.conditions.push(crd::condition("QuotaEnforced", false, "QuotaUnavailable", why, gen));
                }
                write_volume_status(v, st, ctx).await?;
                Ok(Action::await_change())
            }
            // `observedGeneration` is deliberately NOT stamped: an unobserved generation is what
            // makes the next pass try again. Nothing is deleted, nothing is marked permanently
            // failed — the keep-biased rule, applied to the error path.
            //
            // Except for the three the engine names: a snapshot id with no record behind it, a
            // region this node holds no credentials for, and a blob the store says is absent or
            // forbidden (a timeout is the world's, and comes back unmarked). All
            // three are the spec's or the deploy's fault, not the world's — retrying them at RETRY
            // forever is the hot loop `check_source` exists to prevent, so they settle instead.
            Err(e) if permanent_reason(&e).is_some() => {
                let reason = permanent_reason(&e).unwrap();
                let present = ctx.engine.pool.live(&v.name_any()).exists();
                let restored = restored_to.clone();
                let restored_at_err = restored_at.clone();
                return settle(
                    Outcome::Permanent(e, reason),
                    v,
                    "Volume",
                    gen,
                    move |cond| {
                        serde_json::json!({
                            "phase": Phase::Error,
                            "subvolumePresent": present,
                            "restoredTo": restored,
                            "restoreRequestedAt": restored_at_err,
                            "conditions": [cond],
                        })
                    },
                    ctx,
                )
                .await;
            }
            Err(e) => {
                let st = crd::VolumeStatus {
                    phase: Phase::Error,
                    observed_generation: v.status.as_ref().and_then(|s| s.observed_generation),
                    restored_to: restored_to.clone(),
                    restore_requested_at: restored_at.clone(),
                    subvolume_present: ctx.engine.pool.live(&v.name_any()).exists(),
                    conditions: vec![crd::condition("Ready", false, "OperationFailed", &e, gen)],
                };
                write_volume_status(v, st, ctx).await?;
                Ok(Action::requeue(RETRY))
            }
        };
    }

    // 3. Start it, on its own OS thread (see the module doc for why), and observe it later.
    let engine = ctx.engine.clone();
    let id = v.name_any();
    let owner = v.spec.owner.clone();
    let source = v.spec.source.clone();
    // An in-place restore REPLACES the materialize step: re-running the original source's
    // materialize in the same pass would fetch a lineage this volume is about to stop having.
    let materialize = !observed && restore.is_none();
    let quota_gb = v.spec.quota_gb;
    let handle = tokio::task::spawn_blocking(move || {
        volume_work(&engine, Work { id, owner, source, materialize, restore, quota_gb })
    });
    let handle = wake_on_finish(
        handle,
        ctx.wake_volume.clone(),
        kube::runtime::reflector::ObjectRef::<crd::Volume>::new(&v.name_any()),
    );
    ctx.running.lock().unwrap_or_else(|p| p.into_inner()).insert(uid, (gen, handle));
    write_volume_status(v, progressing(v, gen), ctx).await?;
    Ok(Action::requeue(TICK))
}

/// The engine's named permanent failures, mapped to the condition `reason` a person reads in
/// `kubectl describe`. Anything else is transient and retried.
fn permanent_reason(e: &str) -> Option<&'static str> {
    use rustic_git_workspaces::engine::ops::NO_SUCH_RECORD;
    [(NO_SUCH_RECORD, "NoSuchSnapshot")]
        .into_iter()
        .find(|(marker, _)| e.contains(marker))
        .map(|(_, reason)| reason)
}

fn progressing(v: &crd::Volume, gen: i64) -> crd::VolumeStatus {
    let prev = v.status.clone().unwrap_or_default();
    crd::VolumeStatus {
        phase: Phase::Working,
        conditions: vec![crd::condition("Progressing", true, "Working", "btrfs operation in flight", gen)],
        ..prev
    }
}

fn volume_work(engine: &Engine, w: Work) -> Result<Done, String> {
    let Work { id, owner: _, source, materialize, restore, quota_gb } = w;
    let id = id.as_str();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
    rt.block_on(async {
        if materialize {
                match &source {
                    None => engine.create_subvol(id).map_err(|e| e.to_string())?,
                    Some(VolumeSource::CloneOf { volume, .. }) => {
                        engine.clone_local_ids(volume, id).await.map_err(|e| e.to_string())?
                    }
                    // The interrupted clone: bytes from a LOCAL read-only copy of the named cut,
                    // never from the source's `live` — the source's node is down, which is the
                    // whole reason this variant exists.
                    Some(VolumeSource::SeededFrom { volume, snapshot }) => {
                        engine.seed_from_snapshot(volume, snapshot, id).await.map_err(|e| e.to_string())?
                    }
                    // Empty, deliberately: a `GitRepo` volume is seeded by the workspace pod's
                    // INIT CONTAINER, inside the workspace, over SSH, as the owner. The agent no
                    // longer holds a credential that could clone on the user's behalf.
                    Some(VolumeSource::GitRepo { .. }) => engine.create_subvol(id).map_err(|e| e.to_string())?,
                }
        }
        // In place: the wish names a snapshot of THIS volume's own history — a checkout swapped
        // into the worktree that already carries this volume's own id (the API validated Ready +
        // same-volume before writing it). Never a registry fetch any more (Task 8): the old
        // staging-id fetch/swap path is gone with it.
        if let Some(w) = &restore {
            engine.swap_worktree(&w.volume, id, &w.snapshot_id).map_err(|e| e.to_string())?;
        }
        // After EVERY path that can leave a new `live` behind — create, clone, restore — and on a
        // plain quota edit too (a spec change is a new generation, which is a materialize pass
        // that finds `live` already there). Per subvolume, so a restore's fresh `live` would
        // otherwise come up uncapped.
        let quota_unenforced = engine.set_quota(id, quota_gb).map_err(|e| e.to_string())?;
        Ok(Done {
            phase: Phase::Ready,
            restored_to: restore.as_ref().map(|w| w.snapshot_id.clone()),
            quota_unenforced,
        })
    })
}

pub async fn cleanup_volume(v: &crd::Volume, ctx: &Arc<Ctx>) -> Result<Action, ReconcileErr> {
    // Reclaiming the subvolume while a `btrfs send` is still reading it destroys the source
    // mid-stream. The finalizer is what makes waiting safe: the object cannot disappear until this
    // returns, so a requeue costs a tick and the delete completes after the operation does.
    //
    // The finished handle must be DRAINED here, not merely observed: while an object is deleting
    // the finalizer routes every reconcile to this arm, so `apply_volume` never runs and nothing
    // else would ever remove the entry — the delete would requeue forever on its own leftovers.
    let uid = v.uid().unwrap_or_default();
    {
        let mut running = ctx.running.lock().unwrap_or_else(|p| p.into_inner());
        match running.get(&uid) {
            Some((_, h)) if h.is_finished() => {
                running.remove(&uid);
            }
            Some(_) => {
                tracing::info!(volume = %v.name_any(), "delete waiting for an in-flight operation");
                return Ok(Action::requeue(TICK));
            }
            None => {}
        }
    }
    let engine = ctx.engine.clone();
    let id = v.name_any();
    let profile_id = id.clone();
    let profiles = ctx.profiles_dir.clone();
    // ponytail: a profile build in flight is keyed `profile:{workspace uid}`, which this path does
    // not know, so a workspace deleted mid-build leaves one finished handle in `running` and one
    // hash in `profile_builds` until the process restarts. Bounded and harmless; drain both here
    // off the Volume's ownerReference if either map is ever seen growing.
    tokio::task::spawn_blocking(move || {
        crate::janitor::cleanup_local(&engine, &id);
        // A node that never built for this volume has no profile — and a `/nix` this pod cannot
        // see is not a reason to strand a delete behind its finalizer.
        if let Err(e) = crate::nix::remove_profile(&profiles, &profile_id) {
            tracing::warn!(volume = %profile_id, error = %e, "removing the nix profile");
        }
    })
        .await
        .map_err(|e| ReconcileErr(format!("cleanup panicked: {e}")))?;
    Ok(Action::await_change())
}

async fn write_volume_status(v: &crd::Volume, st: crd::VolumeStatus, ctx: &Arc<Ctx>) -> Result<(), ReconcileErr> {
    write_status(v, "Volume", v.status.as_ref(), &st, ctx, |a, b| {
        a.phase == b.phase
            && a.observed_generation == b.observed_generation
            && a.subvolume_present == b.subvolume_present
            && a.restored_to == b.restored_to
            && a.restore_requested_at == b.restore_requested_at
            && conditions_eq(&a.conditions, &b.conditions)
    })
    .await
}

/// Create a `Volume` child if it is missing, and hand back what the API server holds.
///
/// A parent's child takes the PARENT's name: the id is already the registry key, the host path
/// segment and the URL segment, and an ownerReference — not a name — is what makes it a child.
/// That ownerReference is also the whole delete story: `DELETE workspace` reclaims the disk with no
/// ordering logic anywhere in the API.
#[allow(clippy::too_many_arguments)]
pub async fn ensure_child_volume<P>(
    id: &str,
    parent: &P,
    owner: &str,
    team: &str,
    region: &str,
    storage: &crd::WorkspaceStorage,
    node: &str,
    kind: &str,
    ctx: &Arc<Ctx>,
) -> Result<crd::Volume, ReconcileErr>
where
    P: Resource<DynamicType = ()> + ResourceExt,
{
    let api: Api<crd::Volume> = Api::all(ctx.client.clone());
    if let Some(v) = api.get_opt(id).await? {
        return Ok(v);
    }
    let mut vol = crd::Volume::new(
        id,
        crd::VolumeSpec {
            owner: owner.to_string(),
            team: team.to_string(),
            // FROM `status.nodeName`, never recomputed. The mismatch guard in `apply_workspace` is
            // the belt to this brace: a Workspace never names a node its Volume does not, because
            // the Volume is authored here from that one field.
            node_name: node.to_string(),
            region: region.to_string(),
            quota_gb: storage.quota_gb,
            replicas: crd::DEFAULT_REPLICAS,
            source: storage.source.clone(),
            // A fresh child is materialized from `source`; an in-place restore is a later wish the
            // parent's gate writes once, and never part of a create.
            restore_to: None,
        },
    );
    vol.metadata.owner_references = Some(vec![owner_ref_of_kind(parent)?]);
    vol.metadata.labels = Some(std::collections::BTreeMap::from([
        (k8s::OWNER_LABEL.to_string(), owner.to_string()),
        (k8s::KIND_LABEL.to_string(), kind.to_string()),
        (k8s::TEAM_LABEL.to_string(), team.to_string()),
    ]));
    match api.create(&PostParams::default(), &vol).await {
        Ok(v) => Ok(v),
        // Lost a race with our own earlier pass. Read back what won.
        Err(kube::Error::Api(s)) if s.code == 409 => Ok(api.get(id).await?),
        Err(e) => Err(e.into()),
    }
}

/// Compare-and-set one JSON pointer on a Volume, atomically. `test` proves the value we decided
/// against is still there and `replace` writes the new one; the API server applies the pair as a
/// unit, so of two claimants exactly one sees 200 and the other a 409/422 it reads as "lost, not
/// broken" rather than as a failure. That reading is the safety property, which is why there is
/// one of these and not five.
///
/// `Ok(false)` is a lost race and the caller re-decides on its next pass. An `Err` is an outage —
/// never "lost": a caller that treated an unreachable API server as a lost race would silently
/// skip work forever.
pub(crate) async fn cas(
    api: &Api<crd::Volume>,
    name: &str,
    path: &str,
    from: serde_json::Value,
    to: serde_json::Value,
) -> Result<bool, kube::Error> {
    let pointer = || path.parse().expect("callers pass static pointers");
    let ops = json_patch::Patch(vec![
        json_patch::PatchOperation::Test(json_patch::TestOperation { path: pointer(), value: from }),
        json_patch::PatchOperation::Replace(json_patch::ReplaceOperation { path: pointer(), value: to }),
    ]);
    match api.patch(name, &PatchParams::default(), &Patch::Json::<crd::Volume>(ops)).await {
        Ok(_) => Ok(true),
        Err(kube::Error::Api(s)) if s.code == 422 || s.code == 409 => Ok(false),
        Err(e) => Err(e),
    }
}

/// Compare-and-set the owner pin from empty to `node`. The `test` op is what makes two claimants
/// safe: the API server applies the patch atomically, so exactly one of them sees 200 and the
/// other a 409/422 it treats as "lost, not broken" — same construction as `peer::release_dead_volumes`.
pub(crate) async fn take_volume(ctx: &Arc<Ctx>, name: &str, node: &str) -> Result<bool, kube::Error> {
    cas(&Api::all(ctx.client.clone()), name, "/spec/nodeName", serde_json::json!(""), serde_json::json!(node)).await
}

/// The mirror of `take_volume`: compare-and-set the owner pin from `owner` to empty. Same `test`
/// construction and the same "lost, not broken" reading of a 409/422 — a start that raced the
/// dead-node sweep just re-decides on its next pass.
pub(crate) async fn release_volume(ctx: &Arc<Ctx>, name: &str, owner: &str) -> Result<bool, kube::Error> {
    cas(&Api::all(ctx.client.clone()), name, "/spec/nodeName", serde_json::json!(owner), serde_json::json!("")).await
}

/// Remove `parent_uid`'s entry from the Volume's `ownerReferences` so Kubernetes GC stops seeing
/// the Volume as that parent's child. Called only when the Volume still holds a Ready snapshot: a
/// snapshot must outlive the workspace it was pushed from, and its bytes live on the Volume's
/// subvolume — so the Volume has to survive its parent's delete, detached, rather than being
/// collected with it. `ownerReferences` is METADATA, which is why the agent's admission policy
/// (spec-only) needs nothing for this and its existing `patch` on volumes is enough.
///
/// Guarded like `take_volume`: `test` on the list we read, so a concurrent writer's change turns
/// into `Ok(false)` — "lost, not broken" — and the finalizer simply requeues. An empty result is
/// written as an empty list, which IS the detached state (no owners), not an error.
pub(crate) async fn detach_volume(ctx: &Arc<Ctx>, name: &str, parent_uid: &str) -> Result<bool, kube::Error> {
    let api: Api<crd::Volume> = Api::all(ctx.client.clone());
    let current = api.get(name).await?.metadata.owner_references.unwrap_or_default();
    if !current.iter().any(|o| o.uid == parent_uid) {
        // Already detached (a requeue after a successful patch, or an object that never had us).
        return Ok(true);
    }
    let kept: Vec<_> = current.iter().filter(|o| o.uid != parent_uid).cloned().collect();
    cas(
        &api,
        name,
        "/metadata/ownerReferences",
        serde_json::to_value(&current).expect("owner references serialize"),
        serde_json::to_value(&kept).expect("owner references serialize"),
    )
    .await
}

/// The mirror of `detach_volume`: add `parent`'s entry to a Volume it did NOT create, so a
/// restored or cloned working copy is an owner of the volume it runs on (design rule 6). Without
/// it the working copy is kept alive only by the snapshots on that volume — delete the last one
/// and GC takes the subvolume its pod is running on.
///
/// Idempotent: an entry already present is `Ok(true)` on a bare GET. Guarded like `detach_volume`
/// — `test` on the list we read, so a concurrent writer is "lost, not broken" and the caller
/// requeues. `controller` is cleared because only ONE ownerReference may be the controller and
/// that one belongs to the parent that created the Volume. `ownerReferences` is metadata, so the
/// admission policy (spec-only) needs nothing and the existing `patch` on volumes is enough.
pub(crate) async fn attach_volume<K>(ctx: &Arc<Ctx>, name: &str, parent: &K) -> Result<bool, ReconcileErr>
where
    K: Resource<DynamicType = ()>,
{
    let mut mine = owner_ref_of_kind(parent)?;
    mine.controller = Some(false);
    mine.block_owner_deletion = Some(false);
    let api: Api<crd::Volume> = Api::all(ctx.client.clone());
    let current = api.get(name).await?.metadata.owner_references.unwrap_or_default();
    if current.iter().any(|o| o.uid == mine.uid) {
        return Ok(true);
    }
    let mut next = current.clone();
    next.push(mine);
    let path = "/metadata/ownerReferences";
    let value = serde_json::to_value(&next).expect("owner references serialize");
    // A DETACHED volume — the ordinary restore target — has no `ownerReferences` key at all
    // (the API server drops the empty list), and a `test` against `[]` would 422 forever on it.
    // `add` creates the key and is the whole patch there; the guarded form is for the case where
    // there is an existing list to lose.
    if current.is_empty() {
        let ops = json_patch::Patch(vec![json_patch::PatchOperation::Add(json_patch::AddOperation {
            path: path.parse().expect("static pointer parses"),
            value,
        })]);
        match api.patch(name, &PatchParams::default(), &Patch::Json::<crd::Volume>(ops)).await {
            Ok(_) => Ok(true),
            Err(kube::Error::Api(s)) if s.code == 422 || s.code == 409 => Ok(false),
            Err(e) => Err(e.into()),
        }
    } else {
        cas(&api, name, path, serde_json::to_value(&current).expect("owner references serialize"), value)
            .await
            .map_err(Into::into)
    }
}

/// Whether the child's disk actually exists. A parent acts on a child only by reading the child's
/// status, never by guessing — and "the object exists" is not "the subvolume exists". The symptom
/// this guards is a pod wedged forever on `path … does not exist`.
pub(crate) fn volume_is_ready(v: &crd::Volume) -> bool {
    v.status.as_ref().is_some_and(|s| s.phase == crd::Phase::Ready && s.subvolume_present)
}

/// The source references that can be wrong forever, checked ONCE before a Volume is created.
///
/// These never get better by being retried: a live `cloneOf` naming a workspace that does not
/// exist, a restore naming a `Volume` that does not exist, a
/// `cloneOf { commit }` whose snapshot id no `Ready` `Snapshot` carries. Without this branch each of
/// them requeues at `RETRY` forever, and the log line is indistinguishable from a registry outage.
async fn check_source(source: Option<&VolumeSource>, ctx: &Arc<Ctx>) -> Result<(), Outcome> {
    match source {
        None | Some(VolumeSource::GitRepo { .. }) => Ok(()),
        // Workspace THEN Environment: `clone_env` names an environment's id here, and checking only
        // the workspace kind settled every cloned environment as a permanent `NoSuchSource`.
        // `SeededFrom` alongside `CloneOf`: it names a source parent the same way, and a source
        // that does not exist is just as permanently wrong however the bytes would have been copied.
        // A restore (`CloneOf{commit: Some(_)}`) is the one case where the source WORKING COPY is
        // gone by design — restoring a snapshot of a deleted workspace is what durable snapshots
        // exist for. What must still exist is the detached `Volume` CR holding the bytes; checking
        // the parent kinds here parked every such restore on a permanent `NoSuchSource`.
        Some(VolumeSource::CloneOf { volume, commit: Some(_) }) => {
            let vols: Api<crd::Volume> = Api::all(ctx.client.clone());
            match vols.get_opt(volume).await {
                Ok(Some(_)) => Ok(()),
                Ok(None) => Err(Outcome::Permanent(format!("clone source {volume} does not exist"), "NoSuchSource")),
                Err(e) => Err(e.into()),
            }
        }
        Some(VolumeSource::CloneOf { volume, .. }) | Some(VolumeSource::SeededFrom { volume, .. }) => {
            let ws: Api<crd::Workspace> = Api::all(ctx.client.clone());
            if ws.get_opt(volume).await.map_err(Outcome::from)?.is_some() {
                return Ok(());
            }
            let envs: Api<crd::Environment> = Api::all(ctx.client.clone());
            match envs.get_opt(volume).await {
                Ok(Some(_)) => Ok(()),
                Ok(None) => Err(Outcome::Permanent(format!("clone source {volume} does not exist"), "NoSuchSource")),
                Err(e) => Err(e.into()),
            }
        }
    }
}

/// What a parent must do about its `Volume` child, decided once for both parent kinds.
pub(crate) enum Resolved {
    /// The disk exists. Carry on. Boxed only to keep the enum from being a `Volume` wide.
    Ready(Box<crd::Volume>),
    /// Not usable yet (or ever). The parent writes `phase` + `cond` into ITS OWN status struct —
    /// the two status types share no trait — and returns `action`.
    Wait { volume_ref: Option<String>, phase: crd::Phase, cond: Condition, action: Action },
    /// `settle` already wrote the status; the parent just returns.
    Settled(Action),
}

/// Resolve a parent's `Volume` child: author it, refuse a node disagreement, wait for the disk.
/// Shared by `apply_workspace` and `apply_environment` because a second copy of this is a second
/// place for the placement rules to drift.
///
/// `node_name` is the parent's STATUS field — placement is a fact the claim established, and
/// status is the only place it lives.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn resolve_volume<P>(
    parent: &P,
    owner: &str,
    team: &str,
    region: &str,
    storage: &Option<crd::WorkspaceStorage>,
    node_name: &str,
    // The parent's current conditions, so a settle here keeps the ones later passes read back.
    prev_conditions: &[Condition],
    gen: i64,
    ctx: &Arc<Ctx>,
) -> Result<Resolved, ReconcileErr>
where
    P: Resource<DynamicType = ()> + ResourceExt + Clone + serde::de::DeserializeOwned + std::fmt::Debug + serde::Serialize,
{
    let api_kind = P::kind(&()).to_string();
    // Before anything is created: a source that can never resolve is a permanent failure, and the
    // difference between "wrong forever" and "briefly unavailable" is what `settle` writes down.
    let outcome = match storage {
        Some(s) => check_source(s.source.as_ref(), ctx).await.err(),
        // No storage: nothing here can ever build a disk, and no retry adds a field.
        None => Some(Outcome::Permanent("spec.storage is required".into(), "NoStorage")),
    };
    if let Some(outcome) = outcome {
        let node = node_name.to_string();
        let kept = prev_conditions.to_vec();
        return Ok(Resolved::Settled(
            settle(
                outcome,
                parent,
                &api_kind,
                gen,
                move |cond| {
                    serde_json::json!({
                        "phase": crd::Phase::Error,
                        "nodeName": node,
                        // Terminal is not the end of the object: a detach after it still has to find
                        // the grant, so the kept conditions come through here too.
                        "conditions": kept_conditions(&kept, cond),
                    })
                },
                ctx,
            )
            .await?,
        ));
    }

    let s = storage.as_ref().expect("settled above");
    // Snapshot model: a `cloneOf` carrying a resolved snapshot is a second WORKTREE of the source's
    // OWN volume, not a new subvolume — `ensure_child_volume` (and the btrfs clone it would
    // trigger via `clone_local_ids`) is skipped entirely, and `volumeRef` ends up naming the
    // source's volume directly. `check_source` above already proved the source object exists;
    // this reads the Volume itself, which placement decided this parent's node over.
    let shared = match &s.source {
        Some(VolumeSource::CloneOf { volume, commit: Some(_) }) => {
            let vols: Api<crd::Volume> = Api::all(ctx.client.clone());
            Some(vols.get_opt(volume).await?)
        }
        _ => None,
    };
    let is_shared = shared.is_some();
    let vol = match shared {
        Some(Some(v)) => v,
        // The source volume vanished between claim and this pass — same shape as any other
        // "wait for the child to exist" case, not a permanent failure: `check_source` already
        // proved the source OBJECT exists, so a missing Volume is a materialize race, not a typo.
        Some(None) => {
            return Ok(Resolved::Wait {
                volume_ref: None,
                phase: crd::Phase::Creating,
                cond: crd::condition("Ready", false, "SourceVolumeNotReady", "waiting for the clone source's volume", gen),
                action: Action::requeue(TICK),
                });
        }
        None => {
            ensure_child_volume(&parent.name_any(), parent, owner, team, region, s, node_name, &api_kind.to_lowercase(), ctx)
                .await?
        }
    };
    let id = vol.name_any();
    // The belt to `ensure_child_volume`'s brace: two places allowed to name a node is two places
    // that can disagree about where the data is, and the failure mode is an owner's data split
    // across pools — so a disagreement refuses rather than picks.
    // An unowned volume is a dead node's, released by the unclaim sweep. This node claimed the
    // parent (so `may_claim` already proved its replica is Synced): take the pin. Losing the race
    // is not an error — the next pass meets the winner's pin and the guard below refuses as usual.
    if vol.spec.node_name.is_empty() {
        if take_volume(ctx, &id, node_name).await? {
            tracing::info!(volume = %id, node = %node_name, "took over an unowned volume");
        }
        return Ok(Resolved::Wait {
            volume_ref: None,
            phase: crd::Phase::Creating,
            cond: crd::condition("Ready", false, "VolumeTakeover", "taking ownership of the released volume", gen),
            action: Action::requeue(std::time::Duration::from_secs(5)),
        });
    }
    // Design rule 6: a working copy grafted onto someone ELSE's volume becomes an owner of it,
    // or only the snapshots keep it alive and deleting the last one collects the subvolume this
    // pod runs on. After the placement guards, not before: a pass that turns out to be on the
    // wrong node has no business rewriting the volume's owner list. A lost CAS just requeues.
    if is_shared && vol.spec.node_name == node_name && !attach_volume(ctx, &id, parent).await? {
        return Ok(Resolved::Wait {
            volume_ref: None,
            phase: crd::Phase::Creating,
            cond: crd::condition("Ready", false, "VolumeAttach", "attaching to the shared volume", gen),
            action: Action::requeue(std::time::Duration::from_secs(5)),
        });
    }
    if vol.spec.node_name != node_name {
        let why = format!("status.nodeName {node_name} disagrees with volume {id}'s node {}", vol.spec.node_name);
        // The owner is alive and is somebody else: I lost the takeover CAS (two up-to-date nodes
        // raced for a released volume, and exactly one won). Clear MY OWN claim and requeue, so
        // the winner's reconcile picks the parent up. Without this the loser sits in `error`
        // forever holding a `status.nodeName` nobody will ever clear — the object is neither
        // placed nor unplaced, so no claim watch matches it.
        //
        // Only for a LIVE owner. A dead owner's volume is released by the per-volume sweep and by
        // nothing else: it is the only code that knows whether a Running sibling still pins it. A
        // failed Node read keeps today's refuse-and-wait — un-placing on a maybe-dead owner would
        // be a second thing allowed to release a volume.
        //
        // `node_is_dead`, NOT `unplaceable`: a DECOMMISSIONING owner is alive and still draining,
        // so it will reclaim this parent. Reading it as not-alive skips the self-heal and parks the
        // loser in `Error`/`Degraded=NodeMismatch` under `await_change()` — forever, since nothing
        // else ever touches that claim. `claim.rs` keeps `unplaceable`: nothing NEW may land here.
        let owner_node = Api::<k8s_openapi::api::core::v1::Node>::all(ctx.client.clone()).get_opt(&vol.spec.node_name).await;
        let alive = match &owner_node {
            Ok(n) => !crate::peer::node_is_dead(n.as_ref(), crate::peer::node_dead_secs(), k8s_openapi::jiff::Timestamp::now()),
            Err(_) => false,
        };
        if alive {
            tracing::info!(volume = %id, owner = %vol.spec.node_name, "lost the volume; un-placing myself so the owner reclaims it");
            // GUARDED, exactly like the claim itself (`claim.rs:209-226`): this write races the
            // real owner's own claim of the same parent, and a forced apply (`write_ws_status`/
            // `patch_status`) would let this un-place silently clobber a claim node-b just made.
            // Merge onto the object AS FETCHED so nothing but nodeName/phase/conditions moves —
            // `volumeRef`/`head`/every other status field rides along untouched.
            let mut status = serde_json::to_value(parent).map_err(|e| ReconcileErr(e.to_string()))?["status"].take();
            if status.is_null() {
                status = serde_json::json!({});
            }
            if let Some(dst) = status.as_object_mut() {
                dst.insert("nodeName".into(), serde_json::json!(""));
                dst.insert("phase".into(), serde_json::json!(crd::Phase::Pending));
                dst.insert("conditions".into(), serde_json::json!(kept_conditions(prev_conditions, crd::condition("Placed", false, "NodeMismatch", &why, gen))));
            }
            let api: Api<P> = Api::all(ctx.client.clone());
            return Ok(Resolved::Settled(match replace_status(&api, parent, &api_kind, status).await {
                Ok(()) => Action::requeue(std::time::Duration::from_secs(5)),
                // Someone else moved this parent first (the owner's own claim, or a peer's earlier
                // un-place) — not an error, just requeue and let the fresher object drive.
                Err(kube::Error::Api(s)) if s.code == 409 => Action::requeue(std::time::Duration::from_secs(5)),
                Err(e) => return Err(e.into()),
            }));
        }
        return Ok(Resolved::Wait {
            volume_ref: None,
            phase: crd::Phase::Error,
            cond: crd::condition("Degraded", true, "NodeMismatch", &why, gen),
            action: Action::await_change(),
        });
    }
    if !volume_is_ready(&vol) {
        // A child that has FAILED is not a child that is still working: requeueing at it forever
        // says "not materialized yet" once a minute and hides the real reason, which the child
        // already wrote down. The Volume watch re-triggers this parent when the child recovers, so
        // waiting for a change costs nothing.
        let failed = vol.status.as_ref().filter(|s| s.phase == crd::Phase::Error).map(|s| {
            s.conditions
                .iter()
                .find(|c| c.type_ == "Ready")
                .map(|c| c.message.clone())
                .unwrap_or_else(|| format!("volume {id} is in phase error"))
        });
        return Ok(Resolved::Wait {
            volume_ref: Some(id),
            phase: crd::Phase::Creating,
            cond: crd::condition(
                "VolumeReady",
                false,
                if failed.is_some() { "VolumeFailed" } else { "VolumeNotReady" },
                failed.as_deref().unwrap_or("the subvolume is not materialized yet"),
                gen,
            ),
            action: if failed.is_some() { Action::await_change() } else { Action::requeue(TICK) },
        });
    }
    Ok(Resolved::Ready(Box::new(vol)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustic_git_workspaces::engine::{Engine, Pool as EnginePool};
    use rustic_git_workspaces::kube_test::{get, mock_client, post, Recorder, Route};

    struct NoopNix;
    #[async_trait::async_trait]
    impl crate::nix::Nix for NoopNix {
        async fn build(&self, _e: &str, _t: std::time::Duration) -> Result<std::path::PathBuf, String> {
            Ok(std::path::PathBuf::from("/tmp"))
        }
        async fn ping(&self) -> Result<(), String> {
            Ok(())
        }
        async fn collect_garbage(&self) -> Result<u64, String> {
            Ok(0)
        }
    }

    fn test_ctx(pool: &std::path::Path, node: &str, routes: Vec<Route>) -> (Arc<Ctx>, Recorder) {
        let (client, rec) = mock_client(routes);
        std::env::set_var("WS_DEFAULT_IMAGE", "ghcr.io/kloudlite/rustic-git-workspace:deadbeef");
        (
            Arc::new(Ctx::new(
                client,
                Arc::new(Engine::new(EnginePool::new(pool))),
                node.into(),
                pool.to_string_lossy().into(),
                "r1".into(),
                vec![],
                Some("test:/".into()),
                Arc::new(NoopNix),
                pool.join("profiles"),
            )),
            rec,
        )
    }

    const VOLUMES: &str = "/apis/rustic-git.io/v1alpha1/volumes";

    fn volume_json(name: &str, node: &str) -> serde_json::Value {
        serde_json::json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "Volume",
            "metadata": {"name": name, "uid": format!("uid-{name}"), "generation": 1, "resourceVersion": "9"},
            "spec": {"owner": "alice", "team": "", "nodeName": node, "region": "r1", "quotaGb": 5, "replicas": 2},
            "status": {"phase": "ready"},
        })
    }

    /// The whole point of the helper: a 409 or a 422 is "lost, not broken", and anything else is
    /// an error the caller must see. Five copies of this rule is five places it can drift.
    #[tokio::test]
    async fn cas_reads_a_conflict_as_lost_and_anything_else_as_an_error() {
        let ok = mock_client(vec![Route { method: "PATCH", path: format!("{VOLUMES}/v1"), status: 200, body: volume_json("v1", "node-a") }]);
        let api: Api<crd::Volume> = Api::all(ok.0);
        assert!(cas(&api, "v1", "/spec/nodeName", serde_json::json!(""), serde_json::json!("node-a")).await.unwrap());

        let lost_body = serde_json::to_value(kube::core::Status::failure("test failed", "Invalid").with_code(422)).unwrap();
        let lost = mock_client(vec![Route { method: "PATCH", path: format!("{VOLUMES}/v1"), status: 422, body: lost_body }]);
        let api: Api<crd::Volume> = Api::all(lost.0);
        assert!(!cas(&api, "v1", "/spec/nodeName", serde_json::json!(""), serde_json::json!("node-a")).await.unwrap());

        let broken_body = serde_json::to_value(kube::core::Status::failure("etcd is down", "InternalError").with_code(500)).unwrap();
        let broken = mock_client(vec![Route { method: "PATCH", path: format!("{VOLUMES}/v1"), status: 500, body: broken_body }]);
        let api: Api<crd::Volume> = Api::all(broken.0);
        assert!(cas(&api, "v1", "/spec/nodeName", serde_json::json!(""), serde_json::json!("node-a")).await.is_err(), "an outage is not a lost race");
    }

    /// The body must be exactly Test-then-Replace on the given pointer: it is the atomicity of
    /// that pair that makes two claimants safe, and a Replace alone would let both win.
    #[tokio::test]
    async fn cas_sends_a_test_then_a_replace() {
        let (client, rec) = mock_client(vec![Route { method: "PATCH", path: format!("{VOLUMES}/v1"), status: 200, body: volume_json("v1", "node-a") }]);
        let api: Api<crd::Volume> = Api::all(client);
        cas(&api, "v1", "/spec/nodeName", serde_json::json!(""), serde_json::json!("node-a")).await.unwrap();
        let sent = rec.sent("PATCH", &format!("{VOLUMES}/v1"));
        assert_eq!(sent[0], serde_json::json!([
            {"op": "test", "path": "/spec/nodeName", "value": ""},
            {"op": "replace", "path": "/spec/nodeName", "value": "node-a"},
        ]));
    }

    /// A DECOMMISSIONING owner is ALIVE: it is draining at its people's pace and will reclaim this
    /// parent. Reading it through `unplaceable` (which folds decommissioning into dead) skipped the
    /// mismatch self-heal, and the loser of the takeover race then sat in `Error` under
    /// `await_change()` forever, holding a `status.nodeName` nothing would ever clear.
    #[tokio::test]
    async fn a_decommissioning_owner_is_alive_so_the_loser_un_places_itself() {
        let tmp = tempfile::tempdir().unwrap();
        let src = serde_json::json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "Workspace",
            "metadata": {"name": "vol-1", "uid": "uid-src", "generation": 1, "resourceVersion": "9"},
            "spec": {"owner": "alice", "name": "src", "region": "r1", "image": "img", "desiredState": "running", "packages": []},
            "status": {"phase": "ready", "nodeName": "node-a", "volumeRef": "vol-1"},
        });
        let parent_json = serde_json::json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "Workspace",
            "metadata": {"name": "ws-2", "uid": "uid-ws-2", "generation": 1, "resourceVersion": "9"},
            "spec": {"owner": "alice", "name": "copy", "region": "r1", "image": "img", "desiredState": "running", "packages": []},
            "status": {"phase": "ready", "nodeName": "node-b", "volumeRef": "vol-1"},
        });
        let routes = vec![
            get("/apis/rustic-git.io/v1alpha1/workspaces/vol-1", src),
            get(
                "/apis/rustic-git.io/v1alpha1/volumes/vol-1",
                serde_json::json!({
                    "apiVersion": "rustic-git.io/v1alpha1", "kind": "Volume",
                    "metadata": {"name": "vol-1", "uid": "uid-vol-1", "generation": 1, "resourceVersion": "9"},
                    "spec": {"owner": "alice", "team": "", "nodeName": "node-a", "region": "r1", "quotaGb": 5, "replicas": 2},
                    "status": {"phase": "ready"},
                }),
            ),
            get(
                "/api/v1/nodes/node-a",
                serde_json::json!({
                    "apiVersion": "v1", "kind": "Node",
                    "metadata": {"name": "node-a", "labels": {rustic_git_workspaces::crd::DECOMMISSION_LABEL: "true"}},
                    "status": {"conditions": [{"type": "Ready", "status": "True", "lastTransitionTime": "2000-01-01T00:00:00Z"}]},
                }),
            ),
            Route {
                method: "PUT",
                path: "/apis/rustic-git.io/v1alpha1/workspaces/ws-2/status".into(),
                status: 200,
                body: parent_json.clone(),
            },
        ];
        let (ctx, rec) = test_ctx(tmp.path(), "node-b", routes);
        let parent: crd::Workspace = serde_json::from_value(parent_json).unwrap();
        let storage = Some(crd::WorkspaceStorage {
            quota_gb: 5,
            source: Some(VolumeSource::CloneOf { volume: "vol-1".into(), commit: Some("sync-vol-1-bbbb".into()) }),
        });

        let out = resolve_volume(&parent, "alice", "", "r1", &storage, "node-b", &[], 1, &ctx).await.unwrap();

        assert!(matches!(out, Resolved::Settled(_)), "the loser settles by un-placing, not by waiting on an error");
        let sent = rec.sent("PUT", "/apis/rustic-git.io/v1alpha1/workspaces/ws-2/status").remove(0);
        assert_eq!(sent["status"]["nodeName"], "", "un-placed, so the live draining owner reclaims it");
        assert_ne!(sent["status"]["phase"], "error", "a live owner is not an error: {sent}");
    }

    /// The migration baseline is owned by its PARENT, not its Volume (the reverse of a real push
    /// snapshot — see the WHY comment on `migrate_and_seed_baseline`): a Volume that only ever had
    /// its baseline is not worth keeping once the workspace it was cut for is gone, so the
    /// baseline must die with the parent rather than outlive it as an orphan CR. 13 were found
    /// on the cluster that way before this had an owner at all.
    #[tokio::test]
    async fn the_migration_baseline_is_owned_by_its_parent() {
        let tmp = tempfile::tempdir().unwrap();
        // The mid-migration staging dir: `migrate_volume`'s recovery arm reports a real migration
        // without needing btrfs, which is what mints the baseline CR.
        std::fs::create_dir_all(tmp.path().join("vol/vol-1/live-migrating")).unwrap();
        let vol: crd::Volume = serde_json::from_value(serde_json::json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "Volume",
            "metadata": {"name": "vol-1", "uid": "uid-vol-1", "generation": 1, "resourceVersion": "9"},
            "spec": {"owner": "alice", "team": "", "nodeName": "node-a", "region": "r1", "quotaGb": 5, "replicas": 2},
            "status": {"phase": "ready"},
        }))
        .unwrap();
        let parent: crd::Workspace = serde_json::from_value(serde_json::json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "Workspace",
            "metadata": {"name": "vol-1", "uid": "uid-ws-1", "generation": 1, "resourceVersion": "9"},
            "spec": {"owner": "alice", "name": "src", "region": "r1", "image": "img", "desiredState": "running", "packages": []},
            "status": {"phase": "ready", "nodeName": "node-a", "volumeRef": "vol-1"},
        }))
        .unwrap();
        let (ctx, rec) = test_ctx(tmp.path(), "node-a", vec![post(
            "/apis/rustic-git.io/v1alpha1/snapshots",
            serde_json::json!({
                "apiVersion": "rustic-git.io/v1alpha1", "kind": "Snapshot",
                "metadata": {"name": "vol-1.aaaa"},
                "spec": {"volume": "vol-1", "owner": "alice", "worktree": "vol-1", "parent": "", "transient": false},
            }),
        )]);

        let state = crd::SnapshotState::Workspace {
            image: "alpine:3.20".into(),
            packages: vec![],
            resources: Default::default(),
            quota_gb: 5,
            attached_environment: None,
        };
        let parent_ref = super::owner_ref_of_kind(&parent).unwrap();
        assert!(
            super::super::migrate_and_seed_baseline(&ctx, &vol, parent_ref, "alice", state).await.unwrap(),
            "the staging dir is a real migration"
        );

        let sent = rec.sent("POST", "/apis/rustic-git.io/v1alpha1/snapshots").remove(0);
        let owner = &sent["metadata"]["ownerReferences"][0];
        assert_eq!(owner["kind"], "Workspace");
        assert_eq!(owner["name"], "vol-1");
        assert_eq!(owner["uid"], "uid-ws-1");
        assert_eq!(owner["controller"], true);
        assert_eq!(sent["spec"]["state"]["kind"], "workspace");
    }
}
