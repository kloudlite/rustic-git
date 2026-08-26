//! The background lanes: lease renewal/checkpointing and the three backstop sweeps
//! (marker reconciliation, mergeability checks, stranded-merge re-announcement).

use crate::App;
use std::sync::Arc;

/// Renewal, and pruning on the leader — the two background halves of the lifecycle invariant.
/// The work itself lives on `App`; these are only the clocks.
pub fn spawn_lease_tasks(app: Arc<App>) {
    use crate::ownership::{LEASE_TTL, RENEW_EVERY};
    /// How often the leader moves the ownership map's flush pointer. Matched to the collector's
    /// `min_age` so the WAL settles at about two of these rather than growing without bound.
    const CHECKPOINT_EVERY: std::time::Duration = std::time::Duration::from_secs(300);
    /// Ceiling on one checkpoint. Generous for the work (a healthy one takes ~14ms) and short
    /// against the lease TTL it must never eat into.
    const CHECKPOINT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

    // Renewal runs ALONE. It used to share this loop with the reconcile/check/announce lanes,
    // and each lane sleeps RECONCILE_GAP per warm repo — at max_warm that is longer than
    // LEASE_TTL, so a node with enough warm repos skipped renewals and then evicted its own
    // live databases when the leader dropped them. The checkpoint got a deadline for exactly
    // this failure mode; the lanes get their own tasks below, so nothing can delay a beat.
    let a = app.clone();
    tokio::spawn(async move {
        let mut last_checkpoint = std::time::Instant::now();
        loop {
            tokio::time::sleep(RENEW_EVERY).await;
            // A renewal that cannot reach the leader is not fatal: the lease runs to its TTL and
            // the next beat is three seconds away. Missing every beat for a whole TTL is what lets
            // another node claim, which is the intended outcome.
            if let Err(e) = a.renew_once().await {
                tracing::warn!(error = %e, "renewing leases");
            }
            // Move the ownership map's flush pointer so the WAL behind it can be reclaimed.
            // Timed off the CLOCK, and BOUNDED: an unbounded flush hung here once and the leader
            // stopped renewing leases entirely. Missing a checkpoint costs a few hundred
            // reclaimable objects; missing every renewal costs the fleet its routing.
            if last_checkpoint.elapsed() >= CHECKPOINT_EVERY {
                last_checkpoint = std::time::Instant::now();
                match tokio::time::timeout(CHECKPOINT_TIMEOUT, a.ownership.checkpoint()).await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => tracing::warn!(error = %e, "ownership checkpoint"),
                    Err(_) => tracing::warn!(
                        timeout_s = CHECKPOINT_TIMEOUT.as_secs(),
                        "ownership checkpoint: timed out; leases keep renewing"
                    ),
                }
            }
        }
    });

    // The three backstop lanes, one task each so a slow pass delays only its own next pass —
    // a lane is a sequential loop and cannot overlap itself. Periods match what the old beat
    // arithmetic produced (10th/20th/5th beat at RENEW_EVERY = 3s); the per-lane rationale
    // lives on each lane's function below.
    let a = app.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            // 30s + 200ms/repo drift ceiling — see `reconcile_owned_markers`.
            reconcile_owned_markers(&a).await;
        }
    });
    let a = app.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            // 60s + 200ms/repo drift ceiling — see `check_owned_pulls`.
            check_owned_pulls(&a).await;
        }
    });
    let a = app.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(15)).await;
            // 15s + 200ms/repo drift ceiling — see `announce_stranded_merges`.
            announce_stranded_merges(&a).await;
        }
    });

    if !app.is_leader() {
        return;
    }
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(LEASE_TTL).await;
            if let Err(e) = app.prune_once().await {
                tracing::warn!(error = %e, "pruning ownership");
            }
        }
    });
}

/// One pass of the visibility repair lane: for every repo/image this node holds open, move
/// its listing marker back onto what the repo's own database says. `open_repo`'s lazy repair
/// only fires when someone touches a repo; a repo nobody clones or browses — and every
/// pre-existing repo, which has no marker at all until the structural sweep writes a
/// fail-closed PRIVATE one — would otherwise stay missing from listings forever.
///
/// `warm_repos()` is the ownership set on purpose: it names only databases THIS node has
/// open, so the lane can never open a repo owned elsewhere and fence its owner. Repairs are
/// paced by `RECONCILE_GAP` for the same reason the gc sweep paces its owners — this is a
/// backstop, and it must not compete with request traffic for object-store bandwidth.
/// Log-and-continue per repo: a marker is a view, not authorization, so one unreadable repo
/// is not a reason to leave the rest drifting.
pub async fn reconcile_owned_markers(app: &App) {
    for key in app.store.pool.warm_repos() {
        let (kind, rest) = match key.strip_prefix("img/") {
            Some(rest) => (crate::index::Kind::Img, rest),
            None => (crate::index::Kind::Repo, key.as_str()),
        };
        let Some((owner, name)) = rest.split_once('/') else { continue };
        let db_public = match kind {
            crate::index::Kind::Repo => app.store.is_public(owner, name).await,
            crate::index::Kind::Img => {
                use crate::registry::store::ImageExt;
                app.store.image_is_public(owner, name).await
            }
        };
        let db_public = match db_public {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(owner = %owner, repo = %name, error = %e, "reconcile marker");
                continue;
            }
        };
        if let Err(e) = app.store.reconcile_marker(owner, name, kind, db_public).await {
            tracing::warn!(owner = %owner, repo = %name, error = %e, "reconcile marker");
        }
        tokio::time::sleep(rustic_git_app::RECONCILE_GAP).await;
    }
}

/// Recompute mergeability for the open changes in every repo this node has warm.
///
/// THE SAFETY FLOOR for merge work, and it needs no Redis and no Mongo — which is the whole
/// reason discovery moved here. A repo's changes live in its own database, and opening that on
/// any other node fences this one, so the owner is the only party that may go looking. A lost
/// stream event now costs latency, never a check.
///
/// Warm repos only, exactly like `reconcile_owned_markers`: a repo nobody has opened has no
/// reader waiting on the answer either. A repo whose Mongo changes have not been migrated yet
/// has an empty `pull/` prefix and is silently a no-op — the first routed touch migrates it,
/// and this lane picks it up on the next pass.
///
/// Log-and-continue per repo, paced by `RECONCILE_GAP` for the same reason the marker lane is:
/// a backstop must yield bandwidth to real requests rather than compete with them.
pub async fn check_owned_pulls(app: &App) {
    for key in app.store.pool.warm_repos() {
        // Images have no pull requests; `repo/img/...` shares the pool with repos.
        if key.starts_with("img/") {
            continue;
        }
        let Some((owner, name)) = key.split_once('/') else { continue };
        if let Err(e) = crate::pulls::check_repo(&app.store, owner, name).await {
            tracing::warn!(owner = %owner, repo = %name, error = %e, "checking mergeability");
        }
        tokio::time::sleep(rustic_git_app::RECONCILE_GAP).await;
    }
}

/// Re-announce the merges this node's repos are still waiting on.
///
/// This node no longer PERFORMS merges. A merge is a fetch, a three-way merge and a push —
/// all of it expressible over the git protocol — so it happens in the worker, against a bare
/// clone, where an unbounded tree merge cannot sit in front of the pushes this node is serving
/// for the same repo. What stays here is the record: the job, the claim, and the outcome.
///
/// So the floor moved with it. A `MergeRequested` event is the nudge; this lane is what makes
/// a LOST nudge cost latency rather than the merge, by re-emitting it for every job that is
/// still queued or whose claim lapsed. It costs nothing when there is nothing waiting, and it
/// is idempotent by construction — the claim is what decides, and only one worker wins it.
///
/// Warm repos only and log-and-continue per repo, exactly like the two lanes above.
pub async fn announce_stranded_merges(app: &App) {
    for key in app.store.pool.warm_repos() {
        if key.starts_with("img/") {
            continue;
        }
        let Some((owner, name)) = key.split_once('/') else { continue };
        let stranded =
            match crate::pulls::stranded_merges(&app.store, owner, name, App::MERGE_LEASE).await {
                Ok(v) if v.is_empty() => continue, // nothing waiting; no reason to pace
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(owner = %owner, repo = %name, error = %e, "looking for stranded merges");
                    continue;
                }
            };
        for pr in stranded {
            let by = pr.merge.as_ref().map(|j| j.requested_by.clone()).unwrap_or_default();
            crate::events::publish(
                &app.store.cache,
                &crate::events::Event {
                    kind: crate::events::Kind::MergeRequested,
                    repo: format!("{owner}/{name}"),
                    number: pr.number,
                    actor: by,
                    at_ms: crate::ownership::now_ms() as i64,
                    title: pr.title.clone(),
                    base: pr.base.clone(),
                    head: pr.head.clone(),
                },
            )
            .await;
            // Stamped AFTER the event, and best-effort: a stamp that fails costs one extra
            // announcement on the next beat, while stamping first would lose the announcement
            // itself if the publish never happened.
            if let Err(e) = crate::pulls::mark_announced(&app.store, owner, name, pr.number).await {
                tracing::warn!(owner = %owner, repo = %name, number = pr.number, error = %e, "stamping the merge announcement");
            }
        }
        tokio::time::sleep(rustic_git_app::RECONCILE_GAP).await;
    }
}
