//! Every sweep in the agent, and the one rule they all hold: keep-biased. A fresh read
//! immediately before any delete, a partial listing acts on nothing, and an unreadable answer
//! keeps rather than guesses. `volume_decision` + `sweep_volumes` are ONE function for both the
//! dead-node sweep and the drain on purpose (spec simplification 9) — two copies of those arms is
//! how a drain starts libelling a healthy workspace.

use super::placement::{node_is_dead, preferred_node, standby_count};
use super::pull::replica_interval;
use crate::controller::{replace_status, Ctx};
use crate::janitor;
use k8s_openapi::api::core::v1::Node;
use kube::api::{Api, ListParams};
use kube::ResourceExt;
use kloudlite_workspaces::{crd, replicate};
use std::collections::HashSet;
use std::sync::Arc;

// ponytail: `now` is THIS node's own clock against another node's `lastTransitionTime`, so
// the 180 s floor absorbs NTP drift rather than measuring it. At 600 s that was ample slack;
// at 180 it is three minutes of margin, which is still far more than a healthy fleet drifts.
// The upgrade is an apiserver-relative delta (compare against the API server's own clock via
// a `Lease` renewal, as the server tier's ownership lease already does) if drift ever shows up
// as a spurious sweep.
pub(crate) async fn reap_dead_replicas(ctx: &Arc<Ctx>, beat: &crate::listing::Beat, nodes: &[Node], floor: i64, now: k8s_openapi::jiff::Timestamp) {
    let replica_api: Api<crd::VolumeReplica> = Api::all(ctx.client.clone());
    for r in &beat.replicas {
        if node_is_dead(nodes.iter().find(|n| n.name_any() == r.spec.node), floor, now) {
            let rname = r.name_any();
            if let Err(e) = replica_api.delete(&rname, &Default::default()).await {
                tracing::warn!(name = %rname, reason = "dead-node", error = %e, "replica.delete.failed");
            }
        }
    }
}

/// What a sweep decides about one volume: `Mark` writes the condition and keeps the pin,
/// `Release` clears the pin and un-places every parent.
#[derive(Debug)]
pub(crate) enum VolumeVerdict {
    Mark { why: String },
    Release { why: String, reason: &'static str },
}

/// THE per-volume decision, for both sweeps. Ownership is per volume, so moving is decided per
/// volume — never per parent, which is exactly the bug this replaces: un-placing a stopped
/// workspace while a running clone of it kept the same volume pinned left the stopped one
/// claimable on a node that owns nothing.
///
/// The three arms, in the spec's order:
///   1. any parent Running        → nothing moves, pin kept, every parent marked;
///   2. some parent not replicated → nothing moves yet, pin kept — every parent must be
///      covered, or starting elsewhere loses that one's last edits;
///   3. otherwise                 → pin cleared, parents un-placed, an up-to-date node takes it.
///
/// `reason` is the condition reason the caller wants (`NodeDead` for the dead-node sweep,
/// `Decommissioned` for a drain) — the arms are identical, only the word differs.
pub(crate) fn volume_decision(
    volume: &str,
    owner: &str,
    parents: &[&crate::listing::Parent],
    reason: &'static str,
) -> VolumeVerdict {
    if let Some(running) = parents.iter().find(|p| p.is_live_worktree()) {
        return VolumeVerdict::Mark {
            why: format!(
                "owner {owner} is unavailable; a Running worktree ({}) still names volume {volume}, so it stays pinned",
                running.name
            ),
        };
    }
    // `replicated` is the OWNER's own `Replicated` condition off the listing, never recomputed
    // here: two nodes computing "is it replicated" independently is two truths that can disagree.
    let waiting: Vec<&str> = parents.iter().filter(|p| !p.replicated).map(|p| p.name.as_str()).collect();
    if !waiting.is_empty() {
        // Every one of them, not the first: an operator reading this needs to know which parents
        // are holding the volume, and the set shrinks one name at a time as replicas catch up.
        return VolumeVerdict::Mark {
            why: format!("owner {owner} is unavailable; waiting for a replica of: {}", waiting.join(", ")),
        };
    }
    VolumeVerdict::Release {
        why: format!("owner {owner} is unavailable; released, waiting for an up-to-date node to take it"),
        reason,
    }
}

/// Applies `volume_decision` to every volume whose owner is in `owners`. One place, called by the
/// dead-node sweep and by the decommission beat with different sets and different reasons — the
/// arms must never drift, and two copies of them is how they would.
///
/// `mark_running` is what separates the two callers' Mark arms. The dead sweep marks (true): the
/// node is gone, so `Unavailable`/`Degraded` is the literal truth and the only place the API can
/// say why nothing will start there. A drain does NOT (false): the node is alive and the workspace
/// is happily running, so writing `Degraded` would libel a healthy worktree — and `/v1`'s
/// `interrupted()` keys on exactly that condition, which would start 409ing clones of it. The
/// drain's `Decommissioning=True/NodeLeaving` on the parent already carries the whole message.
pub(crate) async fn sweep_volumes(
    ctx: &Arc<Ctx>,
    beat: &crate::listing::Beat,
    owners: &HashSet<String>,
    reason: &'static str,
    mark_running: bool,
) {
    let api: Api<crd::Volume> = Api::all(ctx.client.clone());
    for vol in beat.volumes.iter().cloned() {
        let owner = vol.spec.node_name.clone();
        let name = vol.name_any();
        // `all_parents`, not `parents`: this volume is owned by another node, so this node's own
        // scoped list would show none of its parents and every arm would read as "nothing on it".
        let parents: Vec<&crate::listing::Parent> = beat.all_parents.iter().filter(|p| p.volume == name).collect();
        // An EMPTY pin with parents still placed ON AN UNPLACEABLE NODE is the crash window between
        // the release CAS and the un-place: no watch matches such a parent (`status.nodeName` is
        // neither this node nor empty), and the heal for an unowned volume — `resolve_volume`'s
        // `spec.node_name.is_empty()` → `take_volume` branch — runs only on the node the parent
        // names, the one that is gone. The sweep is the only thing that can see it, so it finishes
        // the release rather than skipping the volume for having no owner. Nothing is re-patched:
        // the pin is already clear. A parent on a LIVE node is deliberately not this case — that is
        // the spread's crash window, and its own node's `take_volume` picks the pin back up — and a
        // parent here cannot be running: the CAS that cleared the pin only ever ran on a volume
        // with nothing running on it. That is also why `stranded` bypasses `volume_decision`
        // entirely, so a drain (`mark_running: false`) finishes an interrupted release too: the
        // only thing it un-places is a stopped parent on a node nothing can start on.
        let stranded = owner.is_empty() && parents.iter().any(|p| owners.contains(&p.node_name));
        if !stranded && (owner.is_empty() || !owners.contains(&owner)) {
            continue;
        }
        // The reason comes back OUT of the verdict, so the word written is the one the decision
        // made rather than a second copy of the caller's argument.
        let (why, reason, release) = if stranded {
            (format!("volume {name} has no owner; finishing an interrupted release"), reason, true)
        } else {
            match volume_decision(&name, &owner, &parents, reason) {
                VolumeVerdict::Mark { .. } if !mark_running => continue,
                VolumeVerdict::Mark { why } => (why, reason, false),
                VolumeVerdict::Release { why, reason } => (why, reason, true),
            }
        };
        let mut cur = vol;
        // A stranded volume is already released — its verdict is the un-place below, and there is
        // no pin left to compare-and-set.
        if release && !stranded {
            // The pin FIRST, before anything is un-placed: a failed CAS with parents already
            // cleared would leave them claimable on a node that does not own the volume — the
            // exact bug this whole function exists to make impossible.
            //
            // `test` proves the owner hadn't already moved (a survivor's takeover landing between
            // our list and this patch), THEN `replace` clears it; a failed test (409/422) means we
            // lost that race, so nothing at all is written this beat.
            match crate::controller::volume::cas(&api, &name, "/spec/nodeName", serde_json::json!(owner), serde_json::json!("")).await {
                // The patched object, not our stale copy: the PUT below carries a
                // `resourceVersion`, and the patch just bumped it.
                Ok(Some(v)) => cur = v,
                Ok(None) => continue, // a survivor's takeover landed between our list and this patch
                Err(e) => {
                    tracing::warn!(volume = %name, error = %e, "volume.release.failed");
                    continue;
                }
            }
        }
        let prev = cur.status.clone().unwrap_or_default();
        let idle = prev.phase == crd::Phase::Unavailable
            && !release
            && prev.conditions.iter().any(|c| c.type_ == "Available" && c.reason == reason && c.message == why);
        if !idle {
            // The same re-read-on-409 loop `mark_parent_of` and `write_replica_status` use, and for
            // the same reason: this is a PUT carrying `resourceVersion`, and a lost race used to
            // just warn — leaving the volume `Available=True` for a dead owner until something else
            // happened to touch it.
            //
            // THREE attempts is enough only because the owner is no longer writing back: the
            // parent and volume reconcilers bail on `my_node` (see `controller::my_node`), so a
            // partitioned agent no longer rewrites this status every 15 s. Against that, no bound
            // would have been enough; against one-shot writers, three is plenty.
            for attempt in 0..3 {
                let mut st = cur.status.clone().unwrap_or_default();
                st.phase = crd::Phase::Unavailable;
                let gen = cur.metadata.generation.unwrap_or(0);
                // No `Released` reason: `Unavailable` with an empty pin IS released, and a third
                // word would restate the pin the object already carries.
                st.conditions = vec![crd::condition("Available", false, reason, &why, gen)];
                match replace_status(&api, &cur, "Volume", serde_json::to_value(st).expect("VolumeStatus serializes")).await {
                    Ok(()) => break,
                    Err(kube::Error::Api(s)) if s.code == 409 && attempt < 2 => match api.get(&name).await {
                        Ok(fresh) => cur = fresh,
                        Err(e) => {
                            tracing::warn!(volume = %name, reason = "re-read", error = %e, "volume.mark.failed");
                            break;
                        }
                    },
                    Err(e) => {
                        tracing::warn!(volume = %name, reason = "mark", error = %e, "volume.mark.failed");
                        break;
                    }
                }
            }
        }
        // Every parent on the volume carries the condition, whatever the verdict — that is how the
        // API answers "why will this not start". Last, because on a release the pin is already
        // clear: an un-placed parent is only safe once nothing owns the volume.
        //
        // `Degraded=True` only where something actually failed — the dead-node sweep, whose owner
        // really is gone. A drain (`mark_running: false`) only ever reaches here on a RELEASE, of a
        // volume whose every parent is stopped and replicated: nothing about that is degraded, and
        // writing the word would paint a healthy workspace red in the API and the web for a routine
        // retirement. `Placed=False` is the condition the claim itself owns, so the next node's
        // claim overwrites it — exactly as a spread's `Moving` does.
        let cond = if mark_running { ("Degraded", true) } else { ("Placed", false) };
        for p in &parents {
            mark_parent(ctx, p, cond, reason, &why, release).await;
        }
    }
}

/// Clear one parent's claim so the node the volume was just handed to can take it. The same write
/// the sweep's release arm makes, for the same reason — a parent left pointing at the old owner
/// would never be looked at by the new one.
/// `Placed=False`, not `Degraded`: a spread is a routine start-time decision, and writing the word
/// the dead-node sweep writes would make every healthy move look like a failure in the API and the
/// web. `Placed` is the condition the claim itself sets, so the next node's claim overwrites it.
pub(crate) async fn unplace_parent(ctx: &Arc<Ctx>, p: &crate::listing::Parent) {
    mark_parent(ctx, p, ("Placed", false), "Moving", "released so an up-to-date node can start it", true).await;
}

/// One parent's status write for the sweep: the condition always, `nodeName: ""` only on a
/// release. The same guarded primitive the claim uses (`replace_status`, a PUT carrying
/// `resourceVersion`, one re-read on a 409) — clearing a claim races the same way winning one does.
/// `cond` is the condition TYPE and its truth: the sweep says `Degraded=True`, a spread says
/// `Placed=False`. Both the idle check and `replaced` key by type, so one type never disturbs the
/// other's condition.
pub(crate) async fn mark_parent(ctx: &Arc<Ctx>, p: &crate::listing::Parent, cond: (&'static str, bool), reason: &str, why: &str, release: bool) {
    match p.kind {
        "Workspace" => mark_parent_of::<crd::Workspace>(ctx, &p.name, "Workspace", cond, reason, why, release).await,
        _ => mark_parent_of::<crd::Environment>(ctx, &p.name, "Environment", cond, reason, why, release).await,
    }
}

/// The generic half. Status is edited as JSON because `Workspace` and `Environment` share no
/// status type — the same reason `listing::Parent` exists at all.
#[allow(clippy::too_many_arguments)]
async fn mark_parent_of<K>(ctx: &Arc<Ctx>, name: &str, kind: &'static str, (cond_type, cond_status): (&'static str, bool), reason: &str, why: &str, release: bool)
where
    K: kube::Resource<DynamicType = ()> + Clone + serde::Serialize + serde::de::DeserializeOwned + std::fmt::Debug,
{
    let api: Api<K> = Api::all(ctx.client.clone());
    let mut cur = match api.get_opt(name).await {
        Ok(Some(o)) => o,
        Ok(None) => return, // deleted between the listing and now: nothing to mark
        Err(e) => {
            tracing::warn!(%kind, %name, reason = "read", error = %e, "parent.mark.failed");
            return;
        }
    };
    for attempt in 0..2 {
        let mut status = serde_json::to_value(&cur).unwrap_or_default()["status"].take();
        if status.is_null() {
            status = serde_json::json!({});
        }
        let gen = cur.meta().generation.unwrap_or(0);
        let prev: Vec<crd::Condition> = serde_json::from_value(status["conditions"].clone()).unwrap_or_default();
        let cond = crd::condition_since(prev.iter().find(|c| c.type_ == cond_type), cond_type, cond_status, reason, why, gen);
        // Idle when nothing changed: this runs on every beat of every node, and rewriting an
        // identical status per volume forever is churn the API server pays for.
        if !release && prev.iter().any(|c| c.type_ == cond_type && c.reason == cond.reason && c.message == cond.message) {
            return;
        }
        // Replaced by type, not `kept_conditions`: `Replicated` is what the next beat's second arm
        // reads, and dropping it here would make the volume look unreplicated forever.
        status["conditions"] =
            serde_json::to_value(crate::controller::replaced(&prev, cond)).expect("conditions serialize");
        if release {
            status["nodeName"] = serde_json::json!("");
        }
        match replace_status(&api, &cur, kind, status).await {
            Ok(()) => return,
            Err(kube::Error::Api(s)) if s.code == 409 && attempt == 0 => match api.get(name).await {
                Ok(fresh) => cur = fresh,
                Err(e) => {
                    tracing::warn!(%kind, %name, reason = "re-read", error = %e, "parent.mark.failed");
                    return;
                }
            },
            Err(e) => {
                tracing::warn!(%kind, %name, reason = "mark", error = %e, "parent.mark.failed");
                return;
            }
        }
    }
}

/// The dead half: the set of owners that are dead, handed to `sweep_volumes`. The parents come
/// from the beat's own listing, which is why the per-kind list-and-decide plumbing (`unclaim_kind`,
/// its `releasable` closures, and the `running_volumes` set threaded between it and the release
/// pass) is gone — the listing already knows every parent on every volume.
///
/// `node_is_dead`, deliberately NOT `unplaceable`: a decommissioning node is alive and its running
/// work keeps running. Task 11's decommission beat calls `sweep_volumes` itself, with its own set.
///
/// ponytail: every live node computes the same dead set and runs this same sweep. Only one wins
/// the release CAS and `mark_parent_of`'s idle check absorbs the duplicate marks, so it is
/// correct — it just pays `N ×` the parent GETs and status writes. The upgrade is a rendezvous
/// over `live` keyed by volume id (`preferred_node`, already in this file), not a lease; take it
/// if the dead-node write volume ever shows up in an API server's audit log.
pub(crate) async fn sweep_dead_nodes(
    ctx: &Arc<Ctx>,
    beat: &crate::listing::Beat,
    nodes: &[Node],
    floor: i64,
    now: k8s_openapi::jiff::Timestamp,
) {
    let dead: HashSet<String> = beat
        .volumes
        .iter()
        .map(|v| v.spec.node_name.clone())
        .filter(|n| !n.is_empty() && node_is_dead(nodes.iter().find(|k| k.name_any() == *n), floor, now))
        .collect();
    sweep_volumes(ctx, beat, &dead, "NodeDead", true).await;
}

/// A copy whose rendezvous slot moved (a node joined, or a dead one came back) is not just
/// wasted disk: its stale Synced row still wins claims and satisfies stop's flush gate with
/// data that is no longer being pulled. It goes only once every CURRENT target is Synced, so a
/// spread never passes through a moment with fewer live copies than before. An unowned volume
/// is a dead node's mid-takeover: keep everything until someone owns it again. An EMPTY target
/// list is not "every target is synced" — it's this node itself missing from `live` (its own
/// Node object flapped NotReady while the agent kept running); `all()` is vacuously true on an
/// empty iterator, which would otherwise retire every copy on this node in one beat.
pub(crate) fn should_retire(me: &str, owner: &str, targets: &[String], hosted: bool, synced: &HashSet<String>) -> bool {
    !owner.is_empty()
        && owner != me
        && !hosted
        && !targets.is_empty()
        && !targets.iter().any(|t| t == me)
        && targets.iter().all(|t| synced.contains(t))
}

/// Directories under `{pool}/vol` that no listed Volume names. Files beside them (`{id}.owner`,
/// `{id}.lock`) are not volumes and are cleaned with their directory by `cleanup_local`.
pub(crate) fn orphan_voldirs(vol_root: &std::path::Path, known: &HashSet<String>) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(vol_root) else { return Vec::new() };
    let mut out: Vec<String> = rd
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| !known.contains(n))
        .collect();
    out.sort();
    out
}

/// `Snapshot` CRs whose `spec.volume` names no Volume at all. Every snapshot minted today carries
/// an ownerReference — to its parent (`api.rs`, `sync.rs`) or, since `migrate_and_seed_baseline`
/// gained one, to its Volume — so Kubernetes GC is the real answer; this sweep is for the records
/// already out there (13 on the cluster), and the backstop for any future path that forgets one.
///
/// Keep-biased twice over. `known` comes from the beat's Volume list, which is the only reason this
/// pass runs at all — a failed one bails before here — but that list and this one are separate
/// round trips, so a Volume created between them looks absent while its brand-new baseline does
/// not. One fresh GET per candidate, right before the delete, closes that window, exactly as the
/// stale-worktree drop below does; a failed GET keeps the snapshot. An unlistable snapshot set
/// deletes nothing — `retire_pass` makes that ONE listing and does not call this at all when it
/// fails.
///
/// Every node runs this and no node owns it: the race is three DELETEs for one object, of which two
/// answer 404, which this already tolerates. Electing one node (rendezvous over `live`) would buy
/// nothing an idempotent delete does not already give.
async fn sweep_orphan_snapshots(ctx: &Arc<Ctx>, known: &HashSet<String>, snapshots: &[crd::Snapshot]) {
    let api = Api::<crd::Snapshot>::all(ctx.client.clone());
    for s in snapshots.iter().filter(|s| !known.contains(&s.spec.volume)) {
        if !matches!(Api::<crd::Volume>::all(ctx.client.clone()).get_opt(&s.spec.volume).await, Ok(None)) {
            continue;
        }
        let name = s.name_any();
        match api.delete(&name, &Default::default()).await {
            Ok(_) => tracing::info!(volume = %s.spec.volume, snapshot = %name, reason = "no-volume-cr", "snapshot.dropped"),
            Err(e) if matches!(&e, kube::Error::Api(st) if st.code == 404) => {}
            Err(e) => tracing::warn!(snapshot = %name, reason = "no-volume-cr", error = %e, "snapshot.drop.failed"),
        }
    }
}

/// The names under `snap/` that no `Snapshot` record claims. Pure so the keep rules are testable
/// without btrfs: a record in ANY phase keeps its directory — a `Working` cut is a receive in
/// flight, and deleting under it loses the bytes it is still writing.
pub(crate) fn orphan_snaps(local: &[String], records: &HashSet<String>) -> Vec<String> {
    local.iter().filter(|n| !records.contains(*n)).cloned().collect()
}

/// The BYTE half of "an explicit delete is the only way a snapshot dies": a `snap/<name>`
/// subvolume whose record is gone has nothing left that could ever check it out, and the pull
/// beat's own retire (`retired`) only visits volumes this node is still pulling — a pinned
/// snapshot's volume outlives its workspace and is not one of them.
///
/// Keep-biased throughout: only volumes whose bytes are actually here (a voldir), never one
/// mid-delete, a per-volume listing error skips that volume rather than guessing it empty, and a
/// fresh GET per candidate closes the window between the beat's listing and this delete.
/// Returns what it DECIDED to drop — the decision, not the btrfs outcome, is what a test on a
/// machine without btrfs can read, and a failed delete is retried by the next beat anyway.
///
/// ponytail: one full snap listing per held volume per beat; index records by name if a volume
/// ever grows past thousands of snapshots.
pub(crate) async fn sweep_orphan_snap_bytes(ctx: &Arc<Ctx>, beat: &crate::listing::Beat, snapshots: &[crd::Snapshot]) -> Vec<(String, String)> {
    let api = Api::<crd::Snapshot>::all(ctx.client.clone());
    let mut dropped = Vec::new();
    for v in &beat.volumes {
        let id = v.name_any();
        // A volume being deleted takes its whole voldir with it (`cleanup_local`); racing that
        // with per-snapshot deletes buys nothing.
        if v.metadata.deletion_timestamp.is_some() || !ctx.engine.pool.voldir(&id).exists() {
            continue;
        }
        let local = match ctx.engine.local_snapshots(&id) {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(kind = "Snapshot", volume = %id, reason = "local", error = %e, "listing.failed");
                continue;
            }
        };
        let records: HashSet<String> = snapshots.iter().filter(|s| s.spec.volume == id).map(|s| s.name_any()).collect();
        for name in orphan_snaps(&local, &records) {
            // The list is already stale by the time we get here — the record sweep makes a GET per
            // candidate between it and this loop, and a push started in that window has its CR
            // Ready and its bytes on disk. One fresh GET per candidate, exactly as the record
            // sweep does; a present record OR a failed GET keeps the bytes.
            if !matches!(api.get_opt(&name).await, Ok(None)) {
                continue;
            }
            tracing::info!(volume = %id, snapshot = %name, reason = "no-record", "snapshot.dropped");
            // btrfs delete takes a blocking flock and shells out — never on the reactor thread.
            let (engine, vol, cname) = (ctx.engine.clone(), id.clone(), name.clone());
            match tokio::task::spawn_blocking(move || engine.drop_snapshot(&vol, &cname)).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => tracing::warn!(volume = %id, snapshot = %name, reason = "no-record", error = %e, "snapshot.drop.failed"),
                Err(e) => tracing::warn!(volume = %id, snapshot = %name, reason = "panicked", error = %e, "snapshot.drop.failed"),
            }
            dropped.push((id.clone(), name));
        }
    }
    dropped
}

/// Drops this node's copy of any volume whose rendezvous slot over `live` no longer names it —
/// see `should_retire`. Runs at the end of `pull_beat_with`, after the pull loop, so a new
/// target's pull lands before anyone retires the copy it just replaced.
pub(crate) async fn retire_pass(ctx: &Arc<Ctx>, beat: &crate::listing::Beat, live: &[String]) {
    let vols = &beat.volumes;
    let rows = &beat.replicas;
    let hosted = beat.hosted_volumes();
    // Off the beat's own parent listing — `Parent::replicated` is the owner's `Replicated`
    // condition as written, never recomputed here (see the field's doc), so the gauge and the UI
    // give one answer. Distinct volumes, because two parents on one volume are one backlog item.
    let backlog: HashSet<&str> =
        beat.parents.iter().filter(|p| !p.replicated).map(|p| p.volume.as_str()).collect();
    metrics::gauge!("replication_backlog").set(backlog.len() as f64);
    // A local voldir with no Volume CR at all is an orphan: nothing lists it, so no pull, no
    // retire and no worktree drop ever visits it again. The Volume is always created before any
    // node makes its directory (the parent's reconciler creates the CR, the pull beat only pulls
    // listed volumes), so "no CR" is never "not yet", only "gone". A CR mid-deletion still counts
    // as present — garbage collection finishes on its own and the next beat sees it absent.
    let known: HashSet<String> = vols.iter().map(|v| v.name_any()).collect();
    for id in orphan_voldirs(&ctx.engine.pool.root.join("vol"), &known) {
        tracing::info!(volume = %id, reason = "no-volume-cr", "volume.dropped");
        // A voldir walk plus one `btrfs subvolume delete` per subvolume under it: seconds to
        // minutes of a thread, and this beat shares its reactor with every reconcile and every
        // peer send on the node. Same rule `sweep_orphan_snap_bytes` follows two functions up.
        let (engine, vol) = (ctx.engine.clone(), id.clone());
        if let Err(e) = tokio::task::spawn_blocking(move || janitor::cleanup_local(&engine, &vol)).await {
            tracing::warn!(volume = %id, reason = "panicked", error = %e, "volume.drop.failed");
        }
    }
    // The row half of the same orphan. `retire_pass` only ever visits LISTED volumes, so a
    // `VolumeReplica` whose Volume is gone was never revisited by anything: it outlived the
    // workspace, and its stale `Synced` still satisfies a stop's flush gate and wins claims.
    // ponytail: a sweep, not an ownerReference on the row — the sweep has to exist anyway for the
    // rows already out there, and `write_replica_status` has no Volume UID to hand without a GET
    // per row it creates. Stamp the ownerReference at creation if row garbage ever outgrows this.
    for r in beat.replicas.iter().filter(|r| r.spec.node == ctx.node && !known.contains(&r.spec.volume)) {
        let rname = r.name_any();
        tracing::info!(volume = %r.spec.volume, name = %rname, reason = "no-volume-cr", "replica.deleted");
        if let Err(e) = Api::<crd::VolumeReplica>::all(ctx.client.clone()).delete(&rname, &Default::default()).await {
            if !matches!(&e, kube::Error::Api(s) if s.code == 404) {
                tracing::warn!(name = %rname, reason = "no-volume-cr", error = %e, "replica.delete.failed");
            }
        }
    }
    // ONE Snapshot listing for both record-side and byte-side sweeps: each is cluster-wide and
    // neither may act on a partial view, so a failure skips both rather than deleting on absence.
    match Api::<crd::Snapshot>::all(ctx.client.clone()).list(&ListParams::default()).await {
        Ok(l) => {
            sweep_orphan_snapshots(ctx, &known, &l.items).await;
            sweep_orphan_snap_bytes(ctx, beat, &l.items).await;
            collect_unreferenced_volumes(ctx, beat, &l.items, live).await;
        }
        Err(e) => tracing::warn!(kind = "Snapshot", error = %e, "listing.failed"),
    }
    // Same batching as `interesting_volumes`: one hop off the reactor for every probe this loop
    // needs instead of a `stat` per volume on it.
    let ids: Vec<String> = vols.iter().map(|v| v.name_any()).collect();
    let engine = ctx.engine.clone();
    let held: HashSet<String> = tokio::task::spawn_blocking(move || {
        ids.into_iter().filter(|id| engine.pool.voldir(id).exists()).collect::<HashSet<String>>()
    })
    .await
    .unwrap_or_default();
    for v in vols {
        let id = v.name_any();
        if v.metadata.deletion_timestamp.is_some() || !held.contains(&id) {
            continue;
        }
        let owner_alive = live.iter().any(|n| n == &v.spec.node_name);
        let targets = replicate::targets(&id, &v.spec.node_name, live, standby_count(owner_alive, v.spec.replicas));
        let synced: HashSet<String> = rows
            .iter()
            .filter(|r| r.spec.volume == id && r.status.as_ref().is_some_and(|s| s.phase == "Synced"))
            .map(|r| r.spec.node.clone())
            .collect();
        if !should_retire(&ctx.node, &v.spec.node_name, &targets, hosted.contains(&id), &synced) {
            // Still a target/replica, just not the owner: a `live/{ws}` worktree under it
            // belongs only to the owner and is what a takeover away from this node left behind
            // — UNLESS this node is `hosted` (serving a pod from it right now): the owner record
            // can lag a pod that's actually running here, and deleting a live worktree out from
            // under a running pod is the one thing this pass must never do.
            if !hosted.contains(&id) {
                // `v.spec.node_name` is from `beat.volumes`, listed before the pull loop ran; a
                // takeover landing in that window makes it stale, and against a stale owner this
                // would delete the worktree this node just created for itself. One fresh GET,
                // right before the delete, catches that race; a failed GET keeps everything.
                // Keep-bias: a failed GET, like `mine`, skips the drop rather than risking one
                // against a node name that may already be stale.
                if let Ok(Some(fresh)) = Api::<crd::Volume>::all(ctx.client.clone()).get_opt(&id).await {
                    if fresh.spec.node_name != ctx.node {
                        let (engine, vol, owner, me) =
                            (ctx.engine.clone(), id.clone(), v.spec.node_name.clone(), ctx.node.clone());
                        match tokio::task::spawn_blocking(move || janitor::drop_stale_worktrees(&engine, &vol, &owner, &me)).await {
                            Ok(dropped) if dropped > 0 => {
                                tracing::info!(volume = %id, count = dropped, "worktree.dropped")
                            }
                            Ok(_) => {}
                            Err(e) => tracing::warn!(volume = %id, reason = "panicked", error = %e, "worktree.drop.failed"),
                        }
                    }
                }
            }
            continue;
        }
        let rname = crd::replica_name(&id, &ctx.node);
        if let Err(e) = Api::<crd::VolumeReplica>::all(ctx.client.clone()).delete(&rname, &Default::default()).await {
            if !matches!(&e, kube::Error::Api(s) if s.code == 404) {
                tracing::warn!(volume = %id, reason = "keeping-copy", error = %e, "replica.delete.failed");
                continue; // row first, copy second: a copy without a row is harmless, a row without a copy is a lie
            }
        }
        let (engine, vol) = (ctx.engine.clone(), id.clone());
        if let Err(e) = tokio::task::spawn_blocking(move || janitor::cleanup_local(&engine, &vol)).await {
            tracing::warn!(volume = %id, reason = "panicked", error = %e, "volume.drop.failed");
            continue;
        }
        tracing::info!(volume = %id, reason = "slot-moved", "volume.dropped");
    }
}

/// Design rule 1/5's crash-between-steps safety net: `/v1` deletes a Snapshot then the Volume it
/// leaves reference-less, and a crash between those two steps strands the Volume with no snapshot
/// and no owner entry to ever collect it (Kubernetes GC only fires from an ownerReference, and the
/// finalizer already removed the last one on detach). This is the only other path that deletes a
/// Volume, and it is keep-biased like every sweep beside it: no owner entry, no `beat.parents` entry
/// (even an unfinalized one — the finalizer racing this pass is not evidence of anything, only a
/// gone one is), and no live snapshot (`is_snapshot()`, any phase but `Error` — the same predicate
/// `cleanup_parent`'s detach uses, so the two paths never disagree about "does a snapshot remain").
/// `WS_REPLICA_SECS` as the age floor keeps a Volume just created (its owner entry not yet visible
/// in this beat's stale listing) from being collected out from under its own creator.
///
/// Runs on the SAME Snapshot listing `sweep_orphan_snapshots`/`sweep_orphan_snap_bytes` already
/// made — a failed list skips this too, never "there are no snapshots".
///
/// One deleter: the Volume's pinned node (`spec.node_name`), same as every other per-volume
/// decision here. A released pin (`node_name` empty — `Volume.spec.node_name` is cleared, never
/// absent) has no owner left to be it, so the rendezvous top candidate over `live` stands in, the
/// same substitute `preferred_node` already is for "who takes this volume next".
async fn collect_unreferenced_volumes(ctx: &Arc<Ctx>, beat: &crate::listing::Beat, snapshots: &[crd::Snapshot], live: &[String]) {
    let floor = replica_interval(&ctx.settings).as_secs() as i64;
    let now = k8s_openapi::jiff::Timestamp::now();
    let hosted = beat.hosted_volumes();
    let snapshotted: HashSet<&str> = snapshots
        .iter()
        .filter(|s| s.is_snapshot() && s.status.as_ref().is_none_or(|st| st.phase != crd::Phase::Error))
        .map(|s| s.spec.volume.as_str())
        .collect();
    for v in &beat.volumes {
        let id = v.name_any();
        if v.metadata.deletion_timestamp.is_some() {
            continue; // already going; the delete below would only race its own finalizer
        }
        let has_owner = v.metadata.owner_references.as_ref().is_some_and(|refs| !refs.is_empty());
        if has_owner || hosted.contains(&id) || snapshotted.contains(id.as_str()) {
            continue;
        }
        let old_enough = v
            .metadata
            .creation_timestamp
            .as_ref()
            .is_some_and(|t| now.as_second() - t.0.as_second() > floor);
        if !old_enough {
            continue;
        }
        let deleter = if v.spec.node_name.is_empty() { preferred_node(&id, live) } else { Some(v.spec.node_name.clone()) };
        if deleter.as_deref() != Some(ctx.node.as_str()) {
            continue;
        }
        match Api::<crd::Volume>::all(ctx.client.clone()).delete(&id, &Default::default()).await {
            Ok(_) => tracing::info!(volume = %id, reason = "unreferenced", "volume.collected"),
            Err(e) if matches!(&e, kube::Error::Api(s) if s.code == 404) => {}
            Err(e) => tracing::warn!(volume = %id, error = %e, "volume.collect.failed"),
        }
    }
}

