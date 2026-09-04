// ---------------------------------------------------------------------------
// Merge jobs.
//
// A merge job hangs off a `PullRequest`, so it lives in the repo's own database like everything
// else here, and only the node that owns the repo may touch it.
//
// Mongo's claim needed `find_one_and_update` because ANY worker replica could claim, so
// atomicity had to hold across processes. Repo-local there is exactly ONE writer by construction
// — the owning node — so the repo's `pulls/{owner}/{name}` lock is sufficient, and in fact
// stronger: the race the compare-and-swap was defending against cannot be reached at all.
// ---------------------------------------------------------------------------

use super::model::{get, put, with_merge_jobs, PullRequest, PullState};
use crate::directory::MergeState;
use kloudlite_git_core::Result;
use kloudlite_git_storage::store::Store;

/// Read-modify-write of one change, under the repo's own pull lock.
///
/// The one locking pattern for a change; `browse_api::pulls::update` is its HTTP face and calls
/// straight through here. The lock spans the read AND the write because every write is a
/// modification of one row: two callers that both read the same row would lose one of them.
/// `f` returning `false` means "leave it alone" — nothing is written and the answer is `None`,
/// which is also what a missing change gives, since neither is a change the caller made.
pub async fn modify(
    store: &Store,
    owner: &str,
    name: &str,
    number: i64,
    f: impl FnOnce(&mut PullRequest) -> bool,
) -> Result<Option<PullRequest>> {
    let db = store.db_for(owner, name).await?;
    let lock = store.keyed_lock(&format!("pulls/{owner}/{name}"));
    let _guard = lock.lock().await;
    let Some(mut pr) = get(&db, number).await? else { return Ok(None) };
    if !f(&mut pr) {
        return Ok(None);
    }
    put(&db, &pr).await?;
    Ok(Some(pr))
}

/// Is this job free to take? Queued always; Running only once its claimant has had longer than
/// the lease and is presumed gone.
fn takeable(pr: &PullRequest, now: i64, lease_ms: i64) -> bool {
    match pr.merge.as_ref().map(|j| (j.state, j.claimed_at_ms)) {
        Some((MergeState::Queued, _)) => true,
        Some((MergeState::Running, at)) => at.is_none_or(|t| now - t > lease_ms),
        _ => false,
    }
}

/// One named change's merge job, claimed. `None` means it is not there to take — no job, already
/// running under a live lease, or already finished.
///
/// Claims by number, for the worker: a nudge is about ONE change, and scanning the repo for "any
/// queued merge" would have a worker claim a job some other worker was already nudged about.
pub async fn claim_merge_number(
    store: &Store,
    owner: &str,
    name: &str,
    number: i64,
    lease: std::time::Duration,
    me: &str,
) -> Result<Option<PullRequest>> {
    let now = kloudlite_git_storage::ownership::now_ms() as i64;
    let lease_ms = lease.as_millis() as i64;
    modify(store, owner, name, number, |pr| {
        if pr.state != PullState::Open || !takeable(pr, now, lease_ms) {
            return false;
        }
        if let Some(job) = pr.merge.as_mut() {
            job.state = MergeState::Running;
            job.claimed_at_ms = Some(now);
            job.claimed_by = Some(me.to_string());
        }
        true
    })
    .await
}

/// How long a job must have gone unclaimed before the owner says so again.
///
/// The floor exists for a job whose nudge was LOST, which is rare; the common case is a job the
/// merge handler announced a moment ago and a worker is already claiming. Without this the 15s
/// beat would re-announce that job on every pass, and a job nothing can claim — no worker running,
/// a repo whose merges all fail — would publish forever. The events stream is capped
/// (`MAXLEN 5000`), so that does not grow without bound; it does something worse, which is evict
/// the activity feed everyone else is reading.
pub const ANNOUNCE_EVERY: std::time::Duration = std::time::Duration::from_secs(30);

/// Every merge in this repo that is still waiting and is due to be said again — a lost nudge, or a
/// worker that took the job and died. The owner re-announces these; it no longer performs them.
///
/// `announced_at_ms` (falling back to `requested_at_ms`, for a job from before that field existed
/// and for one nobody has re-announced yet) is what paces it. `mark_announced` moves the stamp.
pub async fn stranded_merges(
    store: &Store,
    owner: &str,
    name: &str,
    lease: std::time::Duration,
) -> Result<Vec<PullRequest>> {
    let db = store.db_for(owner, name).await?;
    let now = kloudlite_git_storage::ownership::now_ms() as i64;
    let lease_ms = lease.as_millis() as i64;
    let quiet_ms = ANNOUNCE_EVERY.as_millis() as i64;
    Ok(with_merge_jobs(&db)
        .await?
        .into_iter()
        .filter(|pr| {
            if pr.state != PullState::Open || !takeable(pr, now, lease_ms) {
                return false;
            }
            let Some(job) = pr.merge.as_ref() else { return false };
            let said = job.announced_at_ms.unwrap_or(job.requested_at_ms);
            now - said > quiet_ms
        })
        .collect())
}

/// Stamp a job as announced, so the next beat does not announce it again immediately.
pub async fn mark_announced(store: &Store, owner: &str, name: &str, number: i64) -> Result<()> {
    modify(store, owner, name, number, |pr| match pr.merge.as_mut() {
        Some(job) => {
            job.announced_at_ms = Some(kloudlite_git_storage::ownership::now_ms() as i64);
            true
        }
        None => false,
    })
    .await
    .map(|_| ())
}
