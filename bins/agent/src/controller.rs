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

use crate::{binding, claim, snapshot};
use futures::StreamExt;
use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::core::v1::{LimitRange, Namespace, Pod, Service};
use k8s_openapi::api::networking::v1::NetworkPolicy;
use k8s_openapi::api::rbac::v1::RoleBinding;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, OwnerReference};
use kube::api::{Patch, PatchParams, PostParams};
use kube::runtime::controller::{Action, Controller};
use kube::runtime::finalizer::{finalizer, Event as FinalizerEvent};
use kube::runtime::watcher;
use kube::{Api, Resource, ResourceExt};
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

/// The API tier's identity, which the per-namespace Secret grant names. Hard-coded: the API is
/// deployed by the manifests in `deploy/`, which name exactly these.
pub(crate) const API_SERVICE_ACCOUNT: &str = "rustic-git-api";
pub(crate) const API_NAMESPACE: &str = "kube-system";

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
    /// `WS_DEFAULT_IMAGE`: the tagged platform image behind `model::DEFAULT_WS_IMAGE`.
    pub default_image: String,
    /// In-flight long btrfs operations, keyed by the uid of the object that asked for them (a
    /// `Volume` being materialized, or a `SnapshotRequest` being pushed). THE idempotency guard,
    /// and a local in-memory check rather than a distributed lease because exactly one agent ever
    /// reconciles a given object: for a `Volume` that is the `spec.nodeName` field selector on the
    /// watch, and for a `SnapshotRequest` — which names no node — it is `snapshot::my_volume`,
    /// which acts only when the named Volume's `spec.nodeName` is this one.
    pub running: Mutex<InFlight>,
    /// A finished operation wakes its own reconciler instead of waiting out the `TICK` requeue: a
    /// local clone's btrfs work takes under a second, and without this the object sat `progressing`
    /// for the rest of the 15s tick because nothing but the clock ever looked at the handle again.
    /// The requeue stays as the backstop — a dropped send costs a tick, never the object.
    pub wake_volume: tokio::sync::mpsc::UnboundedSender<kube::runtime::reflector::ObjectRef<crd::Volume>>,
    pub wake_snapshot: tokio::sync::mpsc::UnboundedSender<kube::runtime::reflector::ObjectRef<crd::SnapshotRequest>>,
    /// The same, for a finished Nix profile build — without it a workspace waits out the tick with
    /// its pod ungated on a profile that is already on disk.
    pub wake_workspace: tokio::sync::mpsc::UnboundedSender<kube::runtime::reflector::ObjectRef<crd::Workspace>>,
    /// The receiving halves, until `run` takes them and feeds each `Controller::reconcile_on`.
    #[allow(clippy::type_complexity)]
    pub wakes: Mutex<
        Option<(
            tokio::sync::mpsc::UnboundedReceiver<kube::runtime::reflector::ObjectRef<crd::Volume>>,
            tokio::sync::mpsc::UnboundedReceiver<kube::runtime::reflector::ObjectRef<crd::SnapshotRequest>>,
            tokio::sync::mpsc::UnboundedReceiver<kube::runtime::reflector::ObjectRef<crd::Workspace>>,
        )>,
    >,
    /// Where `gitRepo` seeding clones from and with what. `WS_GIT_BASE` and the agent-side clone
    /// are gone: the clone happens inside the pod, over SSH, as the owner.
    pub git_ssh_host: String,
    pub git_ssh_port: String,
    pub git_init_image: String,
    /// This node's region, from `WS_REGION` — the other half of an `OwnerBinding`'s identity.
    pub region: String,
    /// The roles this node carries, read ONCE from its own `Node` labels at startup
    /// (`rustic-git.io/session`, `rustic-git.io/env`). A second, hand-maintained copy of a label
    /// the scheduler already reads is a second thing that can be wrong — see `k8s::placement`.
    pub roles: Vec<String>,
    /// The one Nix client, behind a trait so the reconciler is tested with a fake instead of a
    /// real daemon and store.
    pub nix: Arc<dyn crate::nix::Nix>,
    /// The spec hash each in-flight profile build was STARTED from, keyed like `running`. Without
    /// it a spec edit during a build is lost: the finished build is published as if it were the
    /// new spec, stamped with the new hash, and never rebuilt.
    pub profile_builds: Mutex<HashMap<String, String>>,
    /// Where this node's per-workspace profile links live (`nix::PROFILES_DIR` in production). A
    /// field and not a global so a test can point it at a tempdir without racing every other test.
    pub profiles_dir: std::path::PathBuf,
    /// Makes a workspace's SSH host key. Behind a trait so tests never shell out to `ssh-keygen`.
    /// This node's Volumes, from the ONE shared watch `run` opens. Four controllers used to open
    /// their own `spec.nodeName` watch on the same objects, and the snapshot reconciler GETted the
    /// Volume for every request in the cluster — this store is both answers.
    pub volumes: kube::runtime::reflector::Store<crd::Volume>,
    /// The writing half, until `run` takes it and drives the watch into it (tests feed it directly).
    pub volume_writer: Mutex<Option<kube::runtime::reflector::store::Writer<crd::Volume>>>,
    /// What `ensure` last applied, by kind/namespace/name: the hash of the desired object and when.
    /// A converged parent reconciles on every child event and re-applied ~10 objects each time;
    /// an apply whose body has not changed is skipped. See `ensure` for the ceiling.
    pub applied: Mutex<HashMap<String, (u64, std::time::Instant)>>,
    /// `WS_COMMIT_MODEL=1`, read ONCE at construction. A field, not a live env read, so every
    /// commit-model gate in this crate agrees on the same value for the life of the process and a
    /// test can flip it on `Ctx` directly instead of mutating process-global env — the mutation
    /// was flaking the suite when tests ran in parallel (F3).
    pub commit_model: bool,
}

impl Ctx {
    #[allow(clippy::too_many_arguments)]
    pub fn new(client: kube::Client, engine: Arc<Engine>, node: String, pool: String, region: String, roles: Vec<String>, nix: Arc<dyn crate::nix::Nix>, profiles_dir: std::path::PathBuf) -> Ctx {
        // Required, not defaulted: a workspace that names no image runs THIS, and an agent that
        // silently fell back to `:latest` would move every workspace on its next restart.
        let default_image = std::env::var("WS_DEFAULT_IMAGE").ok().filter(|v| !v.is_empty())
            .unwrap_or_else(|| panic!("WS_DEFAULT_IMAGE is required: the pinned image a workspace without one runs"));
        let runtime_class = std::env::var("WS_RUNTIME_CLASS").ok().filter(|v| !v.is_empty());
        if let Some(rc) = &runtime_class {
            tracing::info!(runtime_class = %rc, "tenant pods will run sandboxed");
        }
        // Unbounded on purpose: the only senders are this agent's own finished operations, one
        // wake each, so the queue can never hold more than the operations in flight.
        let (wake_volume, vol_rx) = tokio::sync::mpsc::unbounded_channel();
        let (wake_snapshot, snap_rx) = tokio::sync::mpsc::unbounded_channel();
        let (wake_workspace, ws_rx) = tokio::sync::mpsc::unbounded_channel();
        // 256 is the dispatcher's per-subscriber buffer, not a cap on volumes: a subscriber that
        // stops polling stalls the reflector once it fills, and every subscriber here is a
        // controller polled by the same `join!`.
        let (volumes, volume_writer) = kube::runtime::reflector::store_shared(256);
        Ctx {
            volumes,
            volume_writer: Mutex::new(Some(volume_writer)),
            applied: Mutex::new(HashMap::new()),
            commit_model: std::env::var("WS_COMMIT_MODEL").ok().as_deref() == Some("1"),
            wake_volume,
            wake_snapshot,
            wake_workspace,
            wakes: Mutex::new(Some((vol_rx, snap_rx, ws_rx))),
            client,
            engine,
            node,
            pool,
            git_ssh_host: std::env::var("WS_GIT_SSH_HOST").unwrap_or_else(|_| "git.khost.dev".into()),
            git_ssh_port: std::env::var("WS_GIT_SSH_PORT").unwrap_or_else(|_| "22".into()),
            git_init_image: std::env::var("WS_GIT_INIT_IMAGE").unwrap_or_else(|_| "alpine/git:2.45.2".into()),
            runtime_class,
            default_image,
            running: Mutex::new(HashMap::new()),
            region,
            roles,
            nix,
            profiles_dir,
            profile_builds: Mutex::new(HashMap::new()),
        }
    }
}

impl Ctx {
    /// Put a Volume in the shared store by hand. For tests, which have no watch to feed it; a
    /// no-op once `run` has taken the writer, because then the watch is the only writer.
    pub fn remember_volume(&self, v: crd::Volume) {
        if let Some(w) = self.volume_writer.lock().unwrap_or_else(|p| p.into_inner()).as_mut() {
            w.apply_watcher_event(&watcher::Event::Apply(v));
        }
    }
}

/// What a finished volume operation has to say about the pool, drained into status on a later pass.
#[derive(Debug, Default)]
pub struct Done {
    pub phase: Phase,
    pub lineage_tip: Option<String>,
    /// The snapshot an in-place restore materialized, echoed into `status.restoredTo` — the field
    /// both this controller and the parent read to tell "already done" from "not yet".
    pub restored_to: Option<String>,
    /// Why `spec.quotaGb` is NOT enforced on disk, when it is not — surfaced as `QuotaEnforced`
    /// rather than failing the volume: a pool without qgroups is the operator's to fix, and a
    /// usable-but-uncapped volume beats an unusable one.
    pub quota_unenforced: Option<String>,
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

/// Map a namespaced child back to its CLUSTER-SCOPED owner.
///
/// `Controller::owns` cannot be used here. It derives the parent's `ObjectRef` from the child's
/// owner reference AND the child's namespace — correct when parent and child share a namespace,
/// wrong when the parent is cluster-scoped. It produced refs like
/// `Environment.../env-abc.env-env-abc` and every reconcile triggered by a child event then failed
/// with "not found in local store", so an environment converged once on creation and never
/// responded to its StatefulSets changing again.
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

/// An mpsc receiver as the `Stream` `reconcile_on` wants — `futures` already has the adapter, so
/// this costs no dependency.
fn wake_stream<T: Send + 'static>(
    rx: tokio::sync::mpsc::UnboundedReceiver<T>,
) -> impl futures::Stream<Item = T> + Send + 'static {
    futures::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|v| (v, rx)) })
}

/// Count and time one reconcile per kind. Wrapped here, at the five `.run` sites, rather than
/// inside each reconciler: the reconcilers return early from many places, and this is the one
/// spot that sees every exit.
async fn timed<T, E>(kind: &'static str, fut: impl std::future::Future<Output = Result<T, E>>) -> Result<T, E> {
    let start = std::time::Instant::now();
    let r = fut.await;
    let result = if r.is_ok() { "ok" } else { "error" };
    metrics::counter!("reconciles_total", "kind" => kind, "result" => result).increment(1);
    metrics::histogram!("reconcile_duration_seconds", "kind" => kind).record(start.elapsed().as_secs_f64());
    r
}

pub async fn run(ctx: Arc<Ctx>) -> Result<(), String> {
    // Before the watches: a node with nothing to do must still be able to prove it is alive.
    heartbeat(&ctx.pool);
    spawn_heartbeat(ctx.clone());
    spawn_home_push(ctx.clone());
    spawn_replicate(ctx.clone());
    spawn_pull(ctx.clone());
    // NB the RBAC grant is cluster-wide — a field selector narrows a watch, never authorization.
    let mine = watcher::Config::default().fields(&format!("spec.nodeName={}", ctx.node));
    // The completion wake-ups (see `wake_on_finish`). Taken once; a second `run` on one Ctx would
    // be two agents in one process, which is not a thing.
    let (vol_wakes, snap_wakes, ws_wakes) =
        ctx.wakes.lock().unwrap_or_else(|p| p.into_inner()).take().ok_or("the wake channels are already taken")?;
    // ONE watch on this node's Volumes, shared. The Volume controller reconciles from it, and the
    // three parents that wait on a Volume's status subscribe to it — four watches on the same
    // objects was N_nodes × 4 long-running requests on the API server for one stream of events.
    let writer =
        ctx.volume_writer.lock().unwrap_or_else(|p| p.into_inner()).take().ok_or("the volume writer is already taken")?;
    let subscribe = || writer.subscribe().ok_or("the volume store is not shared");
    let (vol_self, vol_ws, vol_env, vol_snap) = (subscribe()?, subscribe()?, subscribe()?, subscribe()?);
    let volume_watch = {
        use kube::runtime::{watcher, WatchStreamExt};
        watcher(Api::<crd::Volume>::all(ctx.client.clone()), mine.clone())
            .default_backoff()
            // `reflect_shared`, not `reflect`: a shared writer only DISPATCHES to its subscribers
            // (the Volume controller and the three parents) from the shared variant — plain
            // `reflect` fills the store and tells nobody, which is a Volume controller that
            // never reconciles a new Volume.
            .reflect_shared(writer)
            .touched_objects()
            .for_each(|r| async move {
                if let Err(e) = r {
                    tracing::warn!(error = %e, "volume watch")
                }
            })
    };
    let volumes = Controller::for_shared_stream(vol_self, ctx.volumes.clone())
        .reconcile_on(wake_stream(vol_wakes))
        .shutdown_on_signal()
        .run(|v, c| timed("volume", reconcile_volume(v, c)), error_policy, ctx.clone())
        .for_each(|r| async move {
            if let Err(e) = r {
                tracing::warn!(error = %e, "volume reconcile")
            }
        });
    // Placement is a status fact now, so the node's own Workspaces and Environments are selected
    // by `status.nodeName` — `mine` (`spec.nodeName`) stays for the kinds the API still places.
    let placed = watcher::Config::default().fields(&format!("status.nodeName={}", ctx.node));
    // Label-selected, not every Pod in the cluster: a controller that streams every pod event in
    // the cluster to filter for its own is the cheapest way to peg an API server.
    let our_pods = watcher::Config::default().labels(&format!("{}=workspace", k8s::KIND_LABEL));
    let workspaces = Controller::new(Api::<crd::Workspace>::all(ctx.client.clone()), placed.clone())
        .reconcile_on(wake_stream(ws_wakes))
        .watches(Api::<Pod>::all(ctx.client.clone()), our_pods, |p| owned_by::<crd::Workspace, _>(&p))
        // The parent acts on the child's STATUS, so it must wake when that status moves — the 15s
        // requeue is the backstop, never the mechanism. Scoped to this node's Volumes: the child is
        // authored on the parent's node, so a Volume elsewhere can never own a Workspace here.
        //
        // ponytail: a CLONE also waits on its source Workspace's placement, which no ownerReference
        // carries, and it converges on the 15s tick rather than a watch — the fan-out (source →
        // every clone of it) needs a reflector store indexed by `storage.source.cloneOf`, and the
        // mapper is a sync `FnMut` that must not do I/O. Wire the store if that latency is felt.
        .watches_shared_stream(vol_ws, |v: Arc<crd::Volume>| owned_by::<crd::Workspace, _>(&*v))
        // The `stop-home-{ws}` request the stop path waits on, selected by the same `stop-of`
        // label the environments use and mapped back by ownerReference — without it a workspace
        // parked at `StopSnapshotFailed` would never wake, not even for an operator deleting the
        // failed request.
        .watches(
            Api::<crd::SnapshotRequest>::all(ctx.client.clone()),
            watcher::Config::default().labels(crd::STOP_LABEL),
            |r| owned_by::<crd::Workspace, _>(&r),
        )
        .shutdown_on_signal()
        .run(|w, c| timed("workspace", async move { apply_workspace(&w, &c).await }), error_policy, ctx.clone())
        .for_each(|r| async move {
            if let Err(e) = r {
                tracing::warn!(error = %e, "workspace reconcile")
            }
        });
    let mine_bindings = mine.clone();
    // Label-selected like the pods: every StatefulSet in the cluster is not this controller's.
    let env_sets = watcher::Config::default().labels(&format!("{}=environment", k8s::KIND_LABEL));
    let env_pods = env_sets.clone();
    let environments = Controller::new(Api::<crd::Environment>::all(ctx.client.clone()), placed.clone())
        .watches(Api::<StatefulSet>::all(ctx.client.clone()), env_sets, |d| owned_by::<crd::Environment, _>(&d))
        // A restore waits for the service pods to be GONE, not scaled down — and the StatefulSet
        // stops reporting a terminating pod the moment it is marked for deletion, seconds before
        // the process has exited. That wake arrives too early, and without this one the drain
        // would sit out the full requeue tick. The pod's owner is a ReplicaSet, so the namespace
        // is the link — and `crd::env_namespace` makes it the Environment's own name.
        .watches(Api::<Pod>::all(ctx.client.clone()), env_pods, |p| {
            Some(kube::runtime::reflector::ObjectRef::<crd::Environment>::new(p.metadata.namespace.as_deref()?))
        })
        // The env's own Volume child: it waits on that child's STATUS, so it must wake when the
        // status moves. Scoped to this node's Volumes — the child is authored on the parent's node.
        .watches_shared_stream(vol_env, |v: Arc<crd::Volume>| owned_by::<crd::Environment, _>(&*v))
        // The `stop-{env}` snapshot, which the stop path waits on. Its ownerReference is the link:
        // an environment parked at `StopSnapshotFailed` returns `await_change`, so without this
        // watch nothing would ever wake it — not even the operator deleting the failed request.
        // Selected by the `stop-of` label the stop path stamps, so a node does not stream every
        // user push in the cluster to find the handful of stop requests that are its own.
        .watches(
            Api::<crd::SnapshotRequest>::all(ctx.client.clone()),
            watcher::Config::default().labels(crd::STOP_LABEL),
            |r| owned_by::<crd::Environment, _>(&r),
        )
        .shutdown_on_signal()
        .run(|e, c| timed("environment", async move { apply_environment(&e, &c).await }), error_policy, ctx.clone())
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
            .run(|w, c| timed("claim", async move { claim::claim_workspace(&w, &c).await }), error_policy, ctx.clone())
            .for_each(|r| async move {
                if let Err(e) = r {
                    tracing::warn!(error = %e, "workspace claim")
                }
            })
    });
    let bindings = Controller::new(Api::<crd::OwnerBinding>::all(ctx.client.clone()), mine_bindings)
        // A new Workspace of this owner may need a new TEAM namespace, so the binding reconciles
        // on it. Mapped by `spec.owner`, not by ownerReference: the binding is not the Workspace's
        // parent, it is the thing that makes its namespace exist. Only the ones placed HERE: the
        // claim binds an owner's workspaces to the binding's node, so one elsewhere is never ours.
        .watches(Api::<crd::Workspace>::all(ctx.client.clone()), placed, {
            let region = ctx.region.clone();
            move |w: crd::Workspace| {
                Some(kube::runtime::reflector::ObjectRef::<crd::OwnerBinding>::new(&crd::binding_name(
                    &region,
                    &w.spec.owner,
                )))
            }
        })
        .shutdown_on_signal()
        .run(|b, c| timed("binding", async move { binding::apply_binding(&b, &c).await }), error_policy, ctx.clone())
        .for_each(|r| async move {
            if let Err(e) = r {
                tracing::warn!(error = %e, "ownerbinding reconcile")
            }
        });
    // No `mine`: the request carries no node (a node is a controller-owned fact and the API does
    // not copy facts into spec), so ownership is resolved per-object from the named Volume.
    // ponytail: every agent streams every request — two nodes today, so the fan-out is two. A
    // `spec.volume`-indexed reflector is the upgrade if the request count ever makes this hot.
    let snapshots = Controller::new(Api::<crd::SnapshotRequest>::all(ctx.client.clone()), watcher::Config::default());
    // The controller's OWN reflector store, not a second one: it is already populated by the watch
    // above, so the mapper below is a synchronous scan of memory with no I/O — which is all a
    // `watches` mapper is allowed to be.
    let requests = snapshots.store();
    spawn_snapshot_gc(ctx.clone(), requests.clone());
    let snapshots = snapshots
        .reconcile_on(wake_stream(snap_wakes))
        // A request created before its Volume is placed waits, and this is what wakes it. `Volume`
        // and `SnapshotRequest` share no name and no ownerReference — `spec.volume` is the only
        // link — so the store is what turns one Volume event into the requests that named it.
        .watches_shared_stream(vol_snap, move |v: Arc<crd::Volume>| requests_naming(&requests.state(), &v.name_any()))
        .shutdown_on_signal()
        .run(snapshot::reconcile_snapshot, error_policy, ctx.clone())
        .for_each(|r| async move {
            if let Err(e) = r {
                tracing::warn!(error = %e, "snapshot reconcile")
            }
        });
    // The new `Snapshot` kind: no finalizer (see `snapshot::reconcile_commit`'s module doc), so a
    // plain watch over every one in the cluster is enough — same fan-out shape as `snapshots`
    // above, and the same ponytail note applies if the commit count ever makes this hot.
    let commits = Controller::new(Api::<crd::Snapshot>::all(ctx.client.clone()), watcher::Config::default())
        .shutdown_on_signal()
        .run(|s, c| timed("commit", async move { snapshot::reconcile_commit(s, c).await }), error_policy, ctx.clone())
        .for_each(|r| async move {
            if let Err(e) = r {
                tracing::warn!(error = %e, "commit reconcile")
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
        volume_watch,
        volumes,
        workspaces,
        environments,
        bindings,
        snapshots,
        commits,
        futures::future::OptionFuture::from(claim_ws),
        futures::future::OptionFuture::from(claim_env),
    );
    Ok(())
}

/// Every request in the store that names this volume, as refs to reconcile.
///
/// Split out of the `watches` mapper only so it is testable without a live reflector; the mapper
/// itself must stay a synchronous scan of memory, which is what this is.
pub fn requests_naming(
    requests: &[Arc<crd::SnapshotRequest>],
    volume: &str,
) -> Vec<kube::runtime::reflector::ObjectRef<crd::SnapshotRequest>> {
    requests
        .iter()
        .filter(|r| r.spec.volume == volume)
        .map(|r| kube::runtime::reflector::ObjectRef::<crd::SnapshotRequest>::new(&r.name_any()))
        .collect()
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

/// A `done` push request is a receipt, and receipts pile up: an hourly-pushing workspace leaves ~9k
/// objects a year in etcd and in every agent's reflector. Deleting the request deletes no data —
/// the record is in the registry and the listings read it from there — so the receipt is kept this
/// long for `kubectl` and then reclaimed by the node that owns the volume.
pub const SNAPSHOT_REQUEST_TTL: Duration = Duration::from_secs(7 * 24 * 3600);

/// Once an hour, on the OWNING node only (the one whose shared store holds the request's Volume):
/// two nodes deleting the same object is harmless, but two nodes deciding is the multi-writer
/// shape the design removes, and "mine" is already answered by the store.
fn spawn_snapshot_gc(ctx: Arc<Ctx>, requests: kube::runtime::reflector::Store<crd::SnapshotRequest>) {
    tokio::spawn(async move {
        let api: Api<crd::SnapshotRequest> = Api::all(ctx.client.clone());
        let mut tick = tokio::time::interval(Duration::from_secs(3600));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let now = k8s_openapi::jiff::Timestamp::now();
            let mine = |vol: &str| ctx.volumes.get(&kube::runtime::reflector::ObjectRef::new(vol)).is_some();
            for name in expired_requests(&requests.state(), mine, now, SNAPSHOT_REQUEST_TTL) {
                if let Err(e) = delete_ignoring_404(&api, &name).await {
                    tracing::warn!(request = %name, error = %e, "snapshot request gc");
                }
            }
        }
    });
}

/// The `done` requests older than `ttl` whose Volume `mine` claims. Keep-biased on every doubt: an
/// unparseable `at`, a request already terminating, or one with an owner (the `stop-{env}`
/// request, whose teardown deletes it and whose absence means "push again") is left alone.
pub fn expired_requests(
    requests: &[Arc<crd::SnapshotRequest>],
    mine: impl Fn(&str) -> bool,
    now: k8s_openapi::jiff::Timestamp,
    ttl: Duration,
) -> Vec<String> {
    requests
        .iter()
        .filter(|r| r.metadata.deletion_timestamp.is_none() && r.metadata.owner_references.is_none())
        .filter(|r| r.status.as_ref().is_some_and(|s| s.phase == Phase::Done))
        .filter(|r| {
            // `status.at` is when the record landed; `creationTimestamp` is the older fallback for
            // a request written before `at` existed. Neither readable keeps the object.
            let at = r
                .status
                .as_ref()
                .and_then(|s| s.at.as_deref())
                .and_then(|a| a.parse::<k8s_openapi::jiff::Timestamp>().ok())
                .or_else(|| r.metadata.creation_timestamp.as_ref().map(|t| t.0));
            at.is_some_and(|at| (now.as_second() - at.as_second()).max(0) as u64 >= ttl.as_secs())
        })
        .filter(|r| mine(&r.spec.volume))
        .map(|r| r.name_any())
        .collect()
}

/// What the timer's pushes say in `history`. They bypass `SnapshotRequest` on purpose: they are
/// the agent's housekeeping, not something anyone asked for, and a request object per five minutes
/// per person would be noise in the listings.
pub const HOME_PUSH_MESSAGE: &str = "home: periodic";

/// `WS_HOME_PUSH_SECS`, default 300: how often this node pushes the homes whose disk moved. An
/// unchanged home costs one `subvolume show` per beat.
pub fn home_push_interval() -> Duration {
    Duration::from_secs(std::env::var("WS_HOME_PUSH_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(300))
}

/// The homes on this node due for a push, decided from two per-volume numbers so the decision is
/// testable without a filesystem: `generation` is what btrfs says now (`None`: the subvolume is
/// absent or unreadable — skipped, never pushed blind), `pushed` is the generation of the last
/// push's own snapshot (`None`: never pushed — so a home that exists gets its first record on the
/// next beat). Strictly PAST it, not merely different: the snapshot's transaction can leave the
/// live number at or behind the snapshot's, and neither of those is a change to push.
pub fn homes_to_push(
    volumes: &[Arc<crd::Volume>],
    generation: impl Fn(&str) -> Option<u64>,
    pushed: impl Fn(&str) -> Option<u64>,
) -> Vec<Arc<crd::Volume>> {
    volumes
        .iter()
        .filter(|v| crd::is_home_volume(v) && volume_is_ready(v) && v.metadata.deletion_timestamp.is_none())
        .filter(|v| {
            let id = v.name_any();
            match (generation(&id), pushed(&id)) {
                (None, _) => false,
                (Some(now), Some(then)) => now > then,
                (Some(_), None) => true,
            }
        })
        .cloned()
        .collect()
}

/// The timer (spec: "Replication, trigger 1"). Every home is pushed from the agent's own beat and
/// nothing else: inside a region there is one node per person, so no two nodes ever push one home.
/// The sender half of replication (spec: "Replication, trigger 1"'s sibling for standby copies):
/// its own beat, same shape as `spawn_home_push`, so a slow send never delays a reconcile and a
/// reconcile never waits on a send.
fn spawn_replicate(ctx: Arc<Ctx>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(crate::peer::replica_interval());
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            crate::peer::replicate_beat(&ctx).await;
        }
    });
}

/// The commit model's puller: its own beat, same interval and tick shape as `spawn_replicate` —
/// `pull_beat` itself is the inert-until-`WS_COMMIT_MODEL=1` gate, so this always spawns and costs
/// nothing until the cutover flag is set.
fn spawn_pull(ctx: Arc<Ctx>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(crate::peer::replica_interval());
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            crate::peer::pull_beat(&ctx).await;
        }
    });
}

fn spawn_home_push(ctx: Arc<Ctx>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(home_push_interval());
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let ctx = ctx.clone();
            // Its own OS thread: `push_env` blocks on the volume's `flock`, and `subvolume show`
            // is a process per home. Same rule as every btrfs operation in this file.
            if let Err(e) = tokio::task::spawn_blocking(move || home_push_beat(&ctx)).await {
                tracing::warn!(error = %e, "home push beat panicked; skipping it");
            }
        }
    });
}

fn home_push_beat(ctx: &Arc<Ctx>) {
    let engine = &ctx.engine;
    if let Err(e) = engine.sync_pool() {
        tracing::warn!(error = %e, "home push: btrfs sync; skipping the beat");
        return;
    }
    let due = homes_to_push(&ctx.volumes.state(), |id| engine.generation(id).ok(), |id| engine.pool.pushed_gen(id));
    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            tracing::warn!(error = %e, "home push: runtime");
            return;
        }
    };
    for v in due {
        let id = v.name_any();
        // Not under a reconcile-owned operation on this volume (a materialize, a restore): the
        // `running` map is the single-flight guard for those, and the flock would only make this
        // beat wait for them anyway. A `SnapshotRequest` push (the stop) is keyed by its own uid
        // and is serialised by the flock; a beat right behind it pushes an identical tree once.
        // ponytail: that duplicate is one extra record per stop-then-beat coincidence; comparing
        // the generation again after the flock is the fix if `history` ever looks noisy.
        if running_contains(ctx, &v.uid().unwrap_or_default()) {
            continue;
        }
        match rt.block_on(engine.push_env(&v.spec.owner, &id, &serde_json::Value::Null, Some(HOME_PUSH_MESSAGE))) {
            Ok(out) => {
                // The SNAPSHOT's generation, not live's read afterwards: a write that lands between
                // the snapshot and such a read would be folded into the recorded number and never
                // pushed by the timer — only the stop push would catch it, and a lost node loses
                // it. The snapshot's own number bounds exactly what it holds.
                match engine.pushed_generation(&id, &out.layer) {
                    Ok(g) => {
                        if let Err(e) = engine.pool.record_pushed_gen(&id, g) {
                            tracing::warn!(volume = %id, error = %e, "home push: recording the generation");
                        }
                    }
                    Err(e) => tracing::warn!(volume = %id, error = %e, "home push: the snapshot's generation"),
                }
                metrics::counter!("home_pushes_total", "result" => "ok").increment(1);
            }
            // Logged and retried next beat; the subvolume is untouched (spec: failure modes).
            Err(e) => {
                tracing::warn!(volume = %id, error = %e, "home push failed; retrying next beat");
                metrics::counter!("home_pushes_total", "result" => "error").increment(1);
            }
        }
    }
}

/// Wrap a blocking operation's handle so that finishing also wakes the reconciler.
///
/// The map keeps the `JoinHandle` semantics it always had — this is still one handle per uid,
/// still drained by the pass that observes it — the only addition is the send. The wake goes out
/// as the wrapper's last act, so a reconcile that arrives in the sliver before the task is marked
/// finished simply sees "still running" and falls back on the `TICK` requeue, as it does today.
pub fn wake_on_finish<T: Send + 'static>(
    inner: tokio::task::JoinHandle<Result<Done, String>>,
    tx: tokio::sync::mpsc::UnboundedSender<T>,
    msg: T,
) -> tokio::task::JoinHandle<Result<Done, String>> {
    tokio::spawn(async move {
        let out = inner.await.unwrap_or_else(|e| Err(format!("operation panicked: {e}")));
        // A closed receiver means the controller is shutting down; the requeue covers it.
        let _ = tx.send(msg);
        out
    })
}

pub fn running_contains(ctx: &Arc<Ctx>, uid: &str) -> bool {
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

pub async fn apply_volume(v: &crd::Volume, ctx: &Arc<Ctx>) -> Result<Action, ReconcileErr> {
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

    // 1. Nothing asked for. Pushing is a `SnapshotRequest` with its own reconciler now, so a
    //    materialized volume at its current generation has nothing left for this pass to do.
    if observed && !running_contains(ctx, &uid) {
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
    let home = crd::is_home_volume(v);
    let handle = tokio::task::spawn_blocking(move || {
        volume_work(&engine, Work { id, owner, source, materialize, restore, quota_gb, home })
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
    use rustic_git_workspaces::engine::ops::{FETCH_FAILED, NO_SUCH_RECORD, REGION_UNREACHABLE};
    // Region first: a cross-region restore with no credentials also cannot fetch, and naming the
    // missing credentials is the actionable half.
    [(REGION_UNREACHABLE, "RegionUnreachable"), (NO_SUCH_RECORD, "NoSuchSnapshot"), (FETCH_FAILED, "FetchFailed")]
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

/// Everything one volume operation needs, as a struct rather than positional arguments that were
/// trivially swappable at the call site.
pub struct Work {
    pub id: String,
    pub owner: String,
    pub source: Option<VolumeSource>,
    pub materialize: bool,
    /// An in-place restore of THIS volume's own `live`, already gated by the parent (services
    /// down) and by `apply_volume` (not already restored).
    pub restore: Option<crd::RestoreWish>,
    pub quota_gb: u64,
    pub home: bool,
}

fn volume_work(engine: &Engine, w: Work) -> Result<Done, String> {
    let Work { id, owner, source, materialize, restore, quota_gb, home } = w;
    let (id, owner) = (id.as_str(), owner.as_str());
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
    rt.block_on(async {
        if materialize {
            // The owner breadcrumb `Engine::push`'s detached `squash` child reads back — written
            // before anything can push, and re-written on every materialize because a rebuilt node
            // has the subvolume without the file.
            crate::record_owner(&engine.pool.root.to_string_lossy(), id, owner);
            if home {
                // The registry's copy when this node has none, else nothing: a home is never
                // created from a `source`, and the API never writes one for it.
                engine.materialize_home(owner, id).await.map_err(|e| e.to_string())?;
            } else {
                match &source {
                    None => engine.create_subvol(id).map_err(|e| e.to_string())?,
                    Some(VolumeSource::CloneOf { volume }) => {
                        engine.clone_local_ids(owner, volume, id).await.map_err(|e| e.to_string())?
                    }
                    // `owner` is the SOURCE's registry label and `region` the region the RECORD
                    // names, both resolved by the API. Neither is the destination's: a member
                    // restoring a team's environment creates it under the team, and the volume it
                    // reads lives under the team's label too — using the destination owner for
                    // both looked up `karthik/env-x` for a snapshot that only exists as
                    // `acme/env-x` and failed NoSuchSnapshot. `None` (any source written before
                    // the fields existed) means the destination's own.
                    Some(VolumeSource::RestoreOf { volume, snapshot_id, owner: src_owner, region }) => {
                        let src_owner = src_owner.as_deref().unwrap_or(owner);
                        engine
                            .restore(src_owner, volume, snapshot_id, id, region.as_deref())
                            .await
                            .map_err(|e| e.to_string())?
                    }
                    // Empty, deliberately: a `GitRepo` volume is seeded by the workspace pod's
                    // INIT CONTAINER, inside the workspace, over SSH, as the owner. The agent no
                    // longer holds a credential that could clone on the user's behalf.
                    Some(VolumeSource::GitRepo { .. }) => engine.create_subvol(id).map_err(|e| e.to_string())?,
                }
            }
        }
        // In place, and staged: `restore` materializes into a throwaway id, so a failed fetch
        // leaves `live` exactly as it was, and `replace_live` keeps the pre-restore bytes as a
        // local RO snapshot before swapping. Nothing here can lose the current state silently.
        if let Some(w) = &restore {
            let staging = format!("{id}-restoring");
            let src_owner = w.owner.as_deref().unwrap_or(owner);
            // Always from nothing: the staging id is deterministic, and `pull_core` treats an
            // existing `live` as "already converged" — so bytes left by a restore that failed
            // half-way would be swapped in and labelled as THIS snapshot.
            engine.discard_staging(&staging).map_err(|e| e.to_string())?;
            engine
                .restore(src_owner, &w.volume, &w.snapshot_id, &staging, w.region.as_deref())
                .await
                .map_err(|e| e.to_string())?;
            engine.replace_live(id, &staging).map_err(|e| e.to_string())?;
        }
        // After every path that can leave a new `live` behind, same rule as the quota below: a
        // home's caches are nested subvolumes, and a received stream does not carry them.
        if home {
            engine.ensure_home_dirs(id, rustic_git_workspaces::k8s::SSH_UID as u32).map_err(|e| e.to_string())?;
        }
        // After EVERY path that can leave a new `live` behind — create, clone, restore — and on a
        // plain quota edit too (a spec change is a new generation, which is a materialize pass
        // that finds `live` already there). Per subvolume, so a restore's fresh `live` would
        // otherwise come up uncapped.
        let quota_unenforced = engine.set_quota(id, quota_gb).map_err(|e| e.to_string())?;
        Ok(Done {
            phase: Phase::Ready,
            lineage_tip: None,
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

/// The one status write for all three kinds: patch unless `same` says nothing the caller cares
/// about moved. The skip is not an optimization — a status write triggers this controller's own
/// watch event, so writing an unchanged status is a hot loop.
///
/// Each caller passes its OWN field list rather than deriving one: `lastTransitionTime` is
/// re-stamped every pass (hence `conditions_eq`), and per-kind fields a builder does not set must
/// not count as a change either. A whole-status compare would make a permanently-failed object
/// write on every reconcile — the very loop this exists to prevent.
async fn write_status<K, S>(
    obj: &K,
    kind: &str,
    cur: Option<&S>,
    st: &S,
    ctx: &Arc<Ctx>,
    same: impl Fn(&S, &S) -> bool,
) -> Result<(), ReconcileErr>
where
    K: Resource<DynamicType = ()> + Clone + serde::de::DeserializeOwned + std::fmt::Debug,
    S: serde::Serialize,
{
    if cur.is_some_and(|c| same(c, st)) {
        return Ok(());
    }
    let api: Api<K> = Api::all(ctx.client.clone());
    patch_status(&api, &obj.name_any(), kind, serde_json::to_value(st).map_err(|e| ReconcileErr(e.to_string()))?).await
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

/// `conditions_eq`, for a status that is only a `serde_json::Value` — which is what `settle`'s
/// per-kind builders hand back. Compares `phase` and the conditions, ignoring `lastTransitionTime`;
/// every other field a builder writes is copied from the object's own previous status.
fn settled_status_eq<K: serde::Serialize>(obj: &K, next: &serde_json::Value) -> bool {
    fn shape(v: &serde_json::Value) -> serde_json::Value {
        let mut conds = v.get("conditions").cloned().unwrap_or(serde_json::Value::Null);
        if let Some(arr) = conds.as_array_mut() {
            for c in arr {
                if let Some(o) = c.as_object_mut() {
                    o.remove("lastTransitionTime");
                }
            }
        }
        serde_json::json!({"phase": v.get("phase"), "conditions": conds})
    }
    serde_json::to_value(obj)
        .ok()
        .and_then(|v| v.get("status").cloned())
        .is_some_and(|cur| shape(&cur) == shape(next))
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
    K: Resource<DynamicType = ()> + ResourceExt + Clone + serde::Serialize + serde::de::DeserializeOwned + std::fmt::Debug,
    F: FnOnce(Condition) -> serde_json::Value,
{
    match outcome {
        Outcome::Permanent(msg, reason) => {
            let cond = crd::condition("Ready", false, reason, &msg, gen);
            let next = write(cond);
            // A permanently-broken object reconciles on every watch event it causes, so writing an
            // unchanged status re-stamps `lastTransitionTime` and wakes itself: a hot loop that only
            // ever ends when someone fixes the spec. Same no-op guard as every other status writer.
            if settled_status_eq(obj, &next) {
                return Ok(Action::await_change());
            }
            tracing::warn!(kind = %kind, name = %obj.name_any(), reason = %reason, error = %msg, "permanent failure; not retrying");
            let api: Api<K> = Api::all(ctx.client.clone());
            patch_status(&api, &obj.name_any(), kind, next).await?;
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

/// Create a `Volume` child if it is missing, and hand back what the API server holds.
///
/// A parent's child takes the PARENT's name: the id is already the registry key, the host path
/// segment and the URL segment, and an ownerReference — not a name — is what makes it a child.
/// That ownerReference is also the whole delete story: `DELETE workspace` reclaims the disk with no
/// ordering logic anywhere in the API. The one child that is NOT named after its parent is an
/// owner's home (`crd::home_volume_name`), whose parent is the binding — hence `id` is a parameter.
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

/// Whether the child's disk actually exists. A parent acts on a child only by reading the child's
/// status, never by guessing — and "the object exists" is not "the subvolume exists". The symptom
/// this guards is a pod wedged forever on `path … does not exist`.
pub(crate) fn volume_is_ready(v: &crd::Volume) -> bool {
    v.status.as_ref().is_some_and(|s| s.phase == crd::Phase::Ready && s.subvolume_present)
}

/// The source references that can be wrong forever, checked ONCE before a Volume is created.
///
/// These never get better by being retried: a `cloneOf` naming a workspace that does not exist, a
/// `restoreOf` whose snapshot id no `done` SnapshotRequest carries. Without this branch each of
/// them requeues at `RETRY` forever, and the log line is indistinguishable from a registry outage.
async fn check_source(source: Option<&VolumeSource>, ctx: &Arc<Ctx>) -> Result<(), Outcome> {
    match source {
        None | Some(VolumeSource::GitRepo { .. }) => Ok(()),
        // Workspace THEN Environment: `clone_env` names an environment's id here, and checking only
        // the workspace kind settled every cloned environment as a permanent `NoSuchSource`.
        Some(VolumeSource::CloneOf { volume }) => {
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
        // Deliberately unchecked here. A snapshot outlives its SnapshotRequest -- the env-stop
        // request is deleted after teardown, and nothing keeps a push request forever -- so
        // validating against a `done` CR made a deleted environment's snapshots unrestorable while
        // their records sat in the registry untouched. The registry is the source of truth, and
        // the restore work reads it anyway; a missing record comes back as `NO_SUCH_RECORD` and is
        // settled permanently on the work path.
        Some(VolumeSource::RestoreOf { .. }) => Ok(()),
    }
}

/// What a parent must do about its `Volume` child, decided once for both parent kinds.
enum Resolved {
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
async fn resolve_volume<P>(
    parent: &P,
    owner: &str,
    team: &str,
    region: &str,
    storage: &Option<crd::WorkspaceStorage>,
    node_name: &str,
    compatible_nodes: &[String],
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
        let (node, nodes) = (node_name.to_string(), compatible_nodes.to_vec());
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
                        "compatibleNodes": nodes,
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
    let vol =
        ensure_child_volume(&parent.name_any(), parent, owner, team, region, s, node_name, &api_kind.to_lowercase(), ctx)
            .await?;
    let id = vol.name_any();
    // The belt to `ensure_child_volume`'s brace: two places allowed to name a node is two places
    // that can disagree about where the data is, and the failure mode is an owner's data split
    // across pools — so a disagreement refuses rather than picks.
    if vol.spec.node_name != node_name {
        let why = format!("status.nodeName {node_name} disagrees with volume {id}'s node {}", vol.spec.node_name);
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

/// The shared epilogue of every way `ensure_profile` can fail: say what went wrong on status, and
/// then let the pod run anyway if a profile is already on disk (the old tools keep working) or
/// stop the pass with `when` if there is nothing to fall back on.
async fn profile_failed(
    w: &crd::Workspace,
    id: &str,
    gen: i64,
    prev: &mut crd::WorkspaceStatus,
    ctx: &Arc<Ctx>,
    (reason, msg): (&str, &str),
    when: Action,
) -> Result<Option<Action>, ReconcileErr> {
    let has = crate::nix::profile_exists(&ctx.profiles_dir, id);
    let st = packages_status(prev, prev.packages.clone(), reason, msg, has, gen);
    write_ws_status_tracking(w, st, prev, ctx).await?;
    Ok(if has { None } else { Some(when) })
}

/// Bring this workspace's Nix profile up to date with `spec.packages`, and say so on status.
/// `None` means the profile is current and the pod may be (re)started; `Some(action)` means
/// status was written and the pass ends here — a build in flight, or a build that failed with
/// no profile to fall back on.
///
/// Runs on EVERY pass, which is what makes packages present after a restore, a clone, a move or
/// an agent restart: each of those arrives with a spec whose hash does not match the profile
/// this node has (or with no profile at all), and the pod is not applied until it does.
///
/// `prev` is advanced as status is written so the pod step below inherits what was said here —
/// a workspace's profile state must not be erased by the pass that goes on to the pod.
async fn ensure_profile(
    w: &crd::Workspace,
    id: &str,
    gen: i64,
    prev: &mut crd::WorkspaceStatus,
    ctx: &Arc<Ctx>,
) -> Result<Option<Action>, ReconcileErr> {
    use rustic_git_workspaces::packages;
    // An empty list still builds: the pod mounts `{profiles_dir}/{id}` as a subPath of the
    // READ-ONLY `nix` hostPath, so a missing directory is an unmountable pod, not a pod without
    // extras. An empty
    // `buildEnv` is a cache hit.
    let uid = w.uid().unwrap_or_default();
    // Its own key: a workspace can be pushing (keyed by the Volume's uid) while its profile builds.
    let key = format!("profile:{uid}");

    // A finished build: publish it and record what it is. A running one: say so and wait. The
    // lock is dropped before any await — a `MutexGuard` held across one makes the whole reconcile
    // future non-`Send`, which `Controller::run` refuses.
    let (finished, still_running) = {
        let mut running = ctx.running.lock().unwrap_or_else(|p| p.into_inner());
        match running.get(&key) {
            Some((_, h)) if h.is_finished() => (running.remove(&key), false),
            Some(_) => (None, true),
            None => (None, false),
        }
    };
    if still_running {
        let st = packages_status(prev, prev.packages.clone(), "Building", "taking the profile through nix", false, gen);
        write_ws_status_tracking(w, st, prev, ctx).await?;
        return Ok(Some(Action::requeue(TICK)));
    }

    // Validated again here: the API validates, but an object can be written by kubectl or a
    // restored backup, and a name that is not an attribute must never reach an expression.
    if let Err(e) = packages::validate_list(&w.spec.packages) {
        // Only a spec edit fixes this, and that is an event.
        return profile_failed(w, id, gen, prev, ctx, ("BuildFailed", &e.to_string()), Action::await_change()).await;
    }
    let pin = crate::nix::nixpkgs_pin();
    // The platform's base set first, then the workspace's own, deduplicated: the hash covers
    // both, so rolling the base rebuilds every profile, and a name in both lists is one package.
    let base = crate::nix::base_packages();
    let mut all: Vec<String> = base.clone();
    all.extend(w.spec.packages.iter().filter(|p| !base.contains(p)).cloned());
    if let Err(e) = packages::validate_list(&all) {
        // A bad BASE entry is the operator's mistake, not the user's; the message says which.
        let msg = format!("base packages: {e}");
        return profile_failed(w, id, gen, prev, ctx, ("BuildFailed", &msg), Action::await_change()).await;
    }
    let hash = packages::hash(&pin, &all);
    let observed = crd::PackagesStatus {
        base,
        observed: w.spec.packages.clone(),
        observed_hash: Some(hash.clone()),
        profile: Some(crate::nix::profile_path(&ctx.profiles_dir, id).to_string_lossy().into_owned()),
        nixpkgs: Some(pin.clone()),
    };

    let started_from = ctx.profile_builds.lock().unwrap_or_else(|p| p.into_inner()).remove(&key);
    let mut had_finished = false;
    if let Some((_, handle)) = finished {
        let outcome = handle.await.unwrap_or_else(|e| Err(format!("build panicked: {e}")));
        // The spec that build started from, not the one we are looking at now. A PATCH that lands
        // mid-build makes them differ, and publishing then would put yesterday's tools behind
        // today's hash — a workspace that never rebuilds. Drop it and build again below.
        let stale = started_from.as_deref() != Some(hash.as_str());
        match outcome {
            Ok(_) if !stale => {
                tokio::task::spawn_blocking({
                    let id = id.to_string();
                    let profiles = ctx.profiles_dir.clone();
                    let hash = hash.clone();
                    move || {
                        crate::nix::publish(&profiles, &id)?;
                        // Offer it to every other workspace with the same inputs — the store path
                        // is whatever `current` now points at. Best effort on purpose: the profile
                        // is published and correct, so a failure here loses only the sharing, and
                        // failing the reconcile over that would be the worse trade.
                        let indexed = std::fs::read_link(crate::nix::profile_path(&profiles, &id))
                            .and_then(|store_path| crate::nix::record_index(&profiles, &hash, &store_path));
                        if let Err(e) = indexed {
                            tracing::warn!(workspace = %id, error = %e, "built profile not indexed; it will not be reused");
                        }
                        Ok::<(), std::io::Error>(())
                    }
                })
                .await
                .map_err(|e| ReconcileErr(format!("publish panicked: {e}")))?
                .map_err(|e| ReconcileErr(format!("publish profile: {e}")))?;
                had_finished = true;
            }
            Ok(_) => {
                let _ = std::fs::remove_file(crate::nix::building_path(&ctx.profiles_dir, id));
                tracing::info!(workspace = %id, "the spec changed during the build; rebuilding");
            }
            Err(_) if stale => {
                tracing::info!(workspace = %id, "a build for a superseded spec failed; rebuilding");
            }
            Err(e) => {
                // The OLD packages, not the ones that failed (`profile_failed` keeps them):
                // recording the new hash here makes the next pass see hash-match plus a directory
                // on disk and never retry the build.
                let backoff = build_failed_backoff(prev);
                return profile_failed(w, id, gen, prev, ctx, ("BuildFailed", &e), Action::requeue(backoff)).await;
            }
        }
    }

    let current = prev.packages.as_ref().and_then(|p| p.observed_hash.as_deref()) == Some(hash.as_str())
        && crate::nix::profile_exists(&ctx.profiles_dir, id);
    if current {
        return Ok(None);
    }
    // Another workspace on this node already built exactly these inputs. The hash covers the pin,
    // the base set and the spec's packages, so an entry under it IS the store path nix would
    // compute — taking it skips an evaluation of nixpkgs (measured at 28 s cold), not a check.
    //
    // `link_profile` writes the same `.building` path a real build does; that is safe only because
    // this workspace's builds are serialised through `ctx.running` under `profile:{uid}` — the
    // still_running arm above returned before we could get here if one were in flight.
    // Not after our own build: that one just published this exact store path, and the arm below
    // records it as `Built` rather than as something reused.
    if let Some(store_path) = (!had_finished).then(|| crate::nix::indexed(&ctx.profiles_dir, &hash)).flatten() {
        let (profiles, wsid) = (ctx.profiles_dir.clone(), id.to_string());
        tokio::task::spawn_blocking(move || crate::nix::link_profile(&profiles, &wsid, &store_path))
            .await
            .map_err(|e| ReconcileErr(format!("link panicked: {e}")))?
            .map_err(|e| ReconcileErr(format!("link profile: {e}")))?;
        let st = packages_status(prev, Some(observed.clone()), "Built", "reused a profile already on this node", true, gen);
        write_ws_status_tracking(w, st, prev, ctx).await?;
        return Ok(None);
    }

    // A fresh profile on disk whose hash status does not yet record (the publish above, or a
    // restart between publish and status): record it without building again.
    if had_finished && crate::nix::profile_exists(&ctx.profiles_dir, id) {
        let st = packages_status(prev, Some(observed), "Built", "profile is on disk", true, gen);
        write_ws_status_tracking(w, st, prev, ctx).await?;
        return Ok(None);
    }

    // A daemon that is not there is not a failed build: it is this node, and it says so under its
    // own reason so the UI does not blame the package list. A workspace that already has a profile
    // still gets its pod — the tools it has keep working while the daemon is down.
    if let Err(e) = ctx.nix.ping().await {
        return profile_failed(w, id, gen, prev, ctx, ("NoNix", &e), Action::requeue(RETRY)).await;
    }

    // Build, on its own thread: `nix` blocks for as long as the substituter takes. The link is
    // made here rather than by `nix -o`: an out-link's auto GC root points at the `.building`
    // path, so the publish rename would orphan it and leave the live profile collectable.
    let expr = packages::expression(&pin, &all);
    let dir = crate::nix::profile_dir(&ctx.profiles_dir, id);
    let building = crate::nix::building_path(&ctx.profiles_dir, id);
    let nix = ctx.nix.clone();
    let timeout = crate::nix::build_timeout();
    // `nix.build` is async (it drives the child through tokio), so this is a plain task; the fs
    // calls after it are a symlink and a mkdir, not the substituter's minutes.
    let handle = tokio::spawn(async move {
        let store_path = nix.build(&expr, timeout).await?;
        // A node that ran the old flat-link layout has `{id}` as a SYMLINK into the store, and
        // `create_dir_all` would happily accept it — every write below then lands inside a
        // read-only store path.
        if dir.is_symlink() {
            std::fs::remove_file(&dir).map_err(|e| format!("old profile link: {e}"))?;
        }
        std::fs::create_dir_all(&dir).map_err(|e| format!("profile dir: {e}"))?;
        let _ = std::fs::remove_file(&building);
        std::os::unix::fs::symlink(&store_path, &building).map_err(|e| format!("profile link: {e}"))?;
        Ok(Done { phase: crd::Phase::Ready, ..Done::default() })
    });
    let handle = wake_on_finish(
        handle,
        ctx.wake_workspace.clone(),
        kube::runtime::reflector::ObjectRef::<crd::Workspace>::new(&w.name_any()),
    );
    ctx.profile_builds.lock().unwrap_or_else(|p| p.into_inner()).insert(key.clone(), hash.clone());
    ctx.running.lock().unwrap_or_else(|p| p.into_inner()).insert(key, (gen, handle));
    // The OLD packages while it builds, never `observed`: an agent that dies between here and the
    // publish would otherwise leave a status whose hash matches the spec next to the PREVIOUS
    // profile on disk, and the next pass skips the build forever. `observed` is recorded on
    // `Built` and nowhere else — status says what is on the disk, not what is being made.
    let st = packages_status(prev, prev.packages.clone(), "Building", "taking the profile through nix", crate::nix::profile_exists(&ctx.profiles_dir, id), gen);
    write_ws_status_tracking(w, st, prev, ctx).await?;
    Ok(Some(Action::requeue(TICK)))
}

/// How long to wait before retrying a failed build: 60s the first time, growing with how long the
/// workspace has been failing, capped at an hour. A misspelled attribute never becomes buildable
/// on its own — retrying it every minute forever is load on the daemon for nothing, and the fix
/// (a spec edit) is an event that wakes the reconcile regardless of the requeue.
fn build_failed_backoff(prev: &crd::WorkspaceStatus) -> Duration {
    let since = prev
        .conditions
        .iter()
        .find(|c| c.type_ == crd::PACKAGES_READY && c.reason == "BuildFailed")
        .map(|c| k8s_openapi::jiff::Timestamp::now().as_second() - c.last_transition_time.0.as_second())
        .unwrap_or(0);
    Duration::from_secs(since.clamp(60, 3600) as u64)
}

/// Status for the packages step: phase stays what it was (a workspace building a profile is not
/// being CREATED), `observed_generation` stays unset (not converged), the `PackagesReady`
/// condition replaces any earlier one of its type.
fn packages_status(
    prev: &crd::WorkspaceStatus,
    packages: Option<crd::PackagesStatus>,
    reason: &str,
    message: &str,
    ready: bool,
    gen: i64,
) -> crd::WorkspaceStatus {
    let mut conditions: Vec<_> = prev.conditions.iter().filter(|c| c.type_ != crd::PACKAGES_READY).cloned().collect();
    let old = prev.conditions.iter().find(|c| c.type_ == crd::PACKAGES_READY);
    // `lastTransitionTime` is a TRANSITION: a build that fails again for the same reason has not
    // transitioned, and re-stamping it would reset the backoff every pass into a flat 60s retry.
    conditions.push(crd::condition_since(old, crd::PACKAGES_READY, ready && reason == "Built", reason, message, gen));
    crd::WorkspaceStatus { observed_generation: None, packages, conditions, ..prev.clone() }
}

/// Make sure this workspace has an SSH host key, and report its public half on status.
///
/// Get-then-create, never apply: the key is this pod's IDENTITY, pinned in every user's
/// `known_hosts`, so a second generation would look exactly like a man-in-the-middle. The Secret is
/// the record — a pass that finds one reads the public line back out of it and generates nothing.
///
/// Runs before the pod for the same reason `ensure_profile` does: a container started without
/// `/etc/ssh` is an sshd that exits on boot.
async fn ensure_ssh(
    w: &crd::Workspace,
    id: &str,
    ns: &str,
    owner_ref: &OwnerReference,
    prev: &mut crd::WorkspaceStatus,
    ctx: &Arc<Ctx>,
) -> Result<(), ReconcileErr> {
    use k8s_openapi::api::core::v1::Secret;
    let secrets: Api<Secret> = Api::namespaced(ctx.client.clone(), ns);
    let name = k8s::ws_ssh_secret_name(id);
    let public = match secrets.get_opt(&name).await? {
        Some(s) => s
            .data
            .as_ref()
            .and_then(|d| d.get("ssh_host_ed25519_key.pub"))
            .map(|b| String::from_utf8_lossy(&b.0).trim().to_string())
            // A Secret without the public half is one someone edited: the private key is still the
            // pod's identity, so it is never replaced — status just has nothing to report.
            .unwrap_or_default(),
        None => {
            let (private, public) = crate::sshkeys::generate().map_err(ReconcileErr)?;
            let s = k8s::ws_ssh_secret(id, &w.spec.name, ns, &w.spec.owner, owner_ref, &private, &public);
            match secrets.create(&PostParams::default(), &s).await {
                Ok(_) => public,
                // Lost the race with our own earlier pass: the winner's key is the identity, and
                // the one just generated is discarded unread.
                Err(kube::Error::Api(st)) if st.code == 409 => secrets
                    .get(&name)
                    .await?
                    .data
                    .and_then(|d| d.get("ssh_host_ed25519_key.pub").map(|b| String::from_utf8_lossy(&b.0).trim().to_string()))
                    .unwrap_or_default(),
                Err(e) => return Err(e.into()),
            }
        }
    };
    // ponytail: the Secret is created once and never reconciled, so an existing workspace keeps the
    // `sshd_config` it was made with. Delete the Secret (never the key) and let the next pass
    // rewrite it, or patch just that field here, if the config ever has to change under running
    // workspaces.
    //
    // An empty public half is a hand-edited Secret, not a key: report nothing rather than an empty
    // string the CLI would try to pin — and say so, because the symptom is a workspace nobody can
    // ssh into with no other trace.
    if public.is_empty() {
        tracing::warn!(workspace = %id, secret = %name, "host key Secret has no public half; status.sshHostKey left as it was");
        return Ok(());
    }
    if prev.ssh_host_key.as_deref() != Some(public.as_str()) {
        // `observedGeneration` stays unset: this pass has not converged yet — the pod is still
        // ahead of it.
        let st = crd::WorkspaceStatus { ssh_host_key: Some(public), observed_generation: None, ..prev.clone() };
        write_ws_status_tracking(w, st, prev, ctx).await?;
    }
    Ok(())
}

/// `write_ws_status`, remembering what was written: later steps of the same pass build their status
/// from `prev`, so a write that is not tracked is a condition silently dropped by the next one.
async fn write_ws_status_tracking(
    w: &crd::Workspace,
    st: crd::WorkspaceStatus,
    prev: &mut crd::WorkspaceStatus,
    ctx: &Arc<Ctx>,
) -> Result<(), ReconcileErr> {
    *prev = st.clone();
    write_ws_status(w, st, ctx).await
}

/// Render this workspace's `/etc/resolv.conf` into the agent-owned attach directory.
///
/// IN PLACE, never via a rename. The pod bind-mounts this file by inode, so replacing it with
/// `rename(2)` — the usual way to write a file atomically — leaves every running pod reading the
/// OLD inode and attachment silently stops working. Verified on a live cluster; do not "fix" this
/// into an atomic write. `std::fs::write` truncates the existing inode, which is what is wanted.
///
/// Truncate-then-write is not atomic, so a lookup landing inside that window reads a short file and
/// fails once. Accepted: the resolver retries, the next write is complete, and the only atomic
/// alternative — rename — is the thing forbidden above.
///
/// Before the pod, never after: the mount is `type: File`, so a missing target is not created —
/// it is a mount failure, and the pod sits in `ContainerCreating` until this file exists.
pub(crate) fn write_resolv_conf(pool: &str, ws_id: &str, ws_ns: &str, env_ns: Option<&str>) -> Result<(), ReconcileErr> {
    let dir = k8s::attach_dir(pool, ws_id);
    std::fs::create_dir_all(&dir).map_err(|e| ReconcileErr(format!("attach dir {dir}: {e}")))?;
    let path = k8s::attach_file(pool, ws_id);
    // A pre-migration pod mounted this path with a `subPath`, which kubernetes created as a
    // directory when it did not exist yet. Nothing writes a directory here any more, but a node
    // upgraded from that shape can still have one on disk; clear it rather than leaving the
    // workspace with no DNS for as long as the pod lives.
    if std::fs::metadata(&path).map(|m| m.is_dir()).unwrap_or(false) {
        std::fs::remove_dir_all(&path).map_err(|e| ReconcileErr(format!("attach file {path}: {e}")))?;
    }
    let template = std::fs::read_to_string("/etc/resolv.conf")
        .map_err(|e| ReconcileErr(format!("reading the agent's resolv.conf: {e}")))?;
    std::fs::write(&path, k8s::resolv_conf(&template, ws_ns, env_ns))
        .map_err(|e| ReconcileErr(format!("writing {path}: {e}")))
}

/// The pod step's conditions, keeping whatever the packages step said about this profile — and the
/// `Attached` condition, which is not decoration: it is the ONLY record of which environment's
/// namespace holds this workspace's ingress half, and dropping it on a stop (or any pass that
/// rebuilds the list) strands that grant on the detach after it. The pod path recomputes `Attached`
/// and replaces this copy.
fn ws_conditions(prev: &crd::WorkspaceStatus, ready: Condition) -> Vec<Condition> {
    kept_conditions(&prev.conditions, ready)
}

/// The same, for the writes that have the previous condition list but not the whole status —
/// `resolve_volume` is shared with environments and takes it as a slice, and `settle`'s builders
/// only have what they captured.
///
/// EVERY workspace status write goes through one of these two. Three separate sites that built the
/// list literally each dropped `Attached` and stranded the same grant, which is why the invariant is
/// "no literal condition list on a workspace path" rather than three more fixes.
fn kept_conditions(prev: &[Condition], ready: Condition) -> Vec<Condition> {
    let mut c: Vec<Condition> =
        prev.iter().filter(|c| c.type_ == crd::PACKAGES_READY || c.type_ == crd::ATTACHED).cloned().collect();
    c.push(ready);
    c
}

/// `ws_conditions` with this pass's freshly resolved `Attached` — replacing the preserved copy,
/// which is the previous pass's answer, and dropping it entirely when nothing is attached.
fn with_attached(conds: Vec<Condition>, attached: Option<Condition>) -> Vec<Condition> {
    conds.into_iter().filter(|c| c.type_ != crd::ATTACHED).chain(attached).collect()
}

/// One in-progress status write for the stop path: keep everything `prev` says, drop
/// `observedGeneration` (the stop has NOT converged yet) and report `cond`.
async fn wait_status(
    w: &crd::Workspace,
    prev: crd::WorkspaceStatus,
    cond: Condition,
    ctx: &Arc<Ctx>,
) -> Result<(), ReconcileErr> {
    let st = crd::WorkspaceStatus { observed_generation: None, conditions: ws_conditions(&prev, cond), ..prev };
    write_ws_status(w, st, ctx).await
}

/// Stop the workspace: push the owner's home, then delete the pod — and only in that order.
async fn stop_workspace(
    w: &crd::Workspace,
    prev: crd::WorkspaceStatus,
    gen: i64,
    ctx: &Arc<Ctx>,
) -> Result<Action, ReconcileErr> {
    // Already stopped: nothing to do. Load-bearing now that `stop-home-{ws}` is DELETED after
    // the teardown — without this the request's absence reads as "no push yet" on every later
    // event, and a stopped workspace would push its home forever.
    if prev.phase == crd::Phase::Stopped {
        if prev.observed_generation != Some(gen) {
            let st = crd::WorkspaceStatus { observed_generation: Some(gen), ..prev };
            write_ws_status(w, st, ctx).await?;
        }
        return Ok(Action::await_change());
    }
    let ns = crd::ws_namespace(&w.spec.owner, &w.spec.team);
    // The Volume child takes the parent's own name, so the pod's name is known without reading
    // (or creating) it.
    let id = prev.volume_ref.clone().unwrap_or_else(|| w.name_any());
    // The home is pushed BEFORE the pod goes, and the pod goes only once the push has LANDED —
    // the same fail-closed gate an environment's own subvolume gets, and for the same reason: a
    // stop that tore the pod down on a failed push is the one moment the person's dotfiles are
    // lost for good. Only if a pod ever ran: a workspace stopped while still Creating wrote
    // nothing into the home. The pod is NOT drained first: a running shell's home is exactly
    // what the five-minute beat already snapshots — this push is the last of those, not a
    // quiesced one.
    let home = crd::home_volume_name(&w.spec.owner);
    let home_here = match ctx.volumes.get(&kube::runtime::reflector::ObjectRef::new(&home)) {
        _ if prev.pod_ref.is_none() => false,
        // "Is there a subvolume with the person's files in it" — the phase is not the question:
        // a home mid-reconcile (a quota edit) still has everything to lose.
        Some(v) => v.status.as_ref().is_some_and(|s| s.subvolume_present),
        // Not in the store is not "no home". An agent restarting with this stop pending
        // reconciles the Workspace off its initial list, possibly before the Volume watch has
        // delivered the home — and taking the no-home branch here would delete the pod without
        // a push, silently. The binding is the fact that decides: it authors the home before it
        // reports ready, so while it exists the home is on its way and the stop waits. Only an
        // owner with no binding at all (bound before homes existed, then unbound) has none.
        None => {
            if binding::get_binding(ctx, &w.spec.region, &w.spec.owner).await?.is_some() {
                let cond = crd::condition("Progressing", true, "PushBeforeStop", "waiting for the home volume", gen);
                wait_status(w, prev, cond, ctx).await?;
                return Ok(Action::requeue(TICK));
            }
            false
        }
    };
    let request = format!("stop-home-{id}");
    if home_here {
        match stop_push(&request, &w.spec.owner, &home, w, ctx).await? {
            StopPush::Landed => {}
            StopPush::Failed => {
                let cond = crd::condition(
                    "Ready",
                    false,
                    "StopSnapshotFailed",
                    "the home push failed; the pod is kept rather than lose the home's last state",
                    gen,
                );
                wait_status(w, prev, cond, ctx).await?;
                return Ok(Action::await_change());
            }
            StopPush::Waiting => {
                let cond = crd::condition("Progressing", true, "PushBeforeStop", "waiting for the home's push", gen);
                wait_status(w, prev, cond, ctx).await?;
                return Ok(Action::requeue(TICK));
            }
        }
    }
    delete_ignoring_404(&Api::<Pod>::namespaced(ctx.client.clone(), &ns), &id).await?;
    if home_here {
        // Served its purpose; left behind, the NEXT stop would find `done` under the same name
        // and stop without pushing at all.
        delete_ignoring_404(&Api::<crd::SnapshotRequest>::all(ctx.client.clone()), &request).await?;
    }
    // `ws_conditions`, not a bare vec: a stop that dropped `PackagesReady` left the web
    // showing "installing packages…" for a workspace that is simply off.
    let conditions = ws_conditions(&prev, crd::condition("Ready", true, "Converged", "workspace matches spec", gen));
    let st = crd::WorkspaceStatus {
        phase: crd::Phase::Stopped,
        observed_generation: Some(gen),
        volume_ref: Some(id),
        pod_ref: None,
        conditions,
        ..prev
    };
    write_ws_status(w, st, ctx).await?;
    Ok(Action::await_change())
}

pub async fn apply_workspace(w: &crd::Workspace, ctx: &Arc<Ctx>) -> Result<Action, ReconcileErr> {
    heal_labels(&Api::<crd::Workspace>::all(ctx.client.clone()), w, &w.spec.owner, &w.spec.team, "workspace").await?;
    let gen = w.meta().generation.unwrap_or(0);
    let mut prev = w.status.clone().unwrap_or_default();
    // Stopping is a home push and a pod delete — it needs neither the disk nor the namespace. Run
    // it BEFORE those gates: a workspace whose Volume failed permanently would otherwise be
    // unstoppable, stuck reporting `creating` with a pod still running on a broken subvolume.
    if w.spec.desired_state == DesiredState::Stopped {
        return stop_workspace(w, prev, gen, ctx).await;
    }
    let vol = match resolve_volume(
        w,
        &w.spec.owner,
        &w.spec.team,
        &w.spec.region,
        &w.spec.storage,
        &prev.node_name.clone(),
        &prev.compatible_nodes,
        &prev.conditions.clone(),
        gen,
        ctx,
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
    // The namespace is the OwnerBinding reconciler's to make; this one only waits for it. Creating
    // it here as well is how it ended up with two writers.
    //
    // ponytail: a binding becoming ready wakes a waiting workspace only via its 15s requeue —
    // mapping one binding to every waiting Workspace of that owner is a list per binding event, and
    // the wait is bounded by one tick. Wire a `spec.owner`-indexed reflector if first-workspace
    // latency ever shows up as a complaint.
    if !binding::namespace_ready(ctx, &w.spec.region, &w.spec.owner).await? {
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
    // The home the pod mounts must be READY, not merely present: kubelet is happy the instant the
    // hostPath directory exists, and `materialize_home` snapshots `live` into place before the nested cache
    // subvolumes and the qgroup exist — a pod started in that window makes `.npm` a plain
    // directory that keep-biased `ensure_home_dirs` never converts, pushed forever. The binding
    // reported ready only after authoring the home, so "not in the store" is watch latency, and
    // a home whose own reconcile settled in Error (the region unreachable) is permanent here too:
    // no pod can run on a home that will not exist.
    let home = crd::home_volume_name(&w.spec.owner);
    match ctx.volumes.get(&kube::runtime::reflector::ObjectRef::new(&home)) {
        Some(v) if volume_is_ready(&v) => {}
        Some(v) if v.status.as_ref().is_some_and(|s| s.phase == crd::Phase::Error) => {
            let prev = prev.clone();
            return settle(
                Outcome::Permanent(format!("home volume {home} failed; see its status"), "HomeNotReady"),
                w,
                "Workspace",
                gen,
                move |cond| {
                    serde_json::json!({
                        "phase": crd::Phase::Error,
                        "nodeName": prev.node_name,
                        "compatibleNodes": prev.compatible_nodes,
                        "volumeRef": prev.volume_ref,
                        "conditions": ws_conditions(&prev, cond),
                    })
                },
                ctx,
            )
            .await;
        }
        _ => {
            let st = crd::WorkspaceStatus {
                phase: crd::Phase::Creating,
                observed_generation: None,
                volume_ref: Some(id),
                conditions: ws_conditions(&prev, crd::condition("Ready", false, "HomeNotReady", "waiting for the home volume", gen)),
                ..prev
            };
            write_ws_status(w, st, ctx).await?;
            return Ok(Action::requeue(TICK));
        }
    }

    // Commit-model worktree materialization: a workspace just claimed onto this node (or one
    // whose pod was never started here) has no `live/{id}` subvolume yet. `head` is `None` on a
    // brand-new workspace (bootstrap: an empty worktree) — Task 4 never WRITES `status.head`
    // itself, only preserves whatever is already there via `..prev`; the first writers are Task 5
    // (a commit records the new head) and Task 6 (a clone/restore grafts one on). Until one of
    // those lands, `head == None` is ambiguous between "genuinely bootstrap" and "this workspace's
    // own head just has not been recorded yet" — the guard below tells the two apart the same way
    // the claim itself does, by asking whether the VOLUME has any commits at all.
    if ctx.commit_model {
        if prev.head.is_none() && crate::claim::has_commits(ctx, &id).await? {
            // F2 guard: the volume has commits but this workspace's own `head` is still `None` —
            // checking out `None` here would hand it an EMPTY worktree next to real history,
            // which is the never-started-dataless bug in worktree form. Wait for Task 5/6 to
            // write a real head instead of guessing one; zero-commit volumes never reach this arm
            // (`has_commits` is false), so bootstrap is untouched.
            let st = crd::WorkspaceStatus {
                phase: crd::Phase::Creating,
                observed_generation: None,
                volume_ref: Some(id.clone()),
                conditions: ws_conditions(
                    &prev,
                    crd::condition("Ready", false, "HeadUnknown", "volume has commits but this workspace has no recorded head yet", gen),
                ),
                ..prev
            };
            write_ws_status(w, st, ctx).await?;
            return Ok(Action::requeue(TICK));
        }
        // `WORKTREE_EXISTS` converges a race (this pass and an earlier one both reaching here, or
        // a pod restart finding its own worktree already there) into a no-op rather than an error.
        let (engine, vol_id, ws_id, head) = (ctx.engine.clone(), id.clone(), id.clone(), prev.head.clone());
        let result = tokio::task::spawn_blocking(move || engine.checkout(&vol_id, head.as_deref(), &ws_id))
            .await
            .map_err(|e| ReconcileErr(e.to_string()))?;
        match result {
            Ok(()) => {}
            Err(e) if e.0 == rustic_git_workspaces::engine::commit::WORKTREE_EXISTS => {}
            Err(e) => return Err(ReconcileErr(e.0)),
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
    write_resolv_conf(&ctx.pool, &id, &ns, env_ns.as_deref())?;
    let policies: Api<NetworkPolicy> = Api::namespaced(ctx.client.clone(), &ns);
    match &env {
        Some((env_ns, e)) => {
            ensure(&policies, &k8s::attach_egress(&ns, &id, env_ns, &w.spec.owner, &pod_ctx.owner_ref), ctx).await?;
            // The environment-side half cannot be owned by this Workspace: an ownerReference may
            // not cross namespaces. It is owned by the ENVIRONMENT instead, so deleting the
            // environment collects it, and a detach deletes it by name.
            let env_ref = owner_ref_of_kind(e)?;
            let in_env: Api<NetworkPolicy> = Api::namespaced(ctx.client.clone(), env_ns);
            ensure(&in_env, &k8s::attach_ingress(env_ns, &ns, &id, &w.spec.owner, &env_ref), ctx).await?;
        }
        // Detach is this same pass with the field cleared, so the workspace-side half goes by name.
        None => delete_ignoring_404(&policies, &k8s::attach_policy_name(&id)).await?,
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
        delete_ignoring_404(&old, &k8s::attach_policy_name(&id)).await?;
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
        if let Some(action) = ensure_profile(w, &id, gen, &mut prev, ctx).await? {
            return Ok(action);
        }
        // Same rule as the profile: the pod mounts this, so it exists first or sshd dies on boot.
        ensure_ssh(w, &id, &ns, &owner_ref, &mut prev, ctx).await?;
    }

    let pods: Api<Pod> = Api::namespaced(ctx.client.clone(), &ns);
    // Reality, not intent: a pod created before this feature shipped has no `attach` volume and no
    // `/etc/resolv.conf` mount, and `create_if_absent` never replaces it — so the file this pass
    // wrote and both policies it applied resolve nothing at all. Reporting `Attached=True` there is
    // a lie the user cannot see through, so the live pod decides. An absent pod is not a refusal:
    // the one created below carries the mount.
    if env_ns.is_some() && !pod_carries_the_attach_mount(&pods, &id).await? {
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
                                        "compatibleNodes": prev.compatible_nodes,
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
            create_if_absent(&pods, &k8s::workspace_pod(&w.spec, &id, &pod_ctx, init)).await?;
            // Applying a pod is not a pod running. Read it back: a pod can sit Pending on an
            // unschedulable node or CrashLoopBackOff on a bad image, and reporting Ready straight
            // from the apply made a broken workspace indistinguishable from a working one.
            if !pod_is_ready(&pods, &id).await? {
                let st = crd::WorkspaceStatus {
                    phase: crd::Phase::Creating,
                    observed_generation: None,
                    volume_ref: Some(id.clone()),
                    pod_ref: Some(format!("{ns}/{id}")),
                    conditions: with_attached(
                        ws_conditions(&prev, crd::condition("Ready", false, "PodNotReady", "pod is not ready yet", gen)),
                        attached.clone(),
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
            (crd::Phase::Ready, Some(format!("{ns}/{id}")))
        }
        // Handled at the top of this function, before the Volume and namespace gates — stopping IS
        // deleting the pod, and it must not depend on either being healthy.
        DesiredState::Stopped => unreachable!("stopped is handled before the gates"),
    };
    let st = crd::WorkspaceStatus {
        phase,
        observed_generation: Some(gen),
        volume_ref: Some(id),
        pod_ref,
        conditions: with_attached(
            ws_conditions(&prev, crd::condition("Ready", true, "Converged", "workspace matches spec", gen)),
            attached,
        ),
        ..prev
    };
    write_ws_status(w, st, ctx).await?;
    Ok(Action::await_change())
}

/// Whether the RUNNING pod can actually see an attachment: it mounts `attach_file` as a hostPath
/// named `"attach"`. A
/// pod that does not exist yet answers `true` — the one this pass is about to create has it, and a
/// pass that reported `PodPredatesAttachment` for an absent pod would flap the condition on every
/// restart.
async fn pod_carries_the_attach_mount(pods: &Api<Pod>, name: &str) -> Result<bool, ReconcileErr> {
    let Some(pod) = pods.get_opt(name).await? else {
        return Ok(true);
    };
    Ok(pod.spec.and_then(|s| s.volumes).is_some_and(|vs| vs.iter().any(|v| v.name == "attach" && v.host_path.is_some())))
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

pub(crate) async fn write_ws_status(w: &crd::Workspace, st: crd::WorkspaceStatus, ctx: &Arc<Ctx>) -> Result<(), ReconcileErr> {
    write_status(w, "Workspace", w.status.as_ref(), &st, ctx, |a, b| {
        a.phase == b.phase
            && a.observed_generation == b.observed_generation
            && a.pod_ref == b.pod_ref
            && a.node_name == b.node_name
            && a.compatible_nodes == b.compatible_nodes
            && a.volume_ref == b.volume_ref
            // `head`/`durable` in the comparison: without them, a commit's advance of `head` with
            // every other field unchanged reads as a no-op and the write silently never happens —
            // exactly the bug `snapshot::advance_head`'s own test caught.
            && a.head == b.head
            && a.durable == b.durable
            && conditions_eq(&a.conditions, &b.conditions)
    })
    .await
}

// ── environments ─────────────────────────────────────────────────────────

pub async fn apply_environment(e: &crd::Environment, ctx: &Arc<Ctx>) -> Result<Action, ReconcileErr> {
    heal_labels(&Api::<crd::Environment>::all(ctx.client.clone()), e, &e.spec.owner, "", "environment").await?;
    let gen = e.meta().generation.unwrap_or(0);
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
        &prev.compatible_nodes,
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

    let ns = crd::env_namespace(&id);
    let deployments: Api<StatefulSet> = Api::namespaced(ctx.client.clone(), &ns);

    // Before anything else, including the stop path: an environment that is being restored has no
    // business converging its services against a disk that is about to be swapped underneath them.
    if let Some(action) = restore_gate(e, &vol, &ns, &deployments, gen, ctx).await? {
        return Ok(action);
    }

    if e.spec.desired_state == DesiredState::Stopped {
        return stop_environment(e, &vol, &ns, &deployments, prev, gen, ctx).await;
    }
    run_environment(e, &vol, &ns, &deployments, &owner_ref, prev, gen, ctx).await
}

/// Tear the environment down, fail-closed: the services drain, the environment's own subvolume is
/// pushed, and only a push that has LANDED lets the StatefulSets go.
async fn stop_environment(
    e: &crd::Environment,
    vol: &crd::Volume,
    ns: &str,
    deployments: &Api<StatefulSet>,
    prev: crd::EnvironmentStatus,
    gen: i64,
    ctx: &Arc<Ctx>,
) -> Result<Action, ReconcileErr> {
    let id = vol.name_any();

    // Already stopped at this generation: nothing to do. This guard is load-bearing now that
    // the `stop-{env}` request is DELETED after teardown — without it the absence of that
    // object reads as "no push requested yet", so every later event on a stopped environment
    // would create a fresh request and push a snapshot nobody asked for, forever.
    if e.status.as_ref().is_some_and(|s| s.phase == crd::Phase::Stopped && s.observed_generation == Some(gen)) {
        return Ok(Action::await_change());
    }
    // Stopped at an OLDER generation: the services were torn down after a push that landed,
    // and nothing has run since, so there is nothing new on disk to push. A restore is the
    // common way here (`restore_gate` above bumps the generation), and pushing the freshly
    // restored subvolume as a new commit is a snapshot nobody asked for. Observe and stop.
    if prev.phase == crd::Phase::Stopped {
        let st = crd::EnvironmentStatus { observed_generation: Some(gen), volume_ref: Some(id), ..prev };
        write_env_status(e, st, ctx).await?;
        return Ok(Action::await_change());
    }
    // Scaled to zero and DRAINED before the push, not after: the pushed record is what a
    // restore on another node reads back as this environment's last state, and a snapshot
    // taken under a running database is crash-consistent at best. Same shape as the restore
    // gate, same reason. The StatefulSets themselves are not deleted here — that still waits
    // for the push to land, below.
    if drain_services(e, ns, deployments, ctx).await? > 0 {
        let st = crd::EnvironmentStatus {
            phase: crd::Phase::Running,
            observed_generation: None,
            conditions: vec![crd::condition("Progressing", true, "Draining", "waiting for the services to stop", gen)],
            ..prev
        };
        write_env_status(e, st, ctx).await?;
        return Ok(Action::requeue(TICK));
    }
    // An environment that stops must push first. One push of the env's own subvolume covers
    // every mounted volume atomically; an env torn down without it loses its last state for
    // good, which is why the deletes below are gated on the push having landed, not merely
    // requested.
    match stop_push(&format!("stop-{}", e.name_any()), &e.spec.owner, &vol.name_any(), e, ctx).await? {
        StopPush::Landed => {}
        StopPush::Failed => {
            let st = crd::EnvironmentStatus {
                phase: crd::Phase::Running,
                observed_generation: None,
                service_status: vec![],
                conditions: vec![crd::condition(
                    "Ready",
                    false,
                    "StopSnapshotFailed",
                    "the stop snapshot failed; the services are kept (scaled to zero) rather than lose their state",
                    gen,
                )],
                ..e.status.clone().unwrap_or_default()
            };
            write_env_status(e, st, ctx).await?;
            return Ok(Action::await_change());
        }
        StopPush::Waiting => {
            let st = crd::EnvironmentStatus {
                // Still `running`: the StatefulSets exist (at zero) until the push lands, and
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
            return Ok(Action::requeue(TICK));
        }
    }
    for svc in &e.spec.services {
        forget_applied(ctx, "StatefulSet", ns, &svc.name);
        delete_ignoring_404(deployments, &svc.name).await?;
    }
    // The stop request has served its purpose. Left behind, the NEXT stop of this environment
    // would find a `done` object under the same fixed name and tear down without pushing at
    // all — the exact data loss the wait above exists to prevent.
    delete_ignoring_404(&Api::<crd::SnapshotRequest>::all(ctx.client.clone()), &format!("stop-{}", e.name_any()))
        .await?;
    let st = crd::EnvironmentStatus {
        phase: crd::Phase::Stopped,
        observed_generation: Some(gen),
        volume_ref: Some(id),
        service_status: vec![],
        conditions: vec![crd::condition("Ready", true, "Stopped", "pushed and stopped", gen)],
        ..prev
    };
    write_env_status(e, st, ctx).await?;
    Ok(Action::await_change())
}

/// Converge the environment's namespace, storage and services against spec, and report what the
/// StatefulSets actually say about themselves.
#[allow(clippy::too_many_arguments)]
async fn run_environment(
    e: &crd::Environment,
    vol: &crd::Volume,
    ns: &str,
    deployments: &Api<StatefulSet>,
    owner_ref: &OwnerReference,
    prev: crd::EnvironmentStatus,
    gen: i64,
    ctx: &Arc<Ctx>,
) -> Result<Action, ReconcileErr> {
    let id = vol.name_any();

    // Same worktree materialization a workspace does before any pod is built, and the same
    // HeadUnknown guard: an environment claimed onto this node for a volume with commits but no
    // recorded head yet must wait for Task 5/6 to write one rather than checking out empty next
    // to real history. Task 4 left this arm to this task — see `apply_workspace`'s twin.
    if ctx.commit_model {
        if prev.head.is_none() && crate::claim::has_commits(ctx, &id).await? {
            let st = crd::EnvironmentStatus {
                phase: crd::Phase::Creating,
                observed_generation: None,
                conditions: vec![crd::condition(
                    "Ready", false, "HeadUnknown", "volume has commits but this environment has no recorded head yet", gen,
                )],
                ..prev.clone()
            };
            write_env_status(e, st, ctx).await?;
            return Ok(Action::requeue(TICK));
        }
        let (engine, vol_id, ws_id, head) = (ctx.engine.clone(), id.clone(), id.clone(), prev.head.clone());
        let result = tokio::task::spawn_blocking(move || engine.checkout(&vol_id, head.as_deref(), &ws_id))
            .await
            .map_err(|e| ReconcileErr(e.to_string()))?;
        match result {
            Ok(()) => {}
            Err(e) if e.0 == rustic_git_workspaces::engine::commit::WORKTREE_EXISTS => {}
            Err(e) => return Err(ReconcileErr(e.0)),
        }
    }

    ensure(
        &Api::<Namespace>::all(ctx.client.clone()),
        &k8s::namespace(ns, &e.spec.owner, "environment", Some(owner_ref)),
        ctx,
    )
    .await?;
    let policies = Api::<NetworkPolicy>::namespaced(ctx.client.clone(), ns);
    for p in k8s::default_policies(ns, &e.spec.owner, owner_ref) {
        ensure(&policies, &p, ctx).await?;
    }
    // An environment's services are the likeliest place a private image appears, so this namespace
    // needs the same scoped grant a workspace namespace gets — the API writes the pull credential
    // here, and nowhere it has not been vouched for.
    ensure(
        &Api::<RoleBinding>::namespaced(ctx.client.clone(), ns),
        &k8s::api_secret_binding(ns, &e.spec.owner, API_SERVICE_ACCOUNT, API_NAMESPACE, None),
        ctx,
    )
    .await?;
    // The env unit's ceiling, matching `service_deployment`'s resources: 4 GB limit, packed at the
    // model's 1.5x oversubscription. Owned by the Environment — this namespace holds exactly one.
    ensure(
        &Api::<LimitRange>::namespaced(ctx.client.clone(), ns),
        &k8s::limit_range(ns, &e.spec.owner, "environment", &k8s::env_unit_resources(), Some(owner_ref)),
        ctx,
    )
    .await?;
    let pod_ctx = k8s::PodContext {
        pool: &ctx.pool,
        node_name: &vol.spec.node_name,
        owner_ref: owner_ref.clone(),
        runtime_class: ctx.runtime_class.as_deref(),
        default_image: &ctx.default_image,
    };
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

    let services: Api<Service> = Api::namespaced(ctx.client.clone(), ns);
    for svc in &e.spec.services {
        let set = k8s::service_statefulset(svc, &id, &e.spec.owner, &pod_ctx).map_err(ReconcileErr)?;
        ensure(deployments, &set, ctx).await?;
        ensure(&services, &k8s::service_clusterip(svc, &id, &e.spec.owner, owner_ref), ctx).await?;
    }
    // Read each StatefulSet back rather than reporting `ready: true` from having applied it. A
    // service whose image will not pull, or whose pod cannot schedule, was previously reported
    // ready the instant its object existed — so `kubectl wait --for=condition=Ready
    // environment` returned before anything was listening, and the only thing that noticed was a
    // connectivity check failing two steps later.
    let mut service_status = Vec::with_capacity(e.spec.services.len());
    for svc in &e.spec.services {
        service_status.push(deployment_status(deployments, &svc.name).await?);
    }
    let all_ready = service_status.iter().all(|s| s.ready);
    let st = crd::EnvironmentStatus {
        phase: crd::Phase::Running,
        // Not converged until every service is: leaving it unobserved is what makes the next pass
        // look again instead of declaring a half-up environment finished.
        observed_generation: all_ready.then_some(gen),
        service_status,
        conditions: {
            let mut c = vec![if all_ready {
                crd::condition("Ready", true, "Converged", "environment matches spec", gen)
            } else {
                crd::condition("Ready", false, "ServicesNotReady", "one or more services are not ready", gen)
            }];
            // Reaching here with a restore wish means the Volume already reports it materialized —
            // the gate above is what stops anything else getting this far — so the services being
            // ensured on this pass IS the scale back up, and this says the restore is over.
            if e.spec.restore.is_some() {
                c.push(crd::condition("Restoring", false, "Restored", "the snapshot is live", gen));
            }
            c
        },
        volume_ref: Some(id.clone()),
        ..prev
    };
    write_env_status(e, st, ctx).await?;
    Ok(if all_ready { Action::await_change() } else { Action::requeue(TICK) })
}

/// One service's observed readiness, from the StatefulSet's own status.
///
/// `readyReplicas >= 1`, not `replicas`: `replicas` is what was asked for, `readyReplicas` is what
/// is actually serving. A missing StatefulSet reports not-ready rather than erroring — it is the
/// ordinary gap between applying it and the API server materializing it.
async fn deployment_status(deployments: &Api<StatefulSet>, name: &str) -> Result<crd::ServiceStatus, ReconcileErr> {
    let Some(d) = deployments.get_opt(name).await? else {
        return Ok(crd::ServiceStatus { name: name.into(), ready: false, message: Some("statefulset not created yet".into()) });
    };
    let ready = d.status.as_ref().and_then(|s| s.ready_replicas).unwrap_or(0);
    Ok(crd::ServiceStatus {
        name: name.into(),
        ready: ready >= 1,
        message: (ready < 1).then(|| "no ready replicas".to_string()),
    })
}

/// Pods in `ns` that can still be WRITING. A Succeeded or Failed pod holds no file handles and is
/// never collected on its own, so counting every pod in the namespace waits for something that
/// will not happen — a restore would hang forever behind a job that finished days ago. A pod that
/// is already terminating still counts: it has not exited yet.
async fn writing_pods(ns: &str, ctx: &Arc<Ctx>) -> Result<usize, ReconcileErr> {
    let pods: Api<Pod> = Api::namespaced(ctx.client.clone(), ns);
    Ok(pods
        .list(&kube::api::ListParams::default())
        .await?
        .items
        .into_iter()
        .filter(|p| {
            let phase = p.status.as_ref().and_then(|s| s.phase.as_deref()).unwrap_or("Pending");
            matches!(phase, "Running" | "Pending")
        })
        .count())
}

/// `Some(action)` while an in-place restore is in flight; `None` when there is nothing to restore
/// or the Volume already reports the wished-for snapshot live.
///
/// The order is the whole point. A restore rewrites the bytes a running service has open, so every
/// StatefulSet is scaled to ZERO and its pods are gone from the API server before the wish is copied
/// down to the child Volume — "no replicas" is not "no processes", and a database still flushing
/// into a subvolume that is being swapped is corruption nobody can attribute later.
///
/// `spec.restore` is never cleared here: a controller does not edit the user's spec, and "done" is
/// expressible without it (`Volume.status.restoredTo == wish.snapshotId`). A second restore of the
/// same snapshot is a new `requestedAt`, which is a new generation, which the Volume's own guard
/// then sees as a new wish.
async fn restore_gate(
    e: &crd::Environment,
    vol: &crd::Volume,
    ns: &str,
    deployments: &Api<StatefulSet>,
    gen: i64,
    ctx: &Arc<Ctx>,
) -> Result<Option<Action>, ReconcileErr> {
    let Some(wish) = &e.spec.restore else { return Ok(None) };
    let st = vol.status.as_ref();
    if crd::wish_granted(
        wish,
        st.and_then(|s| s.restored_to.as_deref()),
        st.and_then(|s| s.restore_requested_at.as_deref()),
    ) {
        return Ok(None);
    }

    let remaining = drain_services(e, ns, deployments, ctx).await?;
    let (reason, message) = match remaining {
        0 => ("Restoring", "materializing the snapshot"),
        _ => ("Draining", "waiting for the services to stop"),
    };
    if remaining == 0 && vol.spec.restore_to.as_ref() != Some(wish) {
        let api: Api<crd::Volume> = Api::all(ctx.client.clone());
        let patch = serde_json::json!({"spec": {"restoreTo": wish}});
        api.patch(&vol.name_any(), &PatchParams::default(), &Patch::Merge(&patch)).await?;
    }
    let st = crd::EnvironmentStatus {
        // Still `running`, exactly as the stop path is while it waits: `model::EnvState` has no
        // `Working`, and an unknown phase silently projects as `Creating` — both wrong and
        // alarming. The progress belongs in the condition below, which is where a reader looks.
        phase: crd::Phase::Running,
        // Deliberately unobserved: the restore is not finished, and the next pass has to look again.
        observed_generation: None,
        conditions: vec![crd::condition("Restoring", true, reason, message, gen)],
        // `service_status` carried over, not blanked: it is the last thing known about these
        // services, and replacing it with nothing reads as "this environment has no services".
        ..e.status.clone().unwrap_or_default()
    };
    write_env_status(e, st, ctx).await?;
    Ok(Some(Action::requeue(TICK)))
}

/// Scale every service to zero and wait, briefly, for its pods to be GONE. Returns how many are
/// still writing; zero means the subvolume has no open writers and may be snapshotted or swapped.
///
/// Waited for HERE, in this pass: a database exits in about a second, and a restore or a stop is
/// the one moment a person is watching the clock, so handing the wait to the requeue would price
/// every one at a full tick. Bounded well under the pods' grace period; a service that is still
/// shutting down after this falls back to the pod watch, which wakes the pass that finishes.
async fn drain_services(
    e: &crd::Environment,
    ns: &str,
    deployments: &Api<StatefulSet>,
    ctx: &Arc<Ctx>,
) -> Result<usize, ReconcileErr> {
    for svc in &e.spec.services {
        // A merge patch on `replicas` alone: scaling is not a claim on the rest of a StatefulSet
        // spec the reconcile re-applies a few lines later.
        let patch = serde_json::json!({"spec": {"replicas": 0}});
        // The scale happens behind `ensure`'s back, so its memory of this set is wrong from here:
        // without this the re-apply that brings the replicas back is skipped as "unchanged".
        forget_applied(ctx, "StatefulSet", ns, &svc.name);
        match deployments.patch(&svc.name, &PatchParams::default(), &Patch::Merge(&patch)).await {
            Ok(_) => {}
            // Nothing to scale down is the desired state already reached.
            Err(kube::Error::Api(s)) if s.code == 404 => {}
            Err(err) => return Err(err.into()),
        }
    }
    let mut remaining = writing_pods(ns, ctx).await?;
    for _ in 0..40 {
        if remaining == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
        remaining = writing_pods(ns, ctx).await?;
    }
    Ok(remaining)
}

/// What a fixed-name stop request says about its push: landed, failed, or still to wait for
/// (including "just created it"). The caller writes ITS OWN status — the two parent kinds share
/// no status type, and this is the one place the request's lifecycle is decided.
pub(crate) enum StopPush {
    Landed,
    Failed,
    Waiting,
}

/// The stop-before-teardown gate a stopping parent waits on: create the request once, then wait
/// until its own status says `done`.
///
/// The parent generation a stop request was created for. An annotation, not a label: nothing
/// selects on it, and a label is a view of `spec` while this is a fact about the request itself.
const STOP_GENERATION: &str = "rustic-git.io/stop-generation";

/// One object named `stop-{env}` / `stop-home-{ws}` per parent, not one per pass: a fresh request
/// each pass would be an unbounded stream of pushes for one stopping parent. The caller DELETES it
/// once its teardown completes, so the next stop of the same parent creates a fresh one instead of
/// finding the old `done` and pushing nothing.
///
/// Only `Landed` proceeds. `Failed` leaves the parent running with `Ready=False` and nothing torn
/// down: a parent torn down without a landed push loses its last state for good, so a push that
/// failed must stop the teardown rather than wave it through. `await_change` is safe there because
/// both parent controllers watch `SnapshotRequest` and map it back by ownerReference — the parent
/// is woken by the request's own status moving, and by an operator deleting it and letting the
/// `None` arm below create a fresh one.
///
/// The one `error` this retries itself is `AgentRestarted`: the push did not FAIL, the process
/// holding its handle died, and `/v1` has no delete for SnapshotRequests — so left alone, the
/// fixed-name request parks the parent until someone finds `kubectl`. A re-run is safe there: the
/// engine's `unpushed` stage mark makes a retried push resume, not re-snapshot. A real
/// `PushFailed` still parks, because a btrfs send that failed once fails the same way at TICK.
///
/// A finished request from an EARLIER generation of the parent is absent too. The obvious recovery
/// from `StopSnapshotFailed` is Start, which the Running arm serves without touching the leftover
/// request — and the next Stop would then read its `error` and park on the first pass, before any
/// push was attempted, until someone finds `kubectl`. A stale `done` is the same mistake the other
/// way: a stop that was abandoned for Start and landed later would let the NEXT stop tear down
/// without pushing what ran since. The parent's generation at creation is stamped on the request
/// (`STOP_GENERATION`); Start and Stop each bump it. A request without the stamp — from before it
/// existed — is taken as current, so a rollout does not restart a push in flight.
async fn stop_push<P>(name: &str, owner: &str, volume: &str, parent: &P, ctx: &Arc<Ctx>) -> Result<StopPush, ReconcileErr>
where
    P: Resource<DynamicType = ()> + ResourceExt,
{
    let api: Api<crd::SnapshotRequest> = Api::all(ctx.client.clone());
    // A request being deleted is ABSENT. The teardown deletes this object, and a `done` one that is
    // still terminating (a finalizer holds it) would otherwise read as a landed push for the NEXT
    // stop — tearing that one down without pushing at all.
    let req = api.get_opt(name).await?.filter(|r| r.metadata.deletion_timestamp.is_none());
    let mut phase = req.as_ref().map(|r| r.status.as_ref().map(|s| s.phase).unwrap_or(crd::Phase::Pending));
    let restarted = req
        .as_ref()
        .and_then(|r| r.status.as_ref())
        .is_some_and(|s| s.phase == crd::Phase::Error && s.conditions.iter().any(|c| c.reason == "AgentRestarted"));
    let gen = parent.meta().generation.unwrap_or(0).to_string();
    let stale = req.as_ref().is_some_and(|r| {
        matches!(phase, Some(crd::Phase::Done | crd::Phase::Error))
            && r.annotations().get(STOP_GENERATION).is_some_and(|g| *g != gen)
    });
    if restarted || stale {
        delete_ignoring_404(&api, name).await?;
        // Absent now, so this same pass creates the fresh one (a 409 from a still-terminating
        // object is the "same request" case below, and the next pass gets through).
        phase = None;
    }
    match phase {
        Some(crd::Phase::Done) => Ok(StopPush::Landed),
        Some(crd::Phase::Error) => Ok(StopPush::Failed),
        Some(_) => Ok(StopPush::Waiting),
        None => {
            let mut req = crd::snapshot_request(name, owner, volume, Some("stopping".into()));
            // Owned by the parent so the request's own events map back to it — that watch is what
            // wakes the `Failed` arm. NOT a cascade-delete convenience: the request is deleted
            // explicitly after teardown, and by then it has already outlived its usefulness.
            req.metadata.owner_references = Some(vec![owner_ref_of_kind(parent)?]);
            // The label both parent controllers select their request watch by. A view, like every
            // label here — the ownerReference above is what the mapper actually reads.
            req.metadata.labels.get_or_insert_with(Default::default).insert(crd::STOP_LABEL.to_string(), parent.name_any());
            req.metadata.annotations.get_or_insert_with(Default::default).insert(STOP_GENERATION.to_string(), gen);
            match api.create(&PostParams::default(), &req).await {
                // Lost the race with our own earlier pass; it is the same request either way.
                Ok(_) => {}
                Err(kube::Error::Api(s)) if s.code == 409 => {}
                Err(err) => return Err(err.into()),
            }
            Ok(StopPush::Waiting)
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

pub(crate) async fn write_env_status(e: &crd::Environment, st: crd::EnvironmentStatus, ctx: &Arc<Ctx>) -> Result<(), ReconcileErr> {
    write_status(e, "Environment", e.status.as_ref(), &st, ctx, |a, b| {
        a.phase == b.phase
            && a.observed_generation == b.observed_generation
            && a.node_name == b.node_name
            && a.compatible_nodes == b.compatible_nodes
            && a.volume_ref == b.volume_ref
            && a.service_status == b.service_status
            // See `write_ws_status`'s twin comment: without these, a head-only advance is a no-op.
            && a.head == b.head
            && a.durable == b.durable
            && conditions_eq(&a.conditions, &b.conditions)
    })
    .await
}

// ── shared plumbing ──────────────────────────────────────────────────────

/// How long `ensure` trusts its last apply. Bounds the one thing the skip costs: a child deleted by
/// hand, or scaled by a path that forgot to `forget_applied`, is re-applied within this.
const APPLY_RESYNC: Duration = Duration::from_secs(600);

/// Server-side apply of a whole child object: level-triggered convergence in one call, and the one
/// thing that makes "someone deleted the StatefulSet by hand" a self-healing event.
///
/// Skipped when the body hashes to what this process last applied under this name, less than
/// `APPLY_RESYNC` ago. A converged parent reconciles on every event of every child — each pod
/// transition re-applied ~10 objects per workspace and 8 + 4·S per environment, all no-ops on the
/// server and all PATCHes on the API server's ledger.
/// ponytail: the memory is per-process and time-bounded, not watch-driven — a child deleted by
/// hand comes back on the next apply after `APPLY_RESYNC`, not on its delete event. Any path that
/// changes a child OUTSIDE `ensure` (a scale, a delete) must `forget_applied` it first.
pub(crate) async fn ensure<K>(api: &Api<K>, obj: &K, ctx: &Ctx) -> Result<(), ReconcileErr>
where
    K: Resource + Clone + serde::Serialize + serde::de::DeserializeOwned + std::fmt::Debug,
    K::DynamicType: Default,
{
    let name = obj.meta().name.clone().ok_or_else(|| ReconcileErr("child object has no name".into()))?;
    let key = applied_key(&K::kind(&Default::default()), obj.meta().namespace.as_deref(), &name);
    let hash = {
        use std::hash::{Hash, Hasher};
        let mut h = std::hash::DefaultHasher::new();
        serde_json::to_vec(obj).map_err(|e| ReconcileErr(e.to_string()))?.hash(&mut h);
        h.finish()
    };
    let fresh = ctx
        .applied
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(&key)
        .is_some_and(|(h, at)| *h == hash && at.elapsed() < APPLY_RESYNC);
    if fresh {
        return Ok(());
    }
    api.patch(&name, &PatchParams::apply(crd::AGENT_FIELD_MANAGER).force(), &Patch::Apply(obj)).await?;
    ctx.applied.lock().unwrap_or_else(|p| p.into_inner()).insert(key, (hash, std::time::Instant::now()));
    Ok(())
}

fn applied_key(kind: &str, ns: Option<&str>, name: &str) -> String {
    format!("{kind}/{}/{name}", ns.unwrap_or_default())
}

/// Drop `ensure`'s memory of one child, so the next pass applies it again whatever the hash says.
/// Called wherever a child is changed by something other than `ensure` — its absence there is a
/// service that stays scaled to zero after a restore, or never comes back after a stop.
pub(crate) fn forget_applied(ctx: &Ctx, kind: &str, ns: &str, name: &str) {
    ctx.applied.lock().unwrap_or_else(|p| p.into_inner()).remove(&applied_key(kind, Some(ns), name));
}

/// Create a child only when it is missing — for objects an apply cannot legally change: Pods.
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
