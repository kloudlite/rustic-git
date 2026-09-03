//! Who may hold a volume, and who is alive enough to be asked. Pure arithmetic over a Node list
//! and a set of `VolumeReplica` rows, with one rule under all of it: a decision is made by NAME
//! (`up_to_date`), never by comparing clocks across nodes — which is what makes placement
//! skew-proof.

use crate::controller::Ctx;
use k8s_openapi::api::core::v1::Node;
use kube::api::{Api, ListParams};
use kube::ResourceExt;
use rustic_git_workspaces::{crd, replicate};
use std::sync::Arc;

/// The ordering key and the "newest transient" rule both live in `crd` now: `/v1` picks a clone
/// cut's parent with the same function this node's placement reads, and two copies of that key is
/// how two tiers disagree about which cut is newest.
pub(crate) use crd::newest_transient_of;

/// The pool-eligible nodes, `rustic-git.io/pool=true`, name-sorted so `replicate::targets`'
/// rendezvous scoring is deterministic across every node running this beat.
pub(crate) async fn pool_nodes(client: &kube::Client) -> Result<Vec<String>, String> {
    let api: kube::Api<Node> = kube::Api::all(client.clone());
    let lp = ListParams::default().labels("rustic-git.io/pool=true");
    let list = api.list(&lp).await.map_err(|e| e.to_string())?;
    let mut names: Vec<String> = list.items.into_iter().map(|n| n.name_any()).collect();
    names.sort();
    Ok(names)
}

/// `WS_NODE_DEAD_SECS`, default 180 — how long a node must be observed NotReady before its
/// `VolumeReplica` rows are reaped and its volumes swept. Long enough that a rolling restart or a
/// brief kubelet hiccup never costs a replica row; the row is cheap to recreate, a wrongly-reaped
/// one is not. It was 600 with a 180 deploy override, which meant the number every test and every
/// other doc comment saw was not the one production ran.
pub(crate) fn node_dead_secs() -> i64 {
    std::env::var("WS_NODE_DEAD_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(180)
}

/// The nodes a stopped parent could start on, from this node's own view — the pool minus the
/// unplaceable. Keep-biased: a listing error is an empty list, which wakes nobody and places
/// nothing, rather than a guess about who is alive.
pub(crate) async fn placeable_nodes(ctx: &Arc<Ctx>) -> Vec<String> {
    let (Ok(pool), Ok(nodes)) = (
        pool_nodes(&ctx.client).await,
        Api::<Node>::all(ctx.client.clone()).list(&ListParams::default()).await,
    ) else {
        return Vec::new();
    };
    live_nodes(&pool, &nodes.items, node_dead_secs(), k8s_openapi::jiff::Timestamp::now())
}

/// Rendezvous over the FULL pool keeps electing a corpse: the reaper deletes its row every beat
/// and no live node ever becomes a target, so a volume sits one copy short until the node comes
/// back. Placement therefore sees only nodes that pass `unplaceable` — dead, or decommissioning —
/// and a node with no Node object at all is dead, not unknown.
pub(crate) fn live_nodes(pool: &[String], nodes: &[Node], floor: i64, now: k8s_openapi::jiff::Timestamp) -> Vec<String> {
    pool.iter().filter(|n| !unplaceable(nodes.iter().find(|k| k.name_any() == n.as_str()), floor, now)).cloned().collect()
}

/// `targets()` counts the owner as one of `total` and hands back `total - 1` standbys. A dead or
/// decommissioning owner holds nothing anyone can reach, so it is not a copy: ask for one standby more.
pub(crate) fn standby_count(owner_alive: bool, replicas: u32) -> usize {
    replicas as usize + usize::from(!owner_alive)
}

/// THE placement bar, and the only one: a replica is up to date for a worktree when it HOLDS that
/// worktree's newest Ready transient, by name — never by comparing clocks, which a skew between
/// nodes could make an old copy look current. A worktree with no transient at all (never ran, or a
/// fresh restore) has nothing to name, so plain `Synced` is the right bar: a Synced replica holds
/// every Ready snapshot.
pub(crate) fn up_to_date(replica: &crd::VolumeReplica, worktree: &str, newest_transient: Option<&str>) -> bool {
    let Some(st) = replica.status.as_ref() else { return false };
    match newest_transient {
        None => st.phase == "Synced",
        Some(want) => st.branches.get(worktree).is_some_and(|held| held == want),
    }
}

/// Which of these replica rows are up to date for `worktree` — the candidate set a start or a
/// clone chooses among, the owner being added by the caller (it holds the bytes by construction).
pub(crate) fn up_to_date_nodes(worktree: &str, newest: Option<&str>, rows: &[crd::VolumeReplica]) -> Vec<String> {
    let mut out: Vec<String> = rows.iter().filter(|r| up_to_date(r, worktree, newest)).map(|r| r.spec.node.clone()).collect();
    out.sort();
    out
}

/// Rendezvous over the candidate set, keyed by the volume id — `replicate::targets`' own hash, so
/// the spread is deterministic and even by count and a retry lands on the same answer. Every node
/// computes the same result with no coordinator.
///
/// ponytail: by COUNT, not by load. Weighting by free CPU or pool space is the named upgrade and
/// needs an input every node computes identically — a per-node metric every agent can read the
/// same way, not one node's opinion.
pub(crate) fn preferred_node(volume: &str, candidates: &[String]) -> Option<String> {
    // `targets(volume, me = "", candidates, total = 2)` is "the top-scoring candidate", which is
    // the same ordering the replication spread already uses.
    replicate::targets(volume, "", candidates, 2).into_iter().next()
}

/// The newest Ready transient of `worktree` ANYWHERE IN THE CLUSTER — one field-selected list, for
/// a caller with no beat listing of its own. It deliberately ignores what this node holds: it is
/// the bar `up_to_date` compares a replica's `branches` against, so intersecting it with local
/// state would let a node behind on its pulls declare itself current. `snapshot::latest_transient`
/// is the local-hold variant, for a caller asking what it can actually check out right now.
pub(crate) async fn newest_transient(ctx: &Arc<Ctx>, volume: &str, worktree: &str) -> Result<Option<String>, kube::Error> {
    let api: Api<crd::Snapshot> = Api::all(ctx.client.clone());
    let list = api.list(&ListParams::default().fields(&format!("spec.volume={volume}"))).await?;
    Ok(newest_transient_of(&list.items, worktree))
}

/// Deletes any `VolumeReplica` whose `spec.node` has been observed NotReady for longer than
/// `WS_NODE_DEAD_SECS` — positive evidence only. A nodes-list error reaps nothing (the whole
/// listing, not per-row, since a partial list would make an actually-live node look absent). A
/// node absent from a POSITIVELY-listed set counts as dead; a node present with no readable
/// `Ready` condition history does not — the API server just hasn't reported one yet.
/// The one positive-evidence rule both dead-node sweeps below apply, factored out once so the
/// replica reaper and the claim-unclaim sweep can never drift apart: absent from a nodes list we
/// DID get is dead; present with `Ready=false` past `floor` seconds is dead; present with no
/// readable `Ready` condition at all is NOT dead — the API server just hasn't converged one yet.
pub(crate) fn node_is_dead(node: Option<&Node>, floor: i64, now: k8s_openapi::jiff::Timestamp) -> bool {
    match node {
        None => true,
        Some(n) => n
            .status
            .as_ref()
            .and_then(|s| s.conditions.as_ref())
            .and_then(|cs| cs.iter().find(|c| c.type_ == "Ready"))
            .is_some_and(|c| c.status != "True" && c.last_transition_time.as_ref().is_some_and(|t| now.as_second() - t.0.as_second() > floor)),
    }
}

/// Whether an operator has asked for this node to be retired. Exact value only — see the constant.
pub(crate) fn decommissioning(node: Option<&Node>) -> bool {
    node.and_then(|n| n.metadata.labels.as_ref()).and_then(|l| l.get(crd::DECOMMISSION_LABEL)).is_some_and(|v| v == "true")
}

/// "Not a place to run", the ONE predicate every placement decision uses. Dead (NotReady past the
/// floor, or absent from a listing we did get) and decommissioning are the same answer here: both
/// mean nothing new may land, and keeping them as two tests is how the rendezvous and the sweep
/// eventually disagree about whether a node still owns anything.
///
/// It is deliberately NOT `node_is_dead`, which stays the reaper's rule: a decommissioning node is
/// alive, keeps serving pulls, and its replica rows must not be reaped out from under a peer that
/// is mid-transfer from it.
pub(crate) fn unplaceable(node: Option<&Node>, floor: i64, now: k8s_openapi::jiff::Timestamp) -> bool {
    node_is_dead(node, floor, now) || decommissioning(node)
}

