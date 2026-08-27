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

use crate::{binding, claim};
use futures::StreamExt;
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::{LimitRange, Namespace, PersistentVolume, PersistentVolumeClaim, Pod, Service};
use k8s_openapi::api::networking::v1::NetworkPolicy;
use k8s_openapi::api::rbac::v1::RoleBinding;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, OwnerReference};
use kube::api::{Patch, PatchParams, PostParams};
use kube::runtime::controller::{Action, Controller};
use kube::runtime::finalizer::{finalizer, Event as FinalizerEvent};
use kube::runtime::watcher;
use kube::{Api, Resource, ResourceExt};
use rustic_git_workspaces::api::{PUSH_ANNOTATION, PUSH_MESSAGE_ANNOTATION, PUSH_SATISFIED_ANNOTATION};
use rustic_git_workspaces::crd::{self, DesiredState, Phase, VolumeSource};
use rustic_git_workspaces::engine::Engine;
use rustic_git_workspaces::k8s;
use rustic_git_workspaces::model;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// While an operation runs, and while a stop is waiting for its push to land. Short on purpose:
/// these are progress checks, not retries.
pub(crate) const TICK: Duration = Duration::from_secs(15);
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
    /// The API tier's ServiceAccount, which the per-namespace Secret grant names. Configurable
    /// because the API does not run in this cluster and its identity here is a deployment choice.
    pub api_service_account: String,
    pub api_namespace: String,
    /// `WS_RUNTIME_CLASS`, e.g. `gvisor`. Empty means tenant pods run on the host kernel.
    ///
    /// Per-cluster, because a `runtimeClassName` naming a runtime the nodes have not got makes
    /// every tenant pod fail to start. Enabling it belongs where the runtime is installed.
    pub runtime_class: Option<String>,
    /// In-flight long btrfs operations, one per volume. THE idempotency guard, and a local
    /// in-memory check rather than a distributed lease because the field selector already
    /// guarantees this node is the only reconciler of this object.
    pub running: Mutex<InFlight>,
    /// This node's region, from `WS_REGION` — the other half of an `OwnerBinding`'s identity.
    pub region: String,
    /// The roles this node carries, read ONCE from its own `Node` labels at startup
    /// (`rustic-git.io/session`, `rustic-git.io/env`). A second, hand-maintained copy of a label
    /// the scheduler already reads is a second thing that can be wrong — see `k8s::placement`.
    pub roles: Vec<String>,
}

impl Ctx {
    pub fn new(client: kube::Client, engine: Arc<Engine>, node: String, pool: String, region: String, roles: Vec<String>) -> Ctx {
        let runtime_class = std::env::var("WS_RUNTIME_CLASS").ok().filter(|v| !v.is_empty());
        if let Some(rc) = &runtime_class {
            tracing::info!(runtime_class = %rc, "tenant pods will run sandboxed");
        }
        Ctx {
            client,
            engine,
            node,
            pool,
            api_service_account: std::env::var("WS_API_SERVICE_ACCOUNT")
                .unwrap_or_else(|_| "rustic-git-api".into()),
            api_namespace: std::env::var("WS_API_NAMESPACE").unwrap_or_else(|_| "kube-system".into()),
            runtime_class,
            running: Mutex::new(HashMap::new()),
            region,
            roles,
        }
    }
}

/// What a finished volume operation has to say about the pool, drained into status on a later pass.
#[derive(Debug, Default)]
pub struct Done {
    pub phase: Phase,
    /// The `PUSH_ANNOTATION` value this run answered, stamped back as `PUSH_SATISFIED_ANNOTATION`.
    pub push_at: Option<String>,
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
    let mine_bindings = mine.clone();
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
    // Unplaced objects, one watch per ROLE this node carries. `status.nodeName=` (empty) is a
    // legal field selector because the CRD declares `.status.nodeName` selectable — and the claim
    // is what moves the object out of this watch and into the node's own, with no poll in between.
    let unplaced = watcher::Config::default().fields("status.nodeName=");
    // ponytail: a node carrying BOTH labels (the dev `session-0`) runs both claim watches and so
    // races peers for Environments as well as Workspaces. The claim is atomic, so this is correct
    // rather than merely tolerated — the only consequence is that an Environment can land on the
    // session node. Single-label nodes are the intended production shape; if mixed nodes ever
    // become normal, the fix is a role check inside the claim, not here.
    let claim_ws = ctx.roles.iter().any(|r| r == "session").then(|| {
        Controller::new(Api::<crd::Workspace>::all(ctx.client.clone()), unplaced.clone())
            .shutdown_on_signal()
            .run(|w, c| async move { claim::claim_workspace(&w, &c).await }, error_policy, ctx.clone())
            .for_each(|r| async move {
                if let Err(e) = r {
                    tracing::warn!(error = %e, "workspace claim")
                }
            })
    });
    let bindings = Controller::new(Api::<crd::OwnerBinding>::all(ctx.client.clone()), mine_bindings)
        // A new Workspace of this owner may need a new TEAM namespace, so the binding reconciles
        // on it. Mapped by `spec.owner`, not by ownerReference: the binding is not the Workspace's
        // parent, it is the thing that makes its namespace exist.
        .watches(Api::<crd::Workspace>::all(ctx.client.clone()), watcher::Config::default(), {
            let region = ctx.region.clone();
            move |w: crd::Workspace| {
                Some(kube::runtime::reflector::ObjectRef::<crd::OwnerBinding>::new(&crd::binding_name(
                    &region,
                    &w.spec.owner,
                )))
            }
        })
        .shutdown_on_signal()
        .run(|b, c| async move { binding::apply_binding(&b, &c).await }, error_policy, ctx.clone())
        .for_each(|r| async move {
            if let Err(e) = r {
                tracing::warn!(error = %e, "ownerbinding reconcile")
            }
        });
    let claim_env = ctx.roles.iter().any(|r| r == "env").then(|| {
        Controller::new(Api::<crd::Environment>::all(ctx.client.clone()), unplaced)
            .shutdown_on_signal()
            .run(|e, c| async move { claim::claim_environment(&e, &c).await }, error_policy, ctx.clone())
            .for_each(|r| async move {
                if let Err(e) = r {
                    tracing::warn!(error = %e, "environment claim")
                }
            })
    });
    tokio::join!(
        volumes,
        workspaces,
        environments,
        bindings,
        futures::future::OptionFuture::from(claim_ws),
        futures::future::OptionFuture::from(claim_env),
    );
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
async fn heal_labels<K>(api: &Api<K>, obj: &K, owner: &str, team: &str, kind: &str) -> Result<(), ReconcileErr>
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

/// The push request this volume has not answered yet — the satisfied annotation carries the exact
/// requested timestamp rather than a wall clock of its own, because "which request did this push
/// answer" is the only question anything asks of it, and a metadata annotation does not bump
/// `metadata.generation` for `observedGeneration` to track.
fn push_pending(v: &crd::Volume) -> Option<String> {
    let requested = v.annotations().get(PUSH_ANNOTATION)?.clone();
    let satisfied = v.annotations().get(PUSH_SATISFIED_ANNOTATION);
    (satisfied.map(String::as_str) != Some(requested.as_str())).then_some(requested)
}

/// Stamped only after the push has actually landed, so a crash mid-push leaves the request pending.
async fn mark_push_satisfied(v: &crd::Volume, at: &str, ctx: &Arc<Ctx>) -> Result<(), ReconcileErr> {
    let api: Api<crd::Volume> = Api::all(ctx.client.clone());
    let patch = serde_json::json!({"metadata": {"annotations": {PUSH_SATISFIED_ANNOTATION: at}}});
    api.patch(&v.name_any(), &PatchParams::default(), &Patch::Merge(&patch)).await?;
    Ok(())
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
                    progress: None,
                    conditions: vec![],
                };
                st.conditions = vec![crd::condition("Ready", true, "Converged", "volume is materialized", gen)];
                if let Some(at) = &done.push_at {
                    mark_push_satisfied(v, at, ctx).await?;
                }
                write_volume_status(v, st, ctx).await?;
                Ok(Action::await_change())
            }
            // `observedGeneration` is deliberately NOT stamped: an unobserved generation is what
            // makes the next pass try again. Nothing is deleted, nothing is marked permanently
            // failed — the keep-biased rule, applied to the error path.
            Err(e) => {
                let st = crd::VolumeStatus {
                    phase: Phase::Error,
                    observed_generation: v.status.as_ref().and_then(|s| s.observed_generation),
                    subvolume_present: ctx.engine.pool.live(&v.name_any()).exists(),
                    lineage_tip: v.status.as_ref().and_then(|s| s.lineage_tip.clone()),
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
    // Read the git credential BEFORE the blocking task: the Secret read is an API call, and the
    // task has no client. Absent is not fatal — a public repo clones without one, and the git tier
    // is what decides.
    let git_token = match &v.spec.source {
        // ponytail: the credential Secret is gone from the schema; the init-container clone that
        // replaces this read is a later task, so the token is simply absent (public repos clone).
        Some(VolumeSource::GitRepo { .. }) => None,
        _ => None,
    };
    let handle = tokio::task::spawn_blocking(move || {
        volume_work(
            &engine,
            Work { id, owner, source, materialize, push: pending, message, git_token },
        )
    });
    ctx.running.lock().unwrap_or_else(|p| p.into_inner()).insert(uid, (gen, handle));
    write_volume_status(v, progressing(v, gen), ctx).await?;
    Ok(Action::requeue(TICK))
}

fn progressing(v: &crd::Volume, gen: i64) -> crd::VolumeStatus {
    let prev = v.status.clone().unwrap_or_default();
    crd::VolumeStatus {
        phase: Phase::Working,
        conditions: vec![crd::condition("Progressing", true, "Working", "btrfs operation in flight", gen)],
        ..prev
    }
}

/// One volume's whole unit of work, on its own OS thread with its own tiny current-thread runtime,
/// exactly as `run_job_blocking` did and for the same reason (see the module doc).
/// Everything one volume operation needs, as a struct rather than eight positional arguments —
/// `materialize`, `push` and `git_token` are all optional-ish and were trivially swappable at the
/// call site.
pub struct Work {
    pub id: String,
    pub owner: String,
    pub source: Option<VolumeSource>,
    pub materialize: bool,
    pub push: Option<String>,
    pub message: Option<String>,
    /// The git token for a `GitRepo` source, read from its Secret by the caller. Never logged and
    /// never formatted into an error: `git_clone` keeps it out of the argv for the same reason
    /// `merge_worker`'s `networked` does.
    pub git_token: Option<String>,
}

fn volume_work(engine: &Engine, w: Work) -> Result<Done, String> {
    let Work { id, owner, source, materialize, push, message, git_token } = w;
    let (id, owner) = (id.as_str(), owner.as_str());
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
                Some(VolumeSource::GitRepo { repo, branch, .. }) => {
                    engine.create_subvol(id).map_err(|e| e.to_string())?;
                    git_clone(repo, branch, git_token.as_deref(), &engine.pool.live(id))?;
                }
            }
        }
        let mut done = Done { phase: Phase::Ready, ..Done::default() };
        if let Some(at) = push {
            // `push_env` rather than `push`: the VOLUME is what gets pushed, and it is keyed by id
            // alone — the workspace or environment around it is not involved.
            let out = engine
                .push_env(owner, id, &serde_json::Value::Null, message.as_deref())
                .await
                .map_err(|e| e.to_string())?;
            done.lineage_tip = Some(out.layer);
            done.push_at = Some(at);
        }
        Ok(done)
    })
}

/// Clone a PLATFORM repository into a freshly created subvolume.
///
/// `repo` is `owner/name`, never a URL: the host comes from `WS_GIT_BASE`, so a caller cannot point
/// this at an arbitrary endpoint. That is the whole reason the field is not a URL — it would be an
/// egress primitive and an SSRF one, reachable by anybody who can create a workspace.
///
/// The token rides in `http.extraHeader` via `-c`, which puts it in the argv for the life of the
/// clone. Same accepted trade, and same hard rule, as `merge_worker`: it must never outlive the
/// process, so no error path here may carry the argv. `stderr` is git's own words, which do not
/// contain the header.
///
/// `--single-branch --depth 1`: a workspace wants the branch's tip to start from, not the history.
/// ponytail: shallow, so `git log` in the workspace shows one commit; deepen on demand if anyone
/// asks for the history they did not ask to clone.
fn git_clone(repo: &str, branch: &str, token: Option<&str>, into: &std::path::Path) -> Result<(), String> {
    let (owner, name) = repo.split_once('/').ok_or_else(|| format!("repo {repo:?} is not owner/name"))?;
    if !rustic_git_storage::store::valid_owner(owner) || !rustic_git_storage::store::valid_segment(name) {
        return Err(format!("repo {repo:?} is not a valid owner/name"));
    }
    let base = std::env::var("WS_GIT_BASE").unwrap_or_default();
    if base.is_empty() {
        return Err("WS_GIT_BASE is unset: cannot clone a platform repository".into());
    }
    let url = format!("{}/{owner}/{name}.git", base.trim_end_matches('/'));

    let mut cmd = std::process::Command::new("git");
    if let Some(t) = token {
        // Basic auth is what the git tier's HTTP surface takes; the username is ignored.
        let basic = base64_basic(t);
        cmd.args(["-c", &format!("http.extraHeader=Authorization: Basic {basic}")]);
    }
    cmd.args(["clone", "--depth", "1", "--single-branch", "--branch", branch, &url])
        .arg(into)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "");
    let out = cmd.output().map_err(|e| format!("git: {e}"))?;
    if !out.status.success() {
        // git's stderr only. NEVER the argv — it carries the credential.
        let why = String::from_utf8_lossy(&out.stderr);
        let last = why.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("clone failed");
        return Err(format!("cloning {repo} at {branch}: {last}"));
    }
    Ok(())
}

/// `base64("x-access-token:{token}")` — the shape git sends for HTTP basic auth.
fn base64_basic(token: &str) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{token}"))
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
pub(crate) fn conditions_eq(a: &[Condition], b: &[Condition]) -> bool {
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
        && a.progress == b.progress
        && conditions_eq(&a.conditions, &b.conditions)
}

/// An OPTIMISTIC status write: `replace_status` carrying the object's current
/// `metadata.resourceVersion`, so a concurrent writer makes this a 409.
///
/// The counterpart to `patch_status`, and the difference is the whole point. `patch_status` applies
/// FORCED, which is right for a write only one node can make (its own node's objects) and wrong for
/// the one write two nodes race: a forced apply has no precondition, never conflicts, and lets both
/// claimants believe they won. Use this for the claim; use `patch_status` for everything else.
///
/// It returns the raw `kube::Error` rather than a `ReconcileErr` so callers can branch on
/// `Api(s).code == 409` structurally — sniffing "409" out of a formatted string is how a message
/// change silently turns "a peer won" back into "retry forever". `?` still works from a reconcile,
/// via `From<kube::Error> for ReconcileErr`.
///
/// `status` must carry `phase`: the CRD schema declares it required, and a write without it is
/// rejected by the API server.
///
/// The body is the OBJECT AS FETCHED with its status replaced, because `replace_status` is a PUT of
/// a whole object and the object already carries the `metadata.resourceVersion` that makes the PUT
/// a precondition. The spec that rides along is ignored by the `/status` subresource — that is what
/// the subresource is for — so this still cannot edit desired state.
pub async fn replace_status<K>(api: &Api<K>, obj: &K, kind: &str, status: serde_json::Value) -> Result<(), kube::Error>
where
    K: Resource + Clone + serde::Serialize + serde::de::DeserializeOwned + std::fmt::Debug,
{
    let name = obj.meta().name.clone().unwrap_or_default();
    let mut body = serde_json::to_value(obj).map_err(kube::Error::SerdeError)?;
    body["apiVersion"] = serde_json::json!(format!("{}/{}", crd::GROUP, crd::VERSION));
    body["kind"] = serde_json::json!(kind);
    body["status"] = status;
    let next: K = serde_json::from_value(body).map_err(kube::Error::SerdeError)?;
    api.replace_status(&name, &PostParams::default(), &next).await?;
    Ok(())
}

/// Why a reconcile could not finish, and therefore what to do about it.
///
/// Today every failure is `Action::requeue(RETRY)`, which makes a spec that can never work look
/// exactly like a registry that is briefly down — the same line in the log, forever, at one a
/// minute. The new `storage.source` inputs make that untenable: a `cloneOf` naming a workspace that
/// does not exist, a `restoreOf` whose snapshot no `done` request carries, a Volume pinned to
/// another node — none of these get better by being retried.
pub enum Outcome {
    /// Nothing will change this without a new spec. Write the condition, stop.
    Permanent(String, &'static str),
    /// The world is briefly unavailable. Return `Err` and take `error_policy`'s backoff.
    Transient(ReconcileErr),
}

impl From<kube::Error> for Outcome {
    /// An API-server error is transient by default — a 5xx, a timeout, a lost connection. A 404 on
    /// a REFERENCE (a `cloneOf` source, say) is permanent, but only the caller knows which
    /// reference it was reading, so that classification is made at the call site, not here.
    fn from(e: kube::Error) -> Self {
        Outcome::Transient(ReconcileErr(e.to_string()))
    }
}

/// Turn an `Outcome` into the reconcile's answer, writing the condition on the permanent path.
///
/// `await_change()` on permanent, deliberately: the object is wrong and the next thing that can
/// help is a human or a new spec, both of which arrive as watch events.
///
/// `reason` is a CamelCase token, never a sentence — `meta/v1.Condition` requires it and
/// `kubectl wait --for=condition=…` matches on it. The `write` closure exists because each kind's
/// status has a different shape; every call site passes a one-line builder for its own status.
pub async fn settle<K, F>(
    outcome: Outcome,
    obj: &K,
    kind: &str,
    gen: i64,
    write: F,
    ctx: &Arc<Ctx>,
) -> Result<Action, ReconcileErr>
where
    K: Resource<DynamicType = ()> + ResourceExt + Clone + serde::de::DeserializeOwned + std::fmt::Debug,
    F: FnOnce(Condition) -> serde_json::Value,
{
    match outcome {
        Outcome::Permanent(msg, reason) => {
            tracing::warn!(kind = %kind, name = %obj.name_any(), reason = %reason, error = %msg, "permanent failure; not retrying");
            let cond = crd::condition("Ready", false, reason, &msg, gen);
            let api: Api<K> = Api::all(ctx.client.clone());
            patch_status(&api, &obj.name_any(), kind, write(cond)).await?;
            Ok(Action::await_change())
        }
        Outcome::Transient(e) => Err(e),
    }
}

/// Server-side apply on the `/status` subresource. Apply, not Merge: the field manager owns exactly
/// the status fields it sets, so two writers cannot silently clobber each other.
pub async fn patch_status<K>(api: &Api<K>, name: &str, kind: &str, status: serde_json::Value) -> Result<(), ReconcileErr>
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
    heal_labels(&Api::<crd::Workspace>::all(ctx.client.clone()), w, &w.spec.owner, &w.spec.team, "workspace").await?;
    let gen = w.meta().generation.unwrap_or(0);
    // Release 1 still reads the deprecated spec pointers; the migration onto status is a later
    // task, and an object written before it lands has nothing else to name its Volume with.
    let id = w.spec.volume_ref.clone().unwrap_or_default();
    let owner_ref = owner_ref_of_kind(w)?;
    let want_node = w.spec.node_name.clone().unwrap_or_default();
    let vol = match volume_node(&id, ctx, &want_node).await? {
        Ok(v) => v,
        Err(why) => {
            let st = crd::WorkspaceStatus {
                phase: crd::Phase::Error,
                observed_generation: None,
                conditions: vec![crd::condition("Degraded", true, "NodeMismatch", &why, gen)],
                ..w.status.clone().unwrap_or_default()
            };
            write_ws_status(w, st, ctx).await?;
            return Ok(Action::await_change());
        }
    };

    let ns = crd::ws_namespace(&w.spec.owner, &w.spec.team);
    // No ownerReference on the namespace: it is shared by every workspace this user owns IN THIS
    // TEAM, so deleting one would garbage-collect its siblings. See `crd::ws_namespace`.
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
    // Scope the API's Secret access to THIS namespace. See `k8s::api_secret_binding`: the
    // alternative is a cluster-wide `secrets: create` for the API, which would include the agent's
    // own credentials.
    ensure(
        &Api::<RoleBinding>::namespaced(ctx.client.clone(), &ns),
        &k8s::api_secret_binding(&ns, &w.spec.owner, &ctx.api_service_account, &ctx.api_namespace, None),
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
                    phase: crd::Phase::Creating,
                    // Unobserved on purpose: this generation has not converged, so the next pass
                    // re-runs instead of treating a Pending pod as done.
                    observed_generation: None,
                    pod_ref: Some(format!("{ns}/{id}")),
                    conditions: vec![crd::condition("Ready", false, "PodNotReady", "pod is not ready yet", gen)],
                    ..w.status.clone().unwrap_or_default()
                };
                write_ws_status(w, st, ctx).await?;
                return Ok(Action::requeue(TICK));
            }
            // `ready`, not `running`: this string is deserialized into `model::WsState` by the
            // `/v1` projection, which spells the running state `Ready`. An unknown phase does not
            // error — it falls back to `Creating`, so a healthy workspace showed "Creating" in the
            // UI forever. `phase_names_the_doc_enum` pins the vocabulary.
            (crd::Phase::Ready, Some(format!("{ns}/{id}")))
        }
        // Stopping IS deleting the pod — there is no policy the kubelet interprets. The subvolume
        // and its claim stay; only the compute goes away.
        DesiredState::Stopped => {
            delete_ignoring_404(&pods, &id).await?;
            (crd::Phase::Stopped, None)
        }
    };
    let st = crd::WorkspaceStatus {
        phase,
        observed_generation: Some(gen),
        pod_ref,
        conditions: vec![crd::condition("Ready", true, "Converged", "workspace matches spec", gen)],
        ..w.status.clone().unwrap_or_default()
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
    heal_labels(&Api::<crd::Environment>::all(ctx.client.clone()), e, &e.spec.owner, "", "environment").await?;
    let gen = e.meta().generation.unwrap_or(0);
    // Release 1 still reads the deprecated spec pointers; see `apply_workspace`.
    let id = e.spec.volume_ref.clone().unwrap_or_default();
    let owner_ref = owner_ref_of_kind(e)?;
    let want_node = e.spec.node_name.clone().unwrap_or_default();
    let vol = match volume_node(&id, ctx, &want_node).await? {
        Ok(v) => v,
        Err(why) => {
            let st = crd::EnvironmentStatus {
                phase: crd::Phase::Error,
                observed_generation: None,
                service_status: vec![],
                conditions: vec![crd::condition("Degraded", true, "NodeMismatch", &why, gen)],
                ..e.status.clone().unwrap_or_default()
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
            phase: crd::Phase::Stopped,
            observed_generation: Some(gen),
            service_status: vec![],
            conditions: vec![crd::condition("Ready", true, "Stopped", "pushed and stopped", gen)],
            ..e.status.clone().unwrap_or_default()
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
    // An environment's services are the likeliest place a private image appears, so this namespace
    // needs the same scoped grant a workspace namespace gets — the API writes the pull credential
    // here, and nowhere it has not been vouched for.
    ensure(
        &Api::<RoleBinding>::namespaced(ctx.client.clone(), &ns),
        &k8s::api_secret_binding(&ns, &e.spec.owner, &ctx.api_service_account, &ctx.api_namespace, None),
    )
    .await?;
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
        phase: crd::Phase::Running,
        // Not converged until every service is: leaving it unobserved is what makes the next pass
        // look again instead of declaring a half-up environment finished.
        observed_generation: all_ready.then_some(gen),
        service_status,
        conditions: vec![if all_ready {
            crd::condition("Ready", true, "Converged", "environment matches spec", gen)
        } else {
            crd::condition("Ready", false, "ServicesNotReady", "one or more services are not ready", gen)
        }],
        ..e.status.clone().unwrap_or_default()
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
    let satisfied = vol.annotations().get(PUSH_SATISFIED_ANNOTATION).cloned();
    match requested {
        Some(r) if satisfied.as_deref() == Some(r.as_str()) => Ok(None),
        Some(_) => {
            let st = crd::EnvironmentStatus {
                // Still `running`: the deployments ARE up until the push lands, and
                // `model::EnvState` has no `Stopping` — an unknown phase silently becomes
                // `Creating`, which is both wrong and alarming. Progress belongs in the condition
                // below, which is where a reader looks for it.
                phase: crd::Phase::Running,
                observed_generation: None,
                service_status: vec![],
                conditions: vec![crd::condition("Progressing", true, "PushBeforeStop", "waiting for the volume's push", gen)],
                ..e.status.clone().unwrap_or_default()
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
pub(crate) async fn ensure<K>(api: &Api<K>, obj: &K) -> Result<(), ReconcileErr>
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
