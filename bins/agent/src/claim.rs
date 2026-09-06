//! Placement, as a reconciler.
//!
//! An object with an empty `status.nodeName` is UNPLACED. Each agent runs a second watch selecting
//! exactly those, and the first node whose claim lands wins. The claim is a status write and only a
//! status write: the API authored this object's spec, and a controller that edits a user's desired
//! state is the failure this whole design exists to remove.
//!
//! A FRESH claim also checks free space (`fits`): a node that takes a workspace it cannot schedule
//! parks the pod `Pending` forever while a peer sits idle, and nothing un-places a live node's
//! claim. Only fresh — a parent whose bytes are here is claimed regardless of capacity, because
//! correctness beats packing.

use crate::controller::{replace_status, Ctx, ReconcileErr};
use k8s_openapi::api::core::v1::{Node, Pod};
use kube::api::{Api, ListParams, PostParams};
use kube::runtime::controller::Action;
use kube::{Resource, ResourceExt};
use kloudlite_workspaces::crd::{self, binding_name, OwnerBinding, OwnerBindingSpec};
use std::sync::Arc;

/// What the claim needs to know about the volume behind an unplaced object, gathered once in
/// `decide` (async) and handed to the pure, testable `may_claim` below.
struct Placement {
    /// Any `Snapshot` CR for this volume, Ready or not — "a snapshot was ever started" is enough to
    /// leave the never-started-dataless guard armed; only a volume with none at all is bootstrap.
    has_snapshots: bool,
    /// THIS node's own replica row, or `None` when it has never pulled the volume.
    my_replica: Option<crd::VolumeReplica>,
    /// The worktree's newest Ready transient, cluster-wide — the name `my_replica` must hold.
    newest_transient: Option<String>,
    worktree: String,
}

/// Whether THIS node may claim the object, and the ONE placement rule in the system: the owner
/// always, any other node only when it is up to date for the worktree being claimed.
///
/// This is the check that used to sit in `stop_push`'s flush gate, holding a person's stop open to
/// answer a question nobody was asking yet. It belongs here, where the answer is actually used —
/// and it is why a stop is now instant and a cross-node START is what waits.
///
/// `compatibleNodes` is gone: it was a memory of "who held this once", and holding it once is not
/// holding it now. A volume with no snapshots at all is still bootstrap, claimable by anyone,
/// because there are no bytes anywhere for a claim to be near.
fn may_claim(me: &str, owner: &str, p: &Placement) -> bool {
    if !p.has_snapshots {
        return true;
    }
    if owner == me {
        return true;
    }
    p.my_replica.as_ref().is_some_and(|r| crate::peer::up_to_date(r, &p.worktree, p.newest_transient.as_deref()))
}

/// cpu millicores and memory MiB, the two dimensions the scheduler packs against.
type Want = (u64, u64);

/// The REQUESTS already committed on this node: every non-terminated pod assigned to it, summed
/// over its containers. `Succeeded`/`Failed` pods are excluded — the kubelet has released their
/// capacity, and counting them would refuse claims on a node full of yesterday's completed jobs.
///
/// ponytail: containers only, not `max(sum(containers), max(initContainers))` as the scheduler
/// does. Nothing we build gives an init container requests, so the two agree today; if some image
/// ever does, fold the init max in here.
fn requested(pods: &[Pod]) -> Want {
    pods.iter()
        .filter(|p| !matches!(p.status.as_ref().and_then(|s| s.phase.as_deref()), Some("Succeeded" | "Failed")))
        .flat_map(|p| p.spec.iter().flat_map(|s| s.containers.iter()))
        .filter_map(|c| c.resources.as_ref()?.requests.as_ref())
        .fold((0, 0), |(cpu, mem), r| {
            let q = |k: &str| r.get(k).map(|v| v.0.as_str()).unwrap_or_default().to_string();
            (cpu + kloudlite_workspaces::quota::millicores(&q("cpu")), mem + kloudlite_workspaces::quota::mebibytes(&q("memory")))
        })
}

/// What percentage of a node's allocatable the model lets a claim fill (`docs/capacity-model.md`).
///
/// A WORKSPACE node (`kloudlite.io/session=true` — the sheet calls a workspace a session) admits up
/// to the guarantee and no further: "guaranteed CPU is NOT oversubscribed on session nodes". An
/// ENV node is packed to the model's 80% target instead, leaving the steady-state headroom the
/// sheet prices. A node carrying BOTH role labels (a single-node install does) is treated as a
/// workspace node, which is the rule that admits the workspace slot in full.
///
/// The sheet's 45% average utilisation is deliberately NOT here: it prices the fleet, and using it
/// to admit more than the guarantees would sell capacity that is not there.
fn admissible_pct(node: Option<&Node>) -> u64 {
    let label = |k: &str| node.and_then(|n| n.metadata.labels.as_ref()).and_then(|l| l.get(k)).map(String::as_str) == Some("true");
    if !label("kloudlite.io/session") && label("kloudlite.io/env") {
        80
    } else {
        100
    }
}

/// Whether `want` still fits on this node once `pods`' requests are subtracted from its
/// ALLOCATABLE — not its capacity: allocatable is already capacity minus the kubelet's
/// `--system-reserved`/`--kube-reserved` and eviction threshold, so the system margin is accounted
/// for and adding a second one here would double-count it. The model's own headroom is the pool's,
/// via `admissible_pct`.
///
/// A node with no allocatable readable (no status yet) fits nothing: refusing leaves the parent
/// visibly unplaced for a peer, where claiming on a guess strands it.
fn fits(node: Option<&Node>, pods: &[Pod], want: Want) -> bool {
    let alloc = node.and_then(|n| n.status.as_ref()).and_then(|s| s.allocatable.as_ref());
    let q = |k: &str| alloc.and_then(|a| a.get(k)).map(|v| v.0.as_str()).unwrap_or_default().to_string();
    let pct = admissible_pct(node);
    let cpu = kloudlite_workspaces::quota::millicores(&q("cpu")) * pct / 100;
    let mem = kloudlite_workspaces::quota::mebibytes(&q("memory")) * pct / 100;
    let (used_cpu, used_mem) = requested(pods);
    cpu.saturating_sub(used_cpu) >= want.0 && mem.saturating_sub(used_mem) >= want.1
}

/// Gathers `Placement` for `volume` (`None` when the child `Volume` has not been created yet —
/// every workspace/environment starts that way, and that IS the bootstrap case). Errors propagate
/// rather than being swallowed: a claim decided on a partial read of "does anyone have this" is
/// exactly the never-started-dataless bug the guard exists to prevent.
async fn placement(
    ctx: &Arc<Ctx>,
    volume: Option<&str>,
    worktree: &str,
    // `Some` only for a `SeededFrom`: the bar is that exact cut, so there is nothing to list.
    pinned_cut: Option<&str>,
) -> Result<Placement, ReconcileErr> {
    let Some(volume) = volume else {
        return Ok(Placement { has_snapshots: false, my_replica: None, newest_transient: None, worktree: worktree.into() });
    };
    let has_snapshots = has_snapshots(ctx, volume).await?;
    let my_replica = Api::<crd::VolumeReplica>::all(ctx.client.clone()).get_opt(&crd::replica_name(volume, &ctx.node)).await?;
    let newest_transient = match pinned_cut {
        Some(cut) => Some(cut.to_string()),
        None => crate::peer::newest_transient(ctx, volume, worktree).await?,
    };
    Ok(Placement { has_snapshots, my_replica, newest_transient, worktree: worktree.into() })
}

/// Any `Snapshot` CR for `volume`, Ready or not. Shared by the claim's own bootstrap check and by
/// `apply_workspace`'s materialize guard (F2): both must agree on "this volume has snapshots", or a
/// claim's bootstrap read and the materialize step's guard read could disagree about the same
/// volume. Errors propagate — never read a listing failure as "bootstrap, nothing here yet".
pub(crate) async fn has_snapshots(ctx: &Arc<Ctx>, volume: &str) -> Result<bool, ReconcileErr> {
    let snaps: Api<crd::Snapshot> = Api::all(ctx.client.clone());
    Ok(!snaps.list(&ListParams::default().fields(&format!("spec.volume={volume}"))).await?.items.is_empty())
}

/// The PHASE of `snapshot` as a `Snapshot` of `volume`, or `None` when no such snapshot of this
/// volume exists — the check a clone's grafted snapshot and a restore's wished snapshot both need
/// before checking out or swapping onto it, so naming a retention-deleted (or foreign-volume)
/// snapshot is caught here rather than as a bare btrfs `NO_SUCH_RECORD` with no distinct reason a
/// person could search for. Errors propagate, same rule as `has_snapshots`: a listing failure must
/// never read as "no such snapshot".
///
/// The phase, not a bool: a clone is created microseconds after its own cut, so the reconcile that
/// follows sees a `Working` snapshot almost every time. Reading that as "not ready" and settling
/// PERMANENT killed every clone at birth. Absent is forever; `Working` is one tick away.
pub(crate) async fn snapshot_phase(ctx: &Arc<Ctx>, volume: &str, snapshot: &str) -> Result<Option<crd::Phase>, ReconcileErr> {
    let snaps: Api<crd::Snapshot> = Api::all(ctx.client.clone());
    Ok(snaps
        .get_opt(snapshot)
        .await?
        .filter(|s| s.spec.volume == volume)
        // A Snapshot with no status block yet has never been cut: `status` is a subresource, so
        // one is born status-less and `reconcile_snapshot` reads that as `Working` too.
        .map(|s| s.status.as_ref().map(|st| st.phase).unwrap_or(crd::Phase::Working)))
}

/// Whether a clone may still be waiting for this snapshot rather than being wrong about it forever.
/// `Error` is the one terminal non-Ready phase; everything else in flight converges.
pub(crate) fn snapshot_pending(phase: Option<crd::Phase>) -> bool {
    matches!(phase, Some(crd::Phase::Working | crd::Phase::Pending | crd::Phase::Creating))
}

/// The SOURCE volume of a `cloneOf`, whose name is also the source worktree's: a clone holds
/// nothing of its own yet, so it places by the same up-to-date rule read over what it is a copy
/// OF. This replaces `source_nodes`, which pinned a clone to the source volume's `nodeName`
/// unconditionally — which is why a clone of a released or dead-node source could never start
/// anywhere at all.
/// The second element is the ONE cut this clone may be seeded from, and only a `SeededFrom` has
/// one: its bytes come from a specific local read-only copy, so "up to date" for it means holding
/// exactly THAT cut, not merely the newest one the cluster knows about. A `CloneOf` keeps reading
/// the newest transient, as before.
fn clone_source(storage: Option<&crd::WorkspaceStorage>) -> Option<(&str, Option<&str>)> {
    match storage.and_then(|s| s.source.as_ref()) {
        Some(crd::VolumeSource::CloneOf { volume, .. }) => Some((volume, None)),
        Some(crd::VolumeSource::SeededFrom { volume, snapshot }) => Some((volume, Some(snapshot))),
        _ => None,
    }
}

/// A pre-migration object: it names its node in the DEPRECATED `spec.nodeName` and has never had a
/// `status.nodeName`, so it matches the unplaced watch while already being placed. Claiming it here
/// would hand it to whichever agent saw it first, ignoring the node its subvolume is actually on.
/// The startup migration is what moves these onto status.
/// What the claim decides for one object.
enum Verdict {
    /// The status to write.
    Claim(serde_json::Value),
    /// Leave it alone — someone else's, or not ours to answer.
    Decline,
    /// Leave it alone AND, once it has gone unclaimed long enough, say why: nobody has room. The
    /// string is the user-facing message.
    NoCapacity(String),
}

/// How long a fresh parent may go unclaimed before a node that declined it for capacity writes
/// `Placed=False/NoCapacity`. Long enough that a peer with room wins the race first in the normal
/// case; short enough that "Creating" never sits unexplained for minutes.
const NO_CAPACITY_AFTER_SECS: i64 = 60;

/// What the claim decides for one object, given what it currently says about itself.
///
/// Split out because a 409 has to run the SAME decision against the re-read object: the peer that
/// beat us may have placed it (leave it), or may have written something else entirely (still ours
/// to claim). A second, subtly different decision on the retry path is how a loser talks itself into
/// overwriting a winner.
async fn decide(ctx: &Arc<Ctx>, name: &str, p: &Parts<'_>, phase: crd::Phase, gen: i64) -> Result<Verdict, ReconcileErr> {
    use Verdict::Decline;
    let (storage, volume, want) = (p.storage, p.volume, p.want);
    if !p.node_name.is_empty() {
        // Already placed: the disk has not moved, so a later start reconciles here with no
        // placement step at all.
        return Ok(Decline);
    }
    // A node with no `WS_HOMES_EXPORT` cannot serve `/home/kl` at all, and nothing ever un-places a
    // live node's claim — so claiming here parks the object at `HomeNotReady` permanently instead
    // of leaving it for a node that can serve it. Refusing keeps it visibly unplaced, which a peer
    // picks up on its own unplaced watch.
    if ctx.homes_export.is_none() {
        return Ok(Decline);
    }
    // A node being retired takes no new work: the label is the operator's decision and the claim
    // is where it has to bite, or a drain never finishes because new workspaces keep landing.
    let me = Api::<Node>::all(ctx.client.clone()).get_opt(&ctx.node).await?;
    if crate::peer::unplaceable(me.as_ref(), crate::peer::node_dead_secs(&ctx.settings), k8s_openapi::jiff::Timestamp::now()) {
        return Ok(Decline);
    }
    // A clone is decided over its SOURCE's volume and worktree; everything else over its own. Both
    // go through the same rule — there is no "same node as the source" policy any more.
    let (volume, worktree, pinned_cut) = match clone_source(storage) {
        Some((v, cut)) => (Some(v), v, cut),
        None => (volume, name, None),
    };
    // The volume's CURRENT owner, not a remembered one.
    let owner = match volume {
        Some(v) => Api::<crd::Volume>::all(ctx.client.clone()).get_opt(v).await?.map(|x| x.spec.node_name).unwrap_or_default(),
        None => String::new(),
    };
    // Only the owner may claim a parent whose volume it owns, full stop — an up-to-date non-owner
    // is otherwise allowed to claim (see `may_claim`), which is exactly what turns the mismatch
    // arm's self-heal into a ping-pong: it un-places, and the very next pass here re-claims the
    // same parent before the live owner's own reconcile gets to it. A DEAD owner is not this
    // guard's business — `may_claim`'s up-to-date rule is what lets a replacement take over.
    if !owner.is_empty() && owner != ctx.node {
        let owner_node = Api::<Node>::all(ctx.client.clone()).get_opt(&owner).await?;
        if !crate::peer::unplaceable(owner_node.as_ref(), crate::peer::node_dead_secs(&ctx.settings), k8s_openapi::jiff::Timestamp::now()) {
            return Ok(Decline);
        }
    }
    let p = placement(ctx, volume, worktree, pinned_cut).await?;
    if !may_claim(&ctx.node, &owner, &p) {
        return Ok(Decline);
    }
    // The capacity gate, and ONLY on the fresh path: `may_claim` let this through because there are
    // no bytes anywhere yet, so every node is equally correct and the one with room should take it.
    // A parent with snapshots has already been decided by where its data is, and no amount of
    // crowding makes another node a right answer for it — hence no Pod list on that path either.
    if !p.has_snapshots {
        let pods: Api<Pod> = Api::all(ctx.client.clone());
        let mine = pods.list(&ListParams::default().fields(&format!("spec.nodeName={}", ctx.node))).await?.items;
        if !fits(me.as_ref(), &mine, want) {
            let why = format!("no node has room for it: it requests {}m cpu and {} MiB", want.0, want.1);
            tracing::info!(%name, cpu_m = want.0, mem_mi = want.1, "claim.declined.capacity");
            return Ok(Verdict::NoCapacity(why));
        }
    }
    Ok(Verdict::Claim(serde_json::json!({
        "phase": phase,
        "nodeName": ctx.node,
        "conditions": [crd::condition("Placed", true, "Claimed", &format!("claimed by {}", ctx.node), gen)],
    })))
}

/// One optimistic attempt, then — on 409 — one re-read and one more.
///
/// Two passes, not a loop: a third attempt against a peer that keeps winning is a hot loop over the
/// API server, and the peer's own write is a watch event that brings this object back anyway. So
/// the fallback is always `await_change()`, never a requeue.
const ATTEMPTS: usize = 2;

/// Seconds since the object was created, as the stand-in for "unclaimed for": this arm is only
/// ever reached on the FRESH path, where the object has never been placed at all, so its age IS
/// how long nothing has had room for it.
fn unplaced_for<K: Resource<DynamicType = ()>>(obj: &K) -> i64 {
    obj.meta()
        .creation_timestamp
        .as_ref()
        .map(|t| k8s_openapi::jiff::Timestamp::now().as_second() - t.0.as_second())
        .unwrap_or(0)
}

/// Everything the claim needs out of one object, whatever its kind.
struct Parts<'a> {
    node_name: String,
    storage: Option<&'a crd::WorkspaceStorage>,
    /// The child `Volume`'s name, once the reconciler has created and reported it — `None` for
    /// every object that has never been placed at all, which the snapshot-model arm reads as "no
    /// snapshots, bootstrap".
    volume: Option<&'a str>,
    region: &'a str,
    owner: &'a str,
    /// What this parent's pods will REQUEST on whichever node takes it — the same numbers
    /// `k8s::quantities` writes onto the containers, read here so the claim can ask whether they
    /// fit before the scheduler is left holding an unschedulable pod.
    want: Want,
}

/// The claim itself, for any kind that carries `Parts`. Written once because a second, subtly
/// different copy of the 409 arms is exactly how a loser talks itself into overwriting a winner.
async fn claim<K>(
    obj: &K,
    ctx: &Arc<Ctx>,
    kind: &'static str,
    phase: crd::Phase,
    parts: fn(&K) -> Parts<'_>,
) -> Result<Action, ReconcileErr>
where
    K: Resource<DynamicType = ()> + Clone + serde::Serialize + serde::de::DeserializeOwned + std::fmt::Debug,
{
    let api: Api<K> = Api::all(ctx.client.clone());
    let mut obj = obj.clone();
    for attempt in 0..ATTEMPTS {
        let p = parts(&obj);
        let patch = match decide(ctx, &obj.name_any(), &p, phase, obj.meta().generation.unwrap_or(0)).await? {
            Verdict::Claim(patch) => patch,
            Verdict::Decline => return Ok(Action::await_change()),
            Verdict::NoCapacity(why) => {
                // Every node that declines writes the same condition, and `mark_parent_of`'s idle
                // check absorbs the duplicates — the same shape the dead-node sweep uses. A later
                // claim (capacity freed, a node joined) overwrites it with `Placed=True/Claimed`.
                //
                // Requeue rather than `await_change`: an unplaced parent nothing can fit generates
                // no watch events of its own, so without a timer the condition would never be
                // written and the workspace would sit silent — the bug this arm exists for.
                if unplaced_for(&obj) >= NO_CAPACITY_AFTER_SECS {
                    crate::peer::sweeps::mark_parent_of::<K>(ctx, &obj.name_any(), kind, ("Placed", false), "NoCapacity", &why, false).await;
                }
                return Ok(Action::requeue(std::time::Duration::from_secs(NO_CAPACITY_AFTER_SECS as u64 / 2)));
            }
        };
        // F1: `replace_status` PUTs the WHOLE status subresource, so a write built from ONLY the
        // 3 fields `decide` cares about would silently erase everything else already there —
        // `head`, `volumeRef`, `packages`, `podRef`. Start from THIS object's current
        // status (fresh on the first attempt; re-read on a 409 below) and merge just the claim's
        // own fields onto it, so the claim never touches anything but
        // phase/nodeName/conditions.
        let mut status = serde_json::to_value(&obj).map_err(|e| ReconcileErr(e.to_string()))?["status"].take();
        if status.is_null() {
            status = serde_json::json!({});
        }
        if let (Some(dst), Some(src)) = (status.as_object_mut(), patch.as_object()) {
            for (k, v) in src {
                dst.insert(k.clone(), v.clone());
            }
        }
        // Optimistic, carrying `metadata.resourceVersion`. NOT `patch_status`, which applies FORCED
        // and therefore never conflicts — with a forced apply two agents both "win" and the second
        // silently overwrites the first, which is the whole failure this write exists to prevent.
        match replace_status(&api, &obj, kind, status).await {
            Ok(()) => {
                // Only the WINNER binds — not for placement any more (the binding is not
                // node-scoped since the home moved to shared NFS), but because the binding is what
                // makes this owner's namespaces exist, and a loser has nothing to create them for.
                let (region, owner) = (p.region.to_string(), p.owner.to_string());
                ensure_binding(ctx, &region, &owner).await?;
                return Ok(Action::await_change());
            }
            Err(kube::Error::Api(s)) if s.code == 409 && attempt + 1 < ATTEMPTS => {
                // A peer wrote first. Re-read and re-decide rather than assuming it placed the
                // object: it may have written something else entirely.
                tracing::debug!(%kind, name = %obj.name_any(), reason = "conflict", "claim.retried");
                let name = obj.name_any();
                obj = api.get(&name).await?;
            }
            Err(kube::Error::Api(s)) if s.code == 409 => {
                tracing::debug!(%kind, name = %obj.name_any(), reason = "lost-race", "claim.refused");
                return Ok(Action::await_change());
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(Action::await_change())
}

pub async fn claim_workspace(w: &crd::Workspace, ctx: &Arc<Ctx>) -> Result<Action, ReconcileErr> {
    claim(w, ctx, "Workspace", crd::Phase::Pending, |o| {
        let st = o.status.clone().unwrap_or_default();
        Parts {
            node_name: st.node_name,
            storage: o.spec.storage.as_ref(),
            volume: o.status.as_ref().and_then(|s| s.volume_ref.as_deref()),
            region: &o.spec.region,
            owner: &o.spec.owner,
            want: want_of(&o.spec.resources),
        }
    })
    .await
}

/// One workspace pod, requesting exactly what `k8s::quantities` will write onto it.
fn want_of(r: &crd::PodResources) -> Want {
    (kloudlite_workspaces::quota::millicores(&r.cpu_request), kloudlite_workspaces::quota::mebibytes(&r.memory_request))
}

pub async fn claim_environment(e: &crd::Environment, ctx: &Arc<Ctx>) -> Result<Action, ReconcileErr> {
    // Environments have no clone-of-a-running-source path through placement: `clone_env` copies a
    // volume by id and the copy is materialized by the Volume controller, which needs the same disk
    // — the same rule, expressed through the same helper.
    claim(e, ctx, "Environment", crd::Phase::Creating, |o| {
        let st = o.status.clone().unwrap_or_default();
        Parts {
            node_name: st.node_name,
            storage: o.spec.storage.as_ref(),
            volume: o.status.as_ref().and_then(|s| s.volume_ref.as_deref()),
            region: &o.spec.region,
            owner: &o.spec.owner,
            // One StatefulSet per service, every one of them on this node and every one of them
            // requesting the env unit — the same arithmetic `quota::usage` prices an environment
            // with, from the same `env_unit_resources`.
            want: {
                let (c, m) = want_of(&kloudlite_workspaces::k8s::env_unit_resources());
                let n = o.spec.services.len() as u64;
                (c * n, m * n)
            },
        }
    })
    .await
}

/// The `{region, owner}` binding for this node, created atomically. A 409 means a peer got there
/// first and its answer is as good as ours — the binding is what makes the per-owner namespace
/// reconciler run, not a second placement decision.
pub async fn ensure_binding(ctx: &Arc<Ctx>, region: &str, owner: &str) -> Result<(), ReconcileErr> {
    let api: Api<OwnerBinding> = Api::all(ctx.client.clone());
    let name = binding_name(region, owner);
    let b = OwnerBinding::new(
        &name,
        OwnerBindingSpec { owner: owner.into(), region: region.into() },
    );
    match api.create(&PostParams::default(), &b).await {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(s)) if s.code == 409 => Ok(()),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsupport::test_ctx as shared_test_ctx;
    use kloudlite_workspaces::kube_test::{get, not_found};

    const SNAPS: &str = "/apis/kloudlite.io/v1alpha1/snapshots";

    // This module never shells to btrfs or inspects the Recorder, so the shared fixture's pool
    // path and second return value are irrelevant here — discarded rather than threaded through
    // every call site.
    fn test_ctx(routes: Vec<kloudlite_workspaces::kube_test::Route>) -> Arc<Ctx> {
        shared_test_ctx(std::path::Path::new("/tmp/claim-test"), "node-a", routes).0
    }

    fn snap_json(volume: &str, phase: Option<&str>) -> serde_json::Value {
        let mut v = serde_json::json!({
            "apiVersion": "kloudlite.io/v1alpha1", "kind": "Snapshot",
            "metadata": {"name": "clone-ws-1-cafe"},
            "spec": {"volume": volume, "owner": "karthik", "worktree": "ws-1", "parent": "", "transient": true},
        });
        if let Some(p) = phase {
            v["status"] = serde_json::json!({"phase": p});
        }
        v
    }

    /// `/v1` creates a clone's cut microseconds before the clone object that names it, so the very
    /// first reconcile of that clone sees a `Working` snapshot. Reading that as "not ready" and
    /// settling `Permanent/NoSuchSnapshot` killed every clone at birth — the phase, not a bool, is
    /// what lets the caller tell "one tick away" from "wrong forever".
    #[tokio::test]
    async fn snapshot_phase_reports_working_absent_and_status_less_apart() {
        let ctx = test_ctx(vec![get(format!("{SNAPS}/clone-ws-1-cafe"), snap_json("ws-1", Some("working")))]);
        assert_eq!(snapshot_phase(&ctx, "ws-1", "clone-ws-1-cafe").await.unwrap(), Some(crd::Phase::Working));

        // Status is a SUBRESOURCE, so a Snapshot is born status-less; that is "not cut yet" too.
        let ctx = test_ctx(vec![get(format!("{SNAPS}/clone-ws-1-cafe"), snap_json("ws-1", None))]);
        assert_eq!(snapshot_phase(&ctx, "ws-1", "clone-ws-1-cafe").await.unwrap(), Some(crd::Phase::Working));

        let ctx = test_ctx(vec![get(format!("{SNAPS}/clone-ws-1-cafe"), snap_json("ws-1", Some("ready")))]);
        assert_eq!(snapshot_phase(&ctx, "ws-1", "clone-ws-1-cafe").await.unwrap(), Some(crd::Phase::Ready));

        // Retention swept it: absent is forever.
        let ctx = test_ctx(vec![not_found(format!("{SNAPS}/clone-ws-1-cafe"))]);
        assert_eq!(snapshot_phase(&ctx, "ws-1", "clone-ws-1-cafe").await.unwrap(), None);

        // Ready, but of ANOTHER volume — as absent as a swept one, and just as permanent.
        let ctx = test_ctx(vec![get(format!("{SNAPS}/clone-ws-1-cafe"), snap_json("other-vol", Some("ready")))]);
        assert_eq!(snapshot_phase(&ctx, "ws-1", "clone-ws-1-cafe").await.unwrap(), None);
    }

    fn replica(node: &str, phase: &str, branches: &[(&str, &str)]) -> crd::VolumeReplica {
        serde_json::from_value(serde_json::json!({
            "apiVersion": "kloudlite.io/v1alpha1", "kind": "VolumeReplica",
            "metadata": {"name": format!("vol-1.{node}")},
            "spec": {"volume": "vol-1", "node": node},
            "status": {"phase": phase,
                       "branches": branches.iter().cloned().collect::<std::collections::BTreeMap<_, _>>()},
        }))
        .unwrap()
    }

    fn p(has_snapshots: bool, my_replica: Option<crd::VolumeReplica>, newest: Option<&str>) -> Placement {
        Placement {
            has_snapshots,
            my_replica,
            newest_transient: newest.map(str::to_string),
            worktree: "ws-1".into(),
        }
    }

    fn node(labels: serde_json::Value, cpu: &str, mem: &str) -> Node {
        serde_json::from_value(serde_json::json!({
            "apiVersion": "v1", "kind": "Node",
            "metadata": {"name": "node-a", "labels": labels},
            "status": {"allocatable": {"cpu": cpu, "memory": mem}},
        }))
        .unwrap()
    }

    fn pod(phase: &str, cpu: &str, mem: &str) -> Pod {
        serde_json::from_value(serde_json::json!({
            "apiVersion": "v1", "kind": "Pod", "metadata": {"name": "p", "namespace": "ws-alice"},
            "spec": {"containers": [{"name": "c", "resources": {"requests": {"cpu": cpu, "memory": mem}}}]},
            "status": {"phase": phase},
        }))
        .unwrap()
    }

    /// The predicate the cluster bug came down to: 8 vCPU allocatable, 7 already promised, and a
    /// workspace asking for the model's guaranteed 2. Quantities are PARSED — millicores and Ki —
    /// never compared as strings.
    #[test]
    fn a_workspace_node_admits_up_to_its_guarantee_and_no_further() {
        let n = node(serde_json::json!({"kloudlite.io/session": "true"}), "8", "33554432Ki");
        let want = (2000, 4096); // `PodResources::default`, parsed.
        assert!(fits(Some(&n), &[pod("Running", "5", "8Gi")], want));
        assert!(!fits(Some(&n), &[pod("Running", "7", "8Gi")], want), "1 vCPU left, 2 asked for");
        assert!(!fits(Some(&n), &[pod("Running", "1", "30Gi")], want), "memory binds first here");
        // A finished pod holds nothing: its capacity is back.
        assert!(fits(Some(&n), &[pod("Succeeded", "7", "8Gi"), pod("Failed", "7", "8Gi")], want));
        // Nothing readable is not "plenty of room".
        assert!(!fits(None, &[], want));
    }

    /// The model packs env nodes to 80% and workspace nodes to the guarantee; a node with both
    /// role labels (a single-node install) takes the workspace rule.
    #[test]
    fn an_env_node_keeps_the_models_twenty_percent_headroom() {
        let want = (2000, 4096);
        let env = node(serde_json::json!({"kloudlite.io/env": "true"}), "8", "33554432Ki");
        // 80% of 8 vCPU is 6.4; 5 promised leaves 1.4, not the 2 asked for — where a workspace
        // node would have said yes.
        assert!(!fits(Some(&env), &[pod("Running", "5", "8Gi")], want));
        assert!(fits(Some(&env), &[pod("Running", "4", "8Gi")], want));
        let both = node(serde_json::json!({"kloudlite.io/env": "true", "kloudlite.io/session": "true"}), "8", "33554432Ki");
        assert!(fits(Some(&both), &[pod("Running", "5", "8Gi")], want), "both labels: the workspace rule");
    }

    /// The owner is ALWAYS allowed: it holds the bytes by construction, and a rule that could
    /// refuse the owner is a rule that can strand a workspace with nowhere at all to start.
    #[test]
    fn the_owner_may_always_claim_even_with_no_replica_row() {
        assert!(may_claim("node-a", "node-a", &p(true, None, Some("stop-ws-1-3"))));
    }

    /// Another node needs the NAME, not the phase: this is the check that used to live in the
    /// flush gate, moved to where the decision is actually made.
    #[test]
    fn another_node_may_claim_only_when_it_holds_the_newest_transient() {
        let holding = Some(replica("node-b", "Synced", &[("ws-1", "stop-ws-1-3")]));
        let behind = Some(replica("node-b", "Synced", &[("ws-1", "sync-ws-1-old")]));
        assert!(may_claim("node-b", "node-a", &p(true, holding, Some("stop-ws-1-3"))));
        assert!(!may_claim("node-b", "node-a", &p(true, behind, Some("stop-ws-1-3"))));
        assert!(!may_claim("node-b", "node-a", &p(true, None, Some("stop-ws-1-3"))), "no row is not up to date");
    }

    /// No transient at all: a restore-to-new, or a worktree that never ran. A `Synced` replica
    /// holds every Ready snapshot, so plain `Synced` is the right bar — and the spec says so.
    #[test]
    fn with_no_transient_a_synced_replica_may_claim() {
        assert!(may_claim("node-b", "node-a", &p(true, Some(replica("node-b", "Synced", &[])), None)));
        assert!(!may_claim("node-b", "node-a", &p(true, Some(replica("node-b", "Syncing", &[])), None)));
    }

    /// Bootstrap is unchanged and is the reason `has_snapshots` survives: a volume nothing has ever
    /// snapshotted to is claimable by any node, because there are no bytes anywhere to be near.
    #[test]
    fn a_volume_with_no_snapshots_is_claimable_by_anyone() {
        assert!(may_claim("node-b", "node-a", &p(false, None, None)));
        assert!(may_claim("node-b", "", &p(false, None, None)), "and by anyone when nothing owns it yet");
    }

    /// Carried from Task 5's review: a clone of a source that had never been snapshotted read as
    /// bootstrap and was claimable ANYWHERE, on a node with none of the source's bytes. The clone
    /// cut `/v1` now takes is a `Snapshot` CR of the source volume, so `has_snapshots` is true from
    /// the moment the clone exists and the up-to-date rule applies to it like everything else.
    #[test]
    fn a_clone_of_a_never_snapshotted_source_places_only_where_its_cut_is_held() {
        let cut = Some("clone-ws-1-cafe");
        let holding = Some(replica("node-b", "Synced", &[("ws-1", "clone-ws-1-cafe")]));
        assert!(may_claim("node-b", "node-a", &p(true, holding, cut)));
        assert!(!may_claim("node-b", "node-a", &p(true, None, cut)), "the cut arms the guard: no row, no claim");
        assert!(
            !may_claim("node-b", "node-a", &p(true, Some(replica("node-b", "Synced", &[])), cut)),
            "and a Synced row that does not name the cut is not up to date for it"
        );
        assert!(may_claim("node-a", "node-a", &p(true, None, cut)), "the owner cut it, so the owner holds it");
    }

    /// The WORKING window, before the owner has taken the btrfs snapshot: the cut exists as a CR,
    /// so `has_snapshots` is true, but it is not Ready and therefore not `newest_transient` — no
    /// node's `branches` can name it. Placement in that window falls back to the source volume's
    /// previous transient, and the OWNER is the only node guaranteed to hold it.
    #[test]
    fn during_the_working_window_only_the_source_volumes_owner_may_claim_a_clone() {
        // `newest` is the PREVIOUS transient: the clone cut is not Ready yet, so it is not it.
        let prev = Some("sync-ws-1-old");
        assert!(may_claim("node-a", "node-a", &p(true, None, prev)), "the owner always; it holds the bytes");
        assert!(!may_claim("node-b", "node-a", &p(true, None, prev)), "no replica row is never up to date");
        // A peer that HAS the previous transient still qualifies — the up-to-date rule is the only
        // rule, and it is about bytes held, not about who cut what.
        assert!(may_claim("node-b", "node-a", &p(true, Some(replica("node-b", "Synced", &[("ws-1", "sync-ws-1-old")])), prev)));
    }

    /// A snapshot still being cut is one tick away, not wrong forever — the distinction that stopped
    /// every clone being settled `Permanent/NoSuchSnapshot` at birth, since `/v1` creates the cut
    /// microseconds before the clone object that names it.
    #[test]
    fn a_working_snapshot_is_pending_and_an_absent_one_is_not() {
        assert!(snapshot_pending(Some(crd::Phase::Working)));
        assert!(snapshot_pending(Some(crd::Phase::Pending)));
        assert!(snapshot_pending(Some(crd::Phase::Creating)));
        assert!(!snapshot_pending(None), "absent is forever: retention swept it, or it was never of this volume");
        assert!(!snapshot_pending(Some(crd::Phase::Error)), "a failed cut is not going to become Ready");
        assert!(!snapshot_pending(Some(crd::Phase::Ready)), "Ready is not pending; it is the destination");
    }

    /// A clone places over its SOURCE, whose volume name is also the source worktree's name —
    /// `source_nodes`' pin to the source's node is gone, so a clone of a released source can start
    /// on any node that is up to date for it.
    #[test]
    fn a_clone_places_over_its_source_worktree() {
        let storage = crd::WorkspaceStorage {
            quota_gb: 20,
            source: Some(crd::VolumeSource::CloneOf { volume: "ws-src".into(), commit: None }),
        };
        assert_eq!(clone_source(Some(&storage)), Some(("ws-src", None)));
        assert_eq!(clone_source(Some(&crd::WorkspaceStorage { quota_gb: 20, source: None })), None);
        assert_eq!(clone_source(None), None);
    }

    /// F6: a `SeededFrom` clone places over ONE named cut, not over "whatever is newest". The
    /// source's node is down, so the newest cut cluster-wide may be one the dead node made and
    /// nobody holds — placing on it would strand the clone exactly the way the shared-worktree
    /// path did.
    #[test]
    fn a_seeded_clone_places_over_the_one_cut_it_names() {
        let storage = crd::WorkspaceStorage {
            quota_gb: 20,
            source: Some(crd::VolumeSource::SeededFrom { volume: "ws-src".into(), snapshot: "sync-ws-src-bbbb".into() }),
        };
        assert_eq!(clone_source(Some(&storage)), Some(("ws-src", Some("sync-ws-src-bbbb"))));

        // The bar `placement` then hands `may_claim` is that exact cut.
        let cut = Some("sync-ws-src-bbbb");
        let holder = Some(replica("node-b", "Synced", &[("ws-1", "sync-ws-src-bbbb")]));
        assert!(may_claim("node-b", "node-a", &p(true, holder, cut)), "the node that holds the cut may seed from it");
        assert!(
            !may_claim("node-b", "node-a", &p(true, Some(replica("node-b", "Synced", &[("ws-1", "sync-ws-src-old")])), cut)),
            "a node holding some OTHER cut cannot seed this one: the bytes are not on its disk"
        );
        assert!(!may_claim("node-b", "node-a", &p(true, None, cut)), "and no replica row at all is never up to date");
        // The owner arm is reachable but not the case that matters here: an interrupted clone's
        // source volume is pinned to the DEAD node, so `decide` only gets past its owner guard
        // because that owner is unplaceable — and then this node is never the owner. Asserted so
        // the arm is not mistaken for the seeding rule.
        assert!(may_claim("node-a", "node-a", &p(true, None, cut)), "an owner holds its own volume's cuts");
    }
}
