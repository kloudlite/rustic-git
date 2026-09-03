//! Starting the controller: the three `Controller` builders, the watches they open, the background
//! beats they spawn, and the error policy. Nothing here decides anything about a workspace — this
//! is the wiring the reconcilers hang off. Split out of `controller.rs` unchanged.

use super::{reconcile_environment, reconcile_volume, reconcile_workspace, Ctx, Done, ReconcileErr, RETRY};
use crate::{binding, claim, snapshot};
use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::core::v1::{Node, Pod};
use rustic_git_workspaces::k8s;
use futures::StreamExt;
use kube::runtime::controller::{Action, Controller};
use kube::runtime::watcher;
use kube::{Api, Resource, ResourceExt};
use rustic_git_workspaces::crd;
use std::sync::Arc;

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

/// Every object a controller currently holds, as reconcile requests.
///
/// The mapper for the Node watch below: a change to THIS node is not about one workspace, it is
/// about all of them, and a controller's own reflector store is exactly "the objects I host". The
/// mapper is a sync `FnMut` that must not do I/O — reading the store is a lock, not a request.
fn all_in_store<K>(store: &kube::runtime::reflector::Store<K>) -> Vec<kube::runtime::reflector::ObjectRef<K>>
where
    K: Resource<DynamicType = ()> + Clone + 'static,
{
    store.state().iter().map(|o| kube::runtime::reflector::ObjectRef::from_obj(o.as_ref())).collect()
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
    spawn_pull(ctx.clone());
    spawn_sync(ctx.clone());
    spawn_decommission(ctx.clone());
    // NB the RBAC grant is cluster-wide — a field selector narrows a watch, never authorization.
    let mine = watcher::Config::default().fields(&format!("spec.nodeName={}", ctx.node));
    // The completion wake-ups (see `wake_on_finish`). Taken once; a second `run` on one Ctx would
    // be two agents in one process, which is not a thing.
    let (vol_wakes, ws_wakes) =
        ctx.wakes.lock().unwrap_or_else(|p| p.into_inner()).take().ok_or("the wake channels are already taken")?;
    // ONE watch on this node's Volumes, shared. The Volume controller reconciles from it, and the
    // three parents that wait on a Volume's status subscribe to it — four watches on the same
    // objects was N_nodes × 4 long-running requests on the API server for one stream of events.
    let writer =
        ctx.volume_writer.lock().unwrap_or_else(|p| p.into_inner()).take().ok_or("the volume writer is already taken")?;
    let subscribe = || writer.subscribe().ok_or("the volume store is not shared");
    let (vol_self, vol_ws, vol_env) = (subscribe()?, subscribe()?, subscribe()?);
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
    // No Node watch on this controller, deliberately: `apply_volume` reads the node only through
    // `my_node`'s dead-guard, which returns `requeue(TICK)` rather than `await_change()` — it
    // re-reads on its own within 15 s. Nothing a Volume writes depends on the decommission label.
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
    // THIS node's own object, nothing else in the cluster. A converged parent ends in
    // `await_change()`, so without this watch a decommission label landing on the Node reached
    // nobody: the annotation said `running=1` while every workspace on it carried no
    // `Decommissioning` condition, for as long as nothing else happened to touch them. Removing
    // the label was just as stuck, leaving a stale notice forever. Readiness moves the same way,
    // so `my_node`'s dead-guard sees a change at once instead of on the next 15s tick.
    let my_node_only = watcher::Config::default().fields(&format!("metadata.name={}", ctx.node));
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
        // A workspace no longer creates a stop request of its own (the home is on NFS now, spec
        // 2026-09-01) — this stays only because an Environment's `stop-{env}` request carries the
        // same `stop-of` label and this watch is shared plumbing with `owned_by` doing the filter.
        .watches(
            Api::<crd::Snapshot>::all(ctx.client.clone()),
            watcher::Config::default().labels(crd::STOP_LABEL),
            |r| owned_by::<crd::Workspace, _>(&r),
        );
    // Every workspace this node hosts wakes on its own Node changing — see `my_node_only`. The
    // store is read at mapper time, not now, so a workspace claimed later is included too.
    let ws_store = workspaces.store();
    let workspaces = workspaces
        .watches(Api::<Node>::all(ctx.client.clone()), my_node_only.clone(), move |_: Node| all_in_store(&ws_store))
        .shutdown_on_signal()
        .run(|w, c| timed("workspace", reconcile_workspace(w, c)), error_policy, ctx.clone())
        .for_each(|r| async move {
            if let Err(e) = r {
                tracing::warn!(error = %e, "workspace reconcile")
            }
        });
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
            Api::<crd::Snapshot>::all(ctx.client.clone()),
            watcher::Config::default().labels(crd::STOP_LABEL),
            |r| owned_by::<crd::Environment, _>(&r),
        );
    let env_store = environments.store();
    let environments = environments
        .watches(Api::<Node>::all(ctx.client.clone()), my_node_only, move |_: Node| all_in_store(&env_store))
        .shutdown_on_signal()
        .run(|e, c| timed("environment", async move { reconcile_environment(e, c).await }), error_policy, ctx.clone())
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
    // NOT `mine`: a binding is no longer node-scoped (the home is on shared NFS), and the
    // namespaces it ensures must exist on whichever node the claim picks. Every node reconciles
    // every binding; every object it writes is a forced server-side apply, so concurrent
    // reconcilers converge on the same result rather than fighting.
    let bindings = Controller::new(Api::<crd::OwnerBinding>::all(ctx.client.clone()), watcher::Config::default())
        // A new Workspace of this owner may need a new TEAM namespace, so the binding reconciles
        // on it. Mapped by `spec.owner`, not by ownerReference: the binding is not the Workspace's
        // parent, it is the thing that makes its namespace exist. Only the ones placed HERE,
        // because `teams_in_use` builds the namespace set from this node's workspaces — a
        // workspace elsewhere wakes ITS node's copy of this same reconciler.
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
    // The `Snapshot` kind: no finalizer (see `snapshot::reconcile_commit`'s module doc), so a
    // plain watch over every one in the cluster is enough.
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
    let everything = async {
        tokio::join!(
            volume_watch,
            volumes,
            workspaces,
            environments,
            bindings,
            commits,
            futures::future::OptionFuture::from(claim_ws),
            futures::future::OptionFuture::from(claim_env),
        );
    };
    // The controllers stop on SIGTERM (`shutdown_on_signal`), but `volume_watch` is a bare watch
    // stream with no such hook and never ends — so the `join!` above never resolved, `run` never
    // returned, and every agent restart sat through the full 120 s grace period before the
    // kubelet SIGKILLed it (exit 137, observed on every delete). Race the join against the same
    // signal instead: on SIGTERM give the controllers a bounded window to drain their in-flight
    // reconciles, then return so the process exits on its own. Nothing here needs a clean stop —
    // every reconcile is idempotent by design and re-runs on the next boot.
    tokio::pin!(everything);
    tokio::select! {
        _ = &mut everything => {}
        _ = shutdown_signal() => {
            tracing::info!("shutdown signal; draining in-flight reconciles");
            let _ = tokio::time::timeout(std::time::Duration::from_secs(20), &mut everything).await;
        }
    }
    Ok(())
}

/// SIGTERM (the kubelet) or SIGINT (a terminal). Tokio multiplexes signal listeners, so this
/// coexists with the handlers `shutdown_on_signal` installs on each controller.
async fn shutdown_signal() {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM handler");
    tokio::select! {
        _ = term.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
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

/// The commit model's puller: its own beat, so a slow pull never delays a reconcile — plus the
/// wake, so a stop or a clone is replicated in seconds instead of at the next tick. A pass already
/// running finishes and then runs ONCE more (the pending flag), never concurrently: two receives of
/// the same volume buy nothing but disk contention.
fn spawn_pull(ctx: Arc<Ctx>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(crate::peer::replica_interval());
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let wake = ctx.pull_wake.clone();
        let mut next = crate::peer::Next::Wait;
        let mut misses = 0;
        loop {
            match next {
                // A pass that could not fetch something retries in 30 s instead of at the next
                // tick, backing off to the ordinary tick if it keeps missing — a wake still wins
                // the race, so a stop or a clone is never delayed by it.
                crate::peer::Next::RetrySoon(d) => {
                    tokio::select! {
                        _ = tokio::time::sleep(d) => {}
                        _ = wake.notified() => {}
                    }
                }
                crate::peer::Next::Wait => {
                    tokio::select! {
                        _ = tick.tick() => {}
                        _ = wake.notified() => {}
                    }
                }
                crate::peer::Next::RunAgain => {}
            }
            let missed = crate::peer::pull_beat(&ctx).await;
            // Wakes that arrived DURING the pass decide whether to go straight round again.
            next = crate::peer::after_pass(&wake, missed, &mut misses);
        }
    });
}

/// The decommission beat (`decommission.rs`), same shape as the others. It costs one node list per
/// 30 s on every node and returns immediately unless THIS node carries the label — cheaper than a
/// watch on Nodes, and a beat is the right shape anyway: what it waits for (a person stopping their
/// workspace) is observed through the same listing everything else already reads.
fn spawn_decommission(ctx: Arc<Ctx>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(crate::decommission::beat_interval());
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            crate::decommission::decommission_beat(&ctx).await;
        }
    });
}

/// The sync beat (`sync.rs`), same shape as `spawn_pull`: a plain ticker, because the bytes it
/// watches change under a running pod without producing any Kubernetes event to reconcile on.
fn spawn_sync(ctx: Arc<Ctx>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(crate::sync::sync_interval());
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            crate::sync::sync_beat(&ctx).await;
        }
    });
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

#[cfg(test)]
mod tests {
    use super::*;
    use kube::runtime::reflector::{store, ObjectRef};

    fn ws(name: &str) -> crd::Workspace {
        serde_json::from_value(serde_json::json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "Workspace",
            "metadata": {"name": name, "uid": format!("{name}-uid")},
            "spec": {"owner": "alice", "team": "", "name": name, "region": "r1",
                     "image": "nginx:alpine", "desiredState": "running", "packages": []},
        }))
        .unwrap()
    }

    /// One Node event is not about one workspace, it is about every workspace this node hosts —
    /// a converged parent sits in `await_change()`, so the label reaches it only if the mapper
    /// names it. Empty in, empty out: a node event before the store has synced enqueues nothing
    /// rather than panicking, and the next sync brings its own events.
    #[test]
    fn a_node_event_maps_to_every_object_the_controller_holds() {
        let (reader, mut writer) = store::<crd::Workspace>();
        assert!(all_in_store(&reader).is_empty(), "nothing hosted yet, nothing to reconcile");

        writer.apply_watcher_event(&kube::runtime::watcher::Event::Apply(ws("ws-1")));
        writer.apply_watcher_event(&kube::runtime::watcher::Event::Apply(ws("ws-2")));

        let mut refs = all_in_store(&reader);
        refs.sort_by_key(|r| r.name.clone());
        assert_eq!(refs, vec![ObjectRef::new("ws-1"), ObjectRef::new("ws-2")]);
    }
}
