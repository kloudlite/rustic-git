//! The node-scoped controller: three reconcilers over the `rustic-git.io/v1alpha1` CRDs bound to
//! THIS node, converging the local btrfs pool and the node's pods toward what the specs ask for.
//!
//! `spec.nodeName` is the whole sharding story: two nodes cannot contend for one object because the
//! object names its node. There is no acquisition, no expiry, no requeue sweep — the queue, the
//! lease and the heartbeat this replaces were all re-implementations of what a watch already is.
//!
//! Long btrfs work runs on `spawn_blocking` (its own OS thread, its own tiny current-thread
//! runtime), not `tokio::spawn` on the shared reactor: `Engine::push`/`squash` block on `ws_lock`'s
//! synchronous `libc::flock`, and a `LocalSet`/single-reactor-thread design would let one
//! workspace's lock wait freeze every other in-flight operation. `spawn_blocking` also sidesteps
//! `WsClone`'s `&dyn Fn` stop/start hooks (no `+Send` bound in `engine::ops.rs`, out of scope to
//! change here) — they never have to cross an actual cross-thread `.await` boundary.

use futures::StreamExt;
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::{LimitRange, Namespace, PersistentVolume, PersistentVolumeClaim, Pod, Service};
use k8s_openapi::api::networking::v1::NetworkPolicy;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, OwnerReference};
use kube::api::{Patch, PatchParams};
use kube::runtime::controller::{Action, Controller};
use kube::runtime::finalizer::{finalizer, Event as FinalizerEvent};
use kube::runtime::watcher;
use kube::{Api, Resource, ResourceExt};
use rustic_git_workspaces::api::{PUSH_ANNOTATION, PUSH_MESSAGE_ANNOTATION};
use rustic_git_workspaces::crd::{self, DesiredState, LastPush, VolumeSource};
use rustic_git_workspaces::engine::Engine;
use rustic_git_workspaces::k8s;
use rustic_git_workspaces::model;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// While an operation runs, and while a stop is waiting for its push to land. Short on purpose:
/// these are progress checks, not retries.
const TICK: Duration = Duration::from_secs(15);
/// After a failure. The reconcile that observes it does not stamp `observedGeneration`, so the next
/// pass starts the work again — backoff, never give up.
const RETRY: Duration = Duration::from_secs(60);

/// Keyed by uid, carrying the generation it was started for — see `Ctx::running`.
///
/// The generation is in the VALUE, not the key. Keyed by `{uid, generation}` a spec edit during a
/// long push produced a new key, so the running handle was never looked up again: it was never
/// drained, never removed, and its btrfs work ran on unobserved. One entry per volume cannot leak,
/// and "is anything running for this volume" becomes answerable — which is what the delete path
/// needs.
pub type InFlight = HashMap<String, (i64, tokio::task::JoinHandle<Result<Done, String>>)>;

pub struct Ctx {
    pub client: kube::Client,
    pub engine: Arc<Engine>,
    pub node: String,
    pub pool: String,
    /// `WS_RUNTIME_CLASS`, e.g. `gvisor`. Empty means tenant pods run on the host kernel.
    ///
    /// Per-cluster, because a `runtimeClassName` naming a runtime the nodes have not got makes
    /// every tenant pod fail to start. Enabling it belongs where the runtime is installed.
    pub runtime_class: Option<String>,
    /// In-flight long btrfs operations, one per volume. THE idempotency guard, and a local
    /// in-memory check rather than a distributed lease because the field selector already
    /// guarantees this node is the only reconciler of this object.
    pub running: Mutex<InFlight>,
}

impl Ctx {
    pub fn new(client: kube::Client, engine: Arc<Engine>, node: String, pool: String) -> Ctx {
        let runtime_class = std::env::var("WS_RUNTIME_CLASS").ok().filter(|v| !v.is_empty());
        if let Some(rc) = &runtime_class {
            tracing::info!(runtime_class = %rc, "tenant pods will run sandboxed");
        }
        Ctx { client, engine, node, pool, runtime_class, running: Mutex::new(HashMap::new()) }
    }
}

/// What a finished volume operation has to say about the pool, drained into status on a later pass.
#[derive(Default, Debug)]
pub struct Done {
    pub phase: String,
    pub last_push: Option<LastPush>,
    pub lineage_tip: Option<String>,
}

#[derive(Debug)]
pub struct ReconcileErr(pub String);

impl std::fmt::Display for ReconcileErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for ReconcileErr {}
impl From<kube::Error> for ReconcileErr {
    fn from(e: kube::Error) -> Self {
        ReconcileErr(e.to_string())
    }
}

/// Runs all three controllers to completion (i.e. forever). Returns only on shutdown signal.
/// Map a namespaced child back to its CLUSTER-SCOPED owner.
///
/// `Controller::owns` cannot be used here. It derives the parent's `ObjectRef` from the child's
/// owner reference AND the child's namespace — correct when parent and child share a namespace,
/// wrong when the parent is cluster-scoped. It produced refs like
/// `Environment.../env-abc.env-env-abc` and every reconcile triggered by a child event then failed
/// with "not found in local store", so an environment converged once on creation and never
/// responded to its Deployments changing again.
fn owned_by<P, C>(child: &C) -> Option<kube::runtime::reflector::ObjectRef<P>>
where
    P: Resource<DynamicType = ()>,
    C: Resource,
{
    child
        .meta()
        .owner_references
        .as_ref()?
        .iter()
        .find(|r| r.controller.unwrap_or(false) && r.kind == P::kind(&()))
        // Deliberately no `.within(..)`: the parent has no namespace to be within.
        .map(|r| kube::runtime::reflector::ObjectRef::<P>::new(&r.name))
}

pub async fn run(ctx: Arc<Ctx>) -> Result<(), String> {
    // Before the watches: a node with nothing to do must still be able to prove it is alive.
    heartbeat(&ctx.pool);
    spawn_heartbeat(ctx.clone());
    // NB the RBAC grant is cluster-wide — a field selector narrows a watch, never authorization.
    let mine = watcher::Config::default().fields(&format!("spec.nodeName={}", ctx.node));
    let volumes = Controller::new(Api::<crd::Volume>::all(ctx.client.clone()), mine.clone())
        .shutdown_on_signal()
        .run(reconcile_volume, error_policy, ctx.clone())
        .for_each(|r| async move {
            if let Err(e) = r {
                tracing::warn!(error = %e, "volume reconcile")
            }
        });
    let workspaces = Controller::new(Api::<crd::Workspace>::all(ctx.client.clone()), mine.clone())
        .watches(Api::<Pod>::all(ctx.client.clone()), watcher::Config::default(), |p| {
            owned_by::<crd::Workspace, _>(&p)
        })
        .shutdown_on_signal()
        .run(reconcile_workspace, error_policy, ctx.clone())
        .for_each(|r| async move {
            if let Err(e) = r {
                tracing::warn!(error = %e, "workspace reconcile")
            }
        });
    let environments = Controller::new(Api::<crd::Environment>::all(ctx.client.clone()), mine)
        .watches(Api::<Deployment>::all(ctx.client.clone()), watcher::Config::default(), |d| {
            owned_by::<crd::Environment, _>(&d)
        })
        .shutdown_on_signal()
        .run(reconcile_environment, error_policy, ctx.clone())
        .for_each(|r| async move {
            if let Err(e) = r {
                tracing::warn!(error = %e, "environment reconcile")
            }
        });
    tokio::join!(volumes, workspaces, environments);
    Ok(())
}

/// Every reconcile error is a requeue with backoff. There is deliberately no branch that concludes
/// "reality doesn't match, so delete it" — see the keep-biased rule, and `crates/registry/src/gc.rs`.
fn error_policy<K: Resource<DynamicType = ()>>(obj: Arc<K>, err: &ReconcileErr, _ctx: Arc<Ctx>) -> Action {
    // Named, because three controllers share this policy: an unattributed "reconcile failed" line
    // says nothing about which object is stuck or even which kind it was.
    tracing::warn!(kind = %K::kind(&()), name = %obj.name_any(), error = %err, "reconcile failed, requeueing");
    Action::requeue(RETRY)
}

/// Proof of life for the DaemonSet's liveness probe: a watch that silently died looks identical
/// from the outside without it.
///
/// Synchronous, and only ever called from `spawn_heartbeat`'s own task — never from a reconcile.
/// The reconcilers used to call it directly, which put a blocking `write` on the reactor for no
/// gain: the periodic beat already proves liveness, and it proves MORE (it makes a real API call
/// first), so a reconcile touching the file added nothing but the blocking call.
fn heartbeat(pool: &str) {
    let _ = std::fs::write(std::path::Path::new(pool).join(".agent-heartbeat"), b"ok");
}

/// Beat independently of whether there is anything to reconcile.
///
/// Reconciles alone are not proof of life: a node with no workspaces on it never reconciles, so an
/// idle controller would look exactly like a dead one and its own liveness probe would kill it —
/// observed on a second node the first time this shipped as a DaemonSet.
///
/// The beat is a real API call rather than a bare timer, because "the process is still scheduled"
/// is not the property the probe is for. A cheap capped list exercises the same connection,
/// credentials and CRD registration the watches depend on, so a controller that has lost the API
/// server stops beating instead of reporting healthy while converging nothing.
fn spawn_heartbeat(ctx: Arc<Ctx>) {
    tokio::spawn(async move {
        let api: Api<crd::Volume> = Api::all(ctx.client.clone());
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            match api.list(&kube::api::ListParams::default().limit(1)).await {
                Ok(_) => heartbeat(&ctx.pool),
                Err(e) => tracing::error!(error = %e, "heartbeat: api unreachable, not beating"),
            }
        }
    });
}

/// Poison-tolerant, like `auth_cache` and the manifest cache elsewhere in this workspace: a panic
/// while this lock was held must not turn every later reconcile into a panic of its own. The map
/// holds join handles, which nothing half-finished can leave inconsistent.
fn running_contains(ctx: &Arc<Ctx>, uid: &str) -> bool {
    ctx.running.lock().unwrap_or_else(|p| p.into_inner()).contains_key(uid)
}

fn owner_ref<K: Resource<DynamicType = ()>>(obj: &K) -> Result<OwnerReference, ReconcileErr> {
    obj.controller_owner_ref(&()).ok_or_else(|| ReconcileErr("object has no uid".into()))
}

// ── volumes ──────────────────────────────────────────────────────────────

async fn reconcile_volume(v: Arc<crd::Volume>, ctx: Arc<Ctx>) -> Result<Action, ReconcileErr> {
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

/// The push request this volume's status has already satisfied — `LastPush.at` carries the
/// annotation's timestamp rather than a wall clock of its own, because "which request did this
/// push answer" is the only question anything asks of it, and a metadata annotation does not bump
/// `metadata.generation` for `observedGeneration` to track.
fn push_pending(v: &crd::Volume) -> Option<String> {
    let requested = v.annotations().get(PUSH_ANNOTATION)?.clone();
    let satisfied = v.status.as_ref().and_then(|s| s.last_push.as_ref()).map(|p| p.at.clone());
    (satisfied.as_deref() != Some(requested.as_str())).then_some(requested)
}

pub async fn apply_volume(v: &crd::Volume, ctx: &Arc<Ctx>) -> Result<Action, ReconcileErr> {
    let gen = v.meta().generation.unwrap_or(0);
    let uid = v.uid().unwrap_or_default();
    let observed = v.status.as_ref().and_then(|s| s.observed_generation) == Some(gen);
    let pending = push_pending(v);

    // 1. Nothing asked for.
    if observed && pending.is_none() && !running_contains(ctx, &uid) {
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
                    // The generation the work actually ran for, not the current one: a spec edited
                    // mid-operation must not be reported as observed by an operation that never
                    // saw it. When they differ this leaves the object unobserved, so the next pass
                    // starts the new work — which is the intended behaviour.
                    observed_generation: Some(started_gen),
                    subvolume_present: true,
                    lineage_tip: done.lineage_tip.or_else(|| v.status.as_ref().and_then(|s| s.lineage_tip.clone())),
                    last_push: done.last_push.or_else(|| v.status.as_ref().and_then(|s| s.last_push.clone())),
                    progress: None,
                    conditions: vec![],
                };
                st.conditions = vec![crd::condition("Ready", true, "Converged", "volume is materialized", gen)];
                write_volume_status(v, st, ctx).await?;
                Ok(Action::await_change())
            }
            // `observedGeneration` is deliberately NOT stamped: an unobserved generation is what
            // makes the next pass try again. Nothing is deleted, nothing is marked permanently
            // failed — the keep-biased rule, applied to the error path.
            Err(e) => {
                let st = crd::VolumeStatus {
                    phase: "error".into(),
                    observed_generation: v.status.as_ref().and_then(|s| s.observed_generation),
                    subvolume_present: ctx.engine.pool.live(&v.name_any()).exists(),
                    lineage_tip: v.status.as_ref().and_then(|s| s.lineage_tip.clone()),
                    last_push: v.status.as_ref().and_then(|s| s.last_push.clone()),
                    progress: None,
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
    let materialize = !observed;
    let message = v.annotations().get(PUSH_MESSAGE_ANNOTATION).cloned();
    let handle = tokio::task::spawn_blocking(move || {
        volume_work(&engine, &id, &owner, source, materialize, pending, message)
    });
    ctx.running.lock().unwrap_or_else(|p| p.into_inner()).insert(uid, (gen, handle));
    write_volume_status(v, progressing(v, gen), ctx).await?;
    Ok(Action::requeue(TICK))
}

fn progressing(v: &crd::Volume, gen: i64) -> crd::VolumeStatus {
    let prev = v.status.clone().unwrap_or_default();
    crd::VolumeStatus {
        phase: "working".into(),
        conditions: vec![crd::condition("Progressing", true, "Working", "btrfs operation in flight", gen)],
        ..prev
    }
}

/// One volume's whole unit of work, on its own OS thread with its own tiny current-thread runtime,
/// exactly as `run_job_blocking` did and for the same reason (see the module doc).
fn volume_work(
    engine: &Engine,
    id: &str,
    owner: &str,
    source: Option<VolumeSource>,
    materialize: bool,
    push: Option<String>,
    message: Option<String>,
) -> Result<Done, String> {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
    rt.block_on(async {
        if materialize {
            // The owner breadcrumb `Engine::push`'s detached `squash` child reads back — written
            // before anything can push, and re-written on every materialize because a rebuilt node
            // has the subvolume without the file.
            crate::record_owner(&engine.pool.root.to_string_lossy(), id, owner);
            match &source {
                None => engine.create_subvol(id).map_err(|e| e.to_string())?,
                Some(VolumeSource::CloneOf { volume }) => {
                    engine.clone_local_ids(owner, volume, id).await.map_err(|e| e.to_string())?
                }
                Some(VolumeSource::RestoreOf { volume, snapshot_id }) => {
                    engine.restore(owner, volume, snapshot_id, id).await.map_err(|e| e.to_string())?
                }
            }
        }
        let mut done = Done { phase: "ready".into(), ..Done::default() };
        if let Some(at) = push {
            // `push_env` rather than `push`: the VOLUME is what gets pushed, and it is keyed by id
            // alone — the workspace or environment around it is not involved.
            let out = engine
                .push_env(owner, id, &serde_json::Value::Null, message.as_deref())
                .await
                .map_err(|e| e.to_string())?;
            done.lineage_tip = Some(out.layer.clone());
            done.last_push = Some(LastPush { snapshot_id: out.layer, at, message });
        }
        Ok(done)
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
    tokio::task::spawn_blocking(move || crate::cleanup_local(&engine, &id))
        .await
        .map_err(|e| ReconcileErr(format!("cleanup panicked: {e}")))?;
    Ok(Action::await_change())
}

async fn write_volume_status(v: &crd::Volume, st: crd::VolumeStatus, ctx: &Arc<Ctx>) -> Result<(), ReconcileErr> {
    if let Some(cur) = &v.status {
        if status_eq(cur, &st) {
            return Ok(());
        }
    }
    let api: Api<crd::Volume> = Api::all(ctx.client.clone());
    patch_status(&api, &v.name_any(), "Volume", serde_json::to_value(&st).map_err(|e| ReconcileErr(e.to_string()))?).await
}

/// Status equality that ignores `lastTransitionTime`: a condition re-stamped with `now` is not a
/// change, and treating it as one is the classic controller hot loop — a status write that triggers
/// its own watch event and reconciles again. That is an outage, not a warning.
fn conditions_eq(a: &[Condition], b: &[Condition]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(x, y)| {
            x.type_ == y.type_
                && x.status == y.status
                && x.reason == y.reason
                && x.message == y.message
                && x.observed_generation == y.observed_generation
        })
}

fn status_eq(a: &crd::VolumeStatus, b: &crd::VolumeStatus) -> bool {
    a.phase == b.phase
        && a.observed_generation == b.observed_generation
        && a.subvolume_present == b.subvolume_present
        && a.lineage_tip == b.lineage_tip
        && a.last_push == b.last_push
        && a.progress == b.progress
        && conditions_eq(&a.conditions, &b.conditions)
}

/// Server-side apply on the `/status` subresource. Apply, not Merge: the field manager owns exactly
/// the status fields it sets, so two writers cannot silently clobber each other.
async fn patch_status<K>(api: &Api<K>, name: &str, kind: &str, status: serde_json::Value) -> Result<(), ReconcileErr>
where
    K: Clone + serde::de::DeserializeOwned + std::fmt::Debug,
{
    let body = serde_json::json!({
        "apiVersion": format!("{}/{}", crd::GROUP, crd::VERSION),
        "kind": kind,
        "status": status,
    });
    api.patch_status(name, &PatchParams::apply(crd::AGENT_FIELD_MANAGER).force(), &Patch::Apply(&body)).await?;
    Ok(())
}

// ── workspaces ───────────────────────────────────────────────────────────

async fn reconcile_workspace(w: Arc<crd::Workspace>, ctx: Arc<Ctx>) -> Result<Action, ReconcileErr> {
    apply_workspace(&w, &ctx).await
}

/// The pod's node is READ from the referenced `Volume`, never recomputed. Two places allowed to name
/// a node is two places that can disagree about where the data is, and the failure mode is an
/// owner's data split across pools — so a disagreement refuses rather than picks.
async fn volume_node(volume_ref: &str, ctx: &Arc<Ctx>, want: &str) -> Result<Result<crd::Volume, String>, ReconcileErr> {
    let api: Api<crd::Volume> = Api::all(ctx.client.clone());
    let vol = api.get(volume_ref).await?;
    if vol.spec.node_name != want {
        return Ok(Err(format!(
            "spec.nodeName {want} disagrees with volume {volume_ref}'s node {}",
            vol.spec.node_name
        )));
    }
    Ok(Ok(vol))
}

pub async fn apply_workspace(w: &crd::Workspace, ctx: &Arc<Ctx>) -> Result<Action, ReconcileErr> {
    let gen = w.meta().generation.unwrap_or(0);
    let id = w.spec.volume_ref.clone();
    let owner_ref = owner_ref(w)?;
    let vol = match volume_node(&id, ctx, &w.spec.node_name).await? {
        Ok(v) => v,
        Err(why) => {
            let st = crd::WorkspaceStatus {
                phase: "error".into(),
                observed_generation: None,
                pod_ref: w.status.as_ref().and_then(|s| s.pod_ref.clone()),
                conditions: vec![crd::condition("Degraded", true, "NodeMismatch", &why, gen)],
            };
            write_ws_status(w, st, ctx).await?;
            return Ok(Action::await_change());
        }
    };

    let ns = crd::ws_namespace(&w.spec.owner);
    // No ownerReference on the namespace: it is shared by every workspace this user owns, so
    // deleting one would garbage-collect its siblings. See `crd::ws_namespace`.
    ensure(&Api::<Namespace>::all(ctx.client.clone()), &k8s::namespace(&ns, &w.spec.owner, "workspace", None)).await?;
    let policies = Api::<NetworkPolicy>::namespaced(ctx.client.clone(), &ns);
    for p in k8s::default_policies(&ns, &w.spec.owner, &owner_ref) {
        ensure(&policies, &p).await?;
    }
    // No ownerReference, for the same reason the namespace has none: it is shared by every
    // workspace this user owns, so deleting one must not take the ceiling with it.
    ensure(
        &Api::<LimitRange>::namespaced(ctx.client.clone(), &ns),
        &k8s::limit_range(&ns, &w.spec.owner, "workspace", &w.spec.resources, None),
    )
    .await?;
    let pod_ctx = k8s::PodContext {
        pool: &ctx.pool,
        node_name: &vol.spec.node_name,
        owner_ref: owner_ref.clone(),
        runtime_class: ctx.runtime_class.as_deref(),
    };
    ensure(
        &Api::<PersistentVolume>::all(ctx.client.clone()),
        &k8s::local_pv(&id, &w.spec.owner, vol.spec.quota_gb, &pod_ctx),
    )
    .await?;
    ensure(
        &Api::<PersistentVolumeClaim>::namespaced(ctx.client.clone(), &ns),
        &k8s::claim(&ns, &id, &w.spec.owner, vol.spec.quota_gb, &owner_ref),
    )
    .await?;

    let pods: Api<Pod> = Api::namespaced(ctx.client.clone(), &ns);
    let (phase, pod_ref) = match w.spec.desired_state {
        DesiredState::Running => {
            create_if_absent(&pods, &k8s::workspace_pod(&w.spec, &id, &pod_ctx)).await?;
            // Applying a pod is not a pod running. Read it back: a pod can sit Pending on an
            // unschedulable node or CrashLoopBackOff on a bad image, and reporting Ready straight
            // from the apply made a broken workspace indistinguishable from a working one.
            if !pod_is_ready(&pods, &id).await? {
                let st = crd::WorkspaceStatus {
                    phase: "creating".into(),
                    // Unobserved on purpose: this generation has not converged, so the next pass
                    // re-runs instead of treating a Pending pod as done.
                    observed_generation: None,
                    pod_ref: Some(format!("{ns}/{id}")),
                    conditions: vec![crd::condition("Ready", false, "PodNotReady", "pod is not ready yet", gen)],
                };
                write_ws_status(w, st, ctx).await?;
                return Ok(Action::requeue(TICK));
            }
            // `ready`, not `running`: this string is deserialized into `model::WsState` by the
            // `/v1` projection, which spells the running state `Ready`. An unknown phase does not
            // error — it falls back to `Creating`, so a healthy workspace showed "Creating" in the
            // UI forever. `phase_names_the_doc_enum` pins the vocabulary.
            ("ready", Some(format!("{ns}/{id}")))
        }
        // Stopping IS deleting the pod — there is no policy the kubelet interprets. The subvolume
        // and its claim stay; only the compute goes away.
        DesiredState::Stopped => {
            delete_ignoring_404(&pods, &id).await?;
            ("stopped", None)
        }
    };
    let st = crd::WorkspaceStatus {
        phase: phase.into(),
        observed_generation: Some(gen),
        pod_ref,
        conditions: vec![crd::condition("Ready", true, "Converged", "workspace matches spec", gen)],
    };
    write_ws_status(w, st, ctx).await?;
    Ok(Action::await_change())
}

/// Whether the pod exists AND its `Ready` condition is true. A missing pod is "not ready", never an
/// error: that is the normal state between applying it and the kubelet creating it.
async fn pod_is_ready(pods: &Api<Pod>, name: &str) -> Result<bool, ReconcileErr> {
    let Some(pod) = pods.get_opt(name).await? else {
        return Ok(false);
    };
    Ok(pod
        .status
        .and_then(|s| s.conditions)
        .is_some_and(|cs| cs.iter().any(|c| c.type_ == "Ready" && c.status == "True")))
}

async fn write_ws_status(w: &crd::Workspace, st: crd::WorkspaceStatus, ctx: &Arc<Ctx>) -> Result<(), ReconcileErr> {
    if let Some(cur) = &w.status {
        if cur.phase == st.phase
            && cur.observed_generation == st.observed_generation
            && cur.pod_ref == st.pod_ref
            && conditions_eq(&cur.conditions, &st.conditions)
        {
            return Ok(());
        }
    }
    let api: Api<crd::Workspace> = Api::all(ctx.client.clone());
    patch_status(&api, &w.name_any(), "Workspace", serde_json::to_value(&st).map_err(|e| ReconcileErr(e.to_string()))?)
        .await
}

// ── environments ─────────────────────────────────────────────────────────

async fn reconcile_environment(e: Arc<crd::Environment>, ctx: Arc<Ctx>) -> Result<Action, ReconcileErr> {
    apply_environment(&e, &ctx).await
}

pub async fn apply_environment(e: &crd::Environment, ctx: &Arc<Ctx>) -> Result<Action, ReconcileErr> {
    let gen = e.meta().generation.unwrap_or(0);
    let id = e.spec.volume_ref.clone();
    let owner_ref = owner_ref(e)?;
    let vol = match volume_node(&id, ctx, &e.spec.node_name).await? {
        Ok(v) => v,
        Err(why) => {
            let st = crd::EnvironmentStatus {
                phase: "error".into(),
                observed_generation: None,
                service_status: vec![],
                conditions: vec![crd::condition("Degraded", true, "NodeMismatch", &why, gen)],
            };
            write_env_status(e, st, ctx).await?;
            return Ok(Action::await_change());
        }
    };

    let ns = crd::env_namespace(&id);
    let deployments: Api<Deployment> = Api::namespaced(ctx.client.clone(), &ns);

    if e.spec.desired_state == DesiredState::Stopped {
        // An environment that stops must push first. One push of the env's own subvolume covers
        // every mounted volume atomically; an env torn down without it loses its last state for
        // good, which is why the deletes below are gated on the push having landed, not merely
        // requested.
        if let Some(action) = await_stop_push(&vol, e, gen, ctx).await? {
            return Ok(action);
        }
        for svc in &e.spec.services {
            delete_ignoring_404(&deployments, &svc.name).await?;
        }
        let st = crd::EnvironmentStatus {
            phase: "stopped".into(),
            observed_generation: Some(gen),
            service_status: vec![],
            conditions: vec![crd::condition("Ready", true, "Stopped", "pushed and stopped", gen)],
        };
        write_env_status(e, st, ctx).await?;
        return Ok(Action::await_change());
    }

    ensure(
        &Api::<Namespace>::all(ctx.client.clone()),
        &k8s::namespace(&ns, &e.spec.owner, "environment", Some(&owner_ref)),
    )
    .await?;
    let policies = Api::<NetworkPolicy>::namespaced(ctx.client.clone(), &ns);
    for p in k8s::default_policies(&ns, &e.spec.owner, &owner_ref) {
        ensure(&policies, &p).await?;
    }
    // The env unit's ceiling, matching `service_deployment`'s resources: 4 GB limit, packed at the
    // model's 1.5x oversubscription. Owned by the Environment — this namespace holds exactly one.
    ensure(
        &Api::<LimitRange>::namespaced(ctx.client.clone(), &ns),
        &k8s::limit_range(&ns, &e.spec.owner, "environment", &k8s::env_unit_resources(), Some(&owner_ref)),
    )
    .await?;
    let pod_ctx = k8s::PodContext {
        pool: &ctx.pool,
        node_name: &vol.spec.node_name,
        owner_ref: owner_ref.clone(),
        runtime_class: ctx.runtime_class.as_deref(),
    };
    ensure(
        &Api::<PersistentVolume>::all(ctx.client.clone()),
        &k8s::local_pv(&id, &e.spec.owner, vol.spec.quota_gb, &pod_ctx),
    )
    .await?;
    ensure(
        &Api::<PersistentVolumeClaim>::namespaced(ctx.client.clone(), &ns),
        &k8s::claim(&ns, &id, &e.spec.owner, vol.spec.quota_gb, &owner_ref),
    )
    .await?;
    // Every declared folder must exist before a subPath binds it — and `validate_mount` here is a
    // security check, not a formality: `create_dir_all` on an unvalidated folder is itself the
    // escape, mkdir -p'ing outside the subvolume before a pod ever starts.
    // On a blocking thread: `create_dir_all` is sync IO, and the pool can be a network-backed or
    // busy disk. Same rule the module doc states for the btrfs work.
    let live = ctx.engine.pool.live(&id);
    let services = e.spec.services.clone();
    tokio::task::spawn_blocking(move || mkdir_env_mounts(&live, &services))
        .await
        .map_err(|e| ReconcileErr(format!("mkdir panicked: {e}")))?
        .map_err(ReconcileErr)?;

    let services: Api<Service> = Api::namespaced(ctx.client.clone(), &ns);
    for svc in &e.spec.services {
        let dep = k8s::service_deployment(svc, &id, &e.spec.owner, &pod_ctx).map_err(ReconcileErr)?;
        ensure(&deployments, &dep).await?;
        ensure(&services, &k8s::service_clusterip(svc, &id, &e.spec.owner, &owner_ref)).await?;
    }
    // Read each Deployment back rather than reporting `ready: true` from having applied it. A
    // service whose image will not pull, or whose pod cannot schedule, was previously reported
    // ready the instant its Deployment object existed — so `kubectl wait --for=condition=Ready
    // environment` returned before anything was listening, and the only thing that noticed was a
    // connectivity check failing two steps later.
    let mut service_status = Vec::with_capacity(e.spec.services.len());
    for svc in &e.spec.services {
        service_status.push(deployment_status(&deployments, &svc.name).await?);
    }
    let all_ready = service_status.iter().all(|s| s.ready);
    let st = crd::EnvironmentStatus {
        phase: "running".into(),
        // Not converged until every service is: leaving it unobserved is what makes the next pass
        // look again instead of declaring a half-up environment finished.
        observed_generation: all_ready.then_some(gen),
        service_status,
        conditions: vec![if all_ready {
            crd::condition("Ready", true, "Converged", "environment matches spec", gen)
        } else {
            crd::condition("Ready", false, "ServicesNotReady", "one or more services are not ready", gen)
        }],
    };
    write_env_status(e, st, ctx).await?;
    Ok(if all_ready { Action::await_change() } else { Action::requeue(TICK) })
}

/// One service's observed readiness, from the Deployment's own status.
///
/// `readyReplicas >= 1`, not `replicas`: `replicas` is what was asked for, `readyReplicas` is what
/// is actually serving. A missing Deployment reports not-ready rather than erroring — it is the
/// ordinary gap between applying it and the API server materializing it.
async fn deployment_status(deployments: &Api<Deployment>, name: &str) -> Result<crd::ServiceStatus, ReconcileErr> {
    let Some(d) = deployments.get_opt(name).await? else {
        return Ok(crd::ServiceStatus { name: name.into(), ready: false, message: Some("deployment not created yet".into()) });
    };
    let ready = d.status.as_ref().and_then(|s| s.ready_replicas).unwrap_or(0);
    Ok(crd::ServiceStatus {
        name: name.into(),
        ready: ready >= 1,
        message: (ready < 1).then(|| "no ready replicas".to_string()),
    })
}

/// `Some(action)` while the stop is still waiting on its push: request it once (an annotation on the
/// `Volume`, the same generation bump `/v1`'s push verb writes), then requeue until the volume's
/// status says that exact request landed.
async fn await_stop_push(
    vol: &crd::Volume,
    e: &crd::Environment,
    gen: i64,
    ctx: &Arc<Ctx>,
) -> Result<Option<Action>, ReconcileErr> {
    let requested = vol.annotations().get(PUSH_ANNOTATION).cloned();
    let satisfied = vol.status.as_ref().and_then(|s| s.last_push.as_ref()).map(|p| p.at.clone());
    match requested {
        Some(r) if satisfied.as_deref() == Some(r.as_str()) => Ok(None),
        Some(_) => {
            let st = crd::EnvironmentStatus {
                // Still `running`: the deployments ARE up until the push lands, and
                // `model::EnvState` has no `Stopping` — an unknown phase silently becomes
                // `Creating`, which is both wrong and alarming. Progress belongs in the condition
                // below, which is where a reader looks for it.
                phase: "running".into(),
                observed_generation: None,
                service_status: vec![],
                conditions: vec![crd::condition("Progressing", true, "PushBeforeStop", "waiting for the volume's push", gen)],
            };
            write_env_status(e, st, ctx).await?;
            Ok(Some(Action::requeue(TICK)))
        }
        None => {
            let api: Api<crd::Volume> = Api::all(ctx.client.clone());
            let now = k8s_openapi::jiff::Timestamp::now().to_string();
            let patch = serde_json::json!({"metadata": {"annotations": {PUSH_ANNOTATION: now}}});
            api.patch(&vol.name_any(), &PatchParams::default(), &Patch::Merge(&patch)).await?;
            Ok(Some(Action::requeue(TICK)))
        }
    }
}

/// Every declared volume is a folder inside the env's ONE subvolume — mkdir -p each before a pod
/// binds it as a subPath.
fn mkdir_env_mounts(live: &std::path::Path, services: &[model::Service]) -> Result<(), String> {
    let mut seen = std::collections::HashSet::new();
    for svc in services {
        for m in &svc.mounts {
            if seen.insert(m.folder.clone()) {
                // `create_dir_all` on an unvalidated folder is itself the escape — it would
                // happily mkdir -p outside the subvolume before a pod ever ran.
                model::validate_mount(m)?;
                std::fs::create_dir_all(live.join("volumes").join(&m.folder)).map_err(|e| e.to_string())?;
            }
        }
    }
    Ok(())
}

async fn write_env_status(e: &crd::Environment, st: crd::EnvironmentStatus, ctx: &Arc<Ctx>) -> Result<(), ReconcileErr> {
    if let Some(cur) = &e.status {
        if cur.phase == st.phase
            && cur.observed_generation == st.observed_generation
            && cur.service_status == st.service_status
            && conditions_eq(&cur.conditions, &st.conditions)
        {
            return Ok(());
        }
    }
    let api: Api<crd::Environment> = Api::all(ctx.client.clone());
    patch_status(
        &api,
        &e.name_any(),
        "Environment",
        serde_json::to_value(&st).map_err(|e| ReconcileErr(e.to_string()))?,
    )
    .await
}

// ── shared plumbing ──────────────────────────────────────────────────────

/// Server-side apply of a whole child object: level-triggered convergence in one call, and the one
/// thing that makes "someone deleted the Deployment by hand" a self-healing event.
async fn ensure<K>(api: &Api<K>, obj: &K) -> Result<(), ReconcileErr>
where
    K: Resource + Clone + serde::Serialize + serde::de::DeserializeOwned + std::fmt::Debug,
    K::DynamicType: Default,
{
    let name = obj.meta().name.clone().ok_or_else(|| ReconcileErr("child object has no name".into()))?;
    api.patch(&name, &PatchParams::apply(crd::AGENT_FIELD_MANAGER).force(), &Patch::Apply(obj)).await?;
    Ok(())
}

/// Create a Pod only when it is missing.
///
/// NOT `ensure`. A Pod is immutable once created: re-applying its spec is refused with "pod updates
/// may not change fields other than `spec.containers[*].image`", so a server-side apply on every
/// reconcile turns the SECOND pass into a permanent error and the object never converges. That is
/// exactly what happened when the readiness gate started requeueing — the first pass created the
/// pod, and every pass after it failed.
///
/// Convergence for a Pod is therefore "exists or does not". A spec change that matters (a new
/// image, a different slot) has to delete and recreate, which is a restart of the user's workspace
/// and belongs to an explicit action, not to a reconcile that happens to notice drift.
/// ponytail: no drift detection on the pod spec; a changed `image` or `resources` needs a stop and
/// start to take effect.
async fn create_if_absent<K>(api: &Api<K>, obj: &K) -> Result<(), ReconcileErr>
where
    K: Resource + Clone + serde::Serialize + serde::de::DeserializeOwned + std::fmt::Debug,
    K::DynamicType: Default,
{
    let name = obj.meta().name.clone().ok_or_else(|| ReconcileErr("child object has no name".into()))?;
    if api.get_opt(&name).await?.is_some() {
        return Ok(());
    }
    match api.create(&kube::api::PostParams::default(), obj).await {
        Ok(_) => Ok(()),
        // Lost a race with our own earlier pass, or with the kubelet recreating it. Already done.
        Err(kube::Error::Api(s)) if s.code == 409 => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// A 404 is the desired state already reached, not an error — a stop that races a delete, or a
/// reconcile replayed after a restart.
async fn delete_ignoring_404<K>(api: &Api<K>, name: &str) -> Result<(), ReconcileErr>
where
    K: Clone + serde::de::DeserializeOwned + std::fmt::Debug,
{
    match api.delete(name, &Default::default()).await {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(s)) if s.code == 404 => Ok(()),
        Err(e) => Err(e.into()),
    }
}
