//! Mergeability checking — the gix graph walk. Feature-gated: the worker links `pulls`
//! without `check` and must not pull in gix or `kloudlite_gitbase`.

use super::model::{get, put, open_only, Deep, Mergeability, PullState};
use crate::directory::MergeableState;
use kloudlite_core::{err, Result};
use kloudlite_storage::store::{Repo, Store};

/// The most graph walks one repo-wide sweep will do. A `HeadMoved` fan-out and the owner's
/// periodic lane share it: neither may turn one push into an unbounded serial graph walk that
/// starves the node it runs on. It caps WORK, not rows: every open change is looked at (two ref
/// reads each, which is what makes the unchanged case cheap), and only the ones whose tips moved
/// count — capping rows meant the same 25 lowest-numbered changes filled it on every pass and
/// #26 onward were never checked at all.
/// ponytail: a flat cap, not a queue — a repo with more moved changes than this leaves the tail to
/// the next pass, which sees them first-by-number again. Upgrade to a cursor over `pull/` if a
/// repo regularly exceeds it.
pub const CHECK_LIMIT: usize = 25;

/// Recompute one change's mergeability and record it, in the repo's own database.
///
/// This runs ONLY on the node that owns the repo, which is what makes it correct AND cheap: that
/// node holds the refs and the objects, so the answer is a local graph walk rather than the two
/// HTTP round trips the merge worker used to make to ask a node about a repo it was already
/// serving. It is also why discovery had to move here at all — no other process may open this
/// database without fencing the owner.
///
/// `Checked::Unchanged` means there was nothing to do: the change is gone or no longer open, or
/// neither tip has moved since the last answer. Nothing is written in that case, deliberately — a
/// lane that restamped every change it looked at would rewrite the whole repo on every pass.
///
/// `Checked::Deep` means the cheap answer ran out: the branches DIVERGED, and whether they combine
/// is a real three-way merge. That is the worker's to do with the git binary — see
/// `crate::merge_worker` — so the row is stamped `Unknown`/"checking…" and the caller is told
/// which change to hand over.
pub async fn check(store: &Store, owner: &str, name: &str, number: i64) -> Result<Checked> {
    let Some(repo) = store.open_repo(owner, name).await? else { return Ok(Checked::Unchanged) };
    check_with(store, owner, name, &repo, number).await
}

/// The check itself, against a repo the caller already opened — `check_repo` sweeps many
/// changes and must not pay `open_repo` (marker reconcile, pack sync) once per change.
async fn check_with(
    store: &Store,
    owner: &str,
    name: &str,
    repo: &Repo,
    number: i64,
) -> Result<Checked> {
    let db = store.db_for(owner, name).await?;
    let Some(pr) = get(&db, number).await? else { return Ok(Checked::Unchanged) };
    if pr.state != PullState::Open {
        return Ok(Checked::Unchanged);
    }

    // The tips FIRST, because that is the cheap question: reading two refs is two `get`s, while
    // comparing the branches walks the commit graph to find where they parted.
    let base = store.get_ref(repo, &format!("refs/heads/{}", pr.base)).await?;
    let head = store.get_ref(repo, &format!("refs/heads/{}", pr.head)).await?;
    // A branch that is gone is the empty string rather than an absent value, so the "has anything
    // moved?" test below converges on a deleted branch too — otherwise a change whose head was
    // deleted would be recomputed on every single pass, forever.
    let hex = |o: &Option<gix_hash::ObjectId>| o.map(|o| o.to_hex().to_string()).unwrap_or_default();
    let (now_base, now_head) = (hex(&base), hex(&head));
    if let Some(old) = &pr.mergeability {
        if old.base_oid == now_base && old.head_oid == now_head {
            return Ok(Checked::Unchanged);
        }
    }

    // Set alongside the row when the cheap answer ran out, and returned to the caller.
    let mut deep = false;
    let m = match (base, head) {
        (Some(b), Some(h)) => {
            // `merge_base` alone: the old `compare(_, _, _, 1)` also built a full unified diff
            // from the merge base and walked commit history, all of it discarded — the sweep
            // needs the ancestry verdict, nothing else.
            // Ceiling on the deep sweep: past-budget divergence returns Unknown and defers to
            // the worker rather than walking forever. The now_base/now_head unchanged-guard
            // above is what keeps this cheap in the common case — most sweeps never reach here.
            const BUDGET: usize = 50_000;
            let repo2 = repo.clone();
            let mb = tokio::task::spawn_blocking(move || {
                repo2.odb().map(|odb| kloudlite_gitbase::merge_base(&odb, b, h, BUDGET))
            })
            .await
            .map_err(|e| err(format!("comparing: {e}")))??;
            use kloudlite_gitbase::MergeBase;
            // Three answers this node can give for free, and one it cannot. Ancestry is a graph
            // walk over data already here; whether two diverged trees COMBINE is a merge, and a
            // merge is the worker's job — see the module doc on `crate::merge_worker`.
            let (state, ff, detail) = match mb {
                MergeBase::Found(m) if m == b => (MergeableState::Clean, true, None),
                MergeBase::Found(m) if m == h => (
                    MergeableState::Behind,
                    false,
                    Some(format!("this branch is already in {}", pr.base)),
                ),
                MergeBase::Unrelated => (
                    MergeableState::Dirty,
                    false,
                    Some("these branches share no history".to_string()),
                ),
                // Diverged, or too deep to tell: both are the worker's question. An exhausted
                // walk must never be recorded as Dirty — that hid the merge button on every
                // long-lived branch of a big repo.
                MergeBase::Found(_) | MergeBase::Exhausted => {
                    deep = true;
                    (MergeableState::Unknown, false, Some("checking…".to_string()))
                }
            };
            Mergeability {
                state,
                base_oid: now_base.clone(),
                head_oid: now_head.clone(),
                checked_at_ms: kloudlite_storage::ownership::now_ms() as i64,
                detail,
                fast_forward: ff,
            }
        }
        // Not an error: the change is simply not mergeable until someone pushes the branch back,
        // and saying so beats retrying forever.
        _ => Mergeability {
            state: MergeableState::Unknown,
            base_oid: now_base,
            head_oid: now_head,
            checked_at_ms: kloudlite_storage::ownership::now_ms() as i64,
            detail: Some("one of the branches is gone".to_string()),
            fast_forward: false,
        },
    };

    // Re-read under the repo's pull lock: the comparison above took real time, and a comment or a
    // merge request that landed meanwhile must not be thrown away by writing back the stale row.
    let lock = store.keyed_lock(&format!("pulls/{owner}/{name}"));
    let _guard = lock.lock().await;
    let Some(mut fresh) = get(&db, number).await? else { return Ok(Checked::Unchanged) };
    fresh.check_at_ms = Some(m.checked_at_ms);
    fresh.mergeability = Some(m);
    put(&db, &fresh).await?;
    Ok(if deep {
        Checked::Deep(Deep { number, base: fresh.base.clone(), head: fresh.head.clone() })
    } else {
        Checked::Answered
    })
}

/// What one cheap check concluded.
#[derive(Debug, Clone, PartialEq)]
pub enum Checked {
    /// Nothing moved, or there is nothing to check. Nothing was written.
    Unchanged,
    /// Answered from ancestry alone, and recorded.
    Answered,
    /// Recorded as "checking…"; the worker must try the merge for real.
    Deep(Deep),
}

/// Every open change in one repo, checked. Both discovery paths land here: the owner's periodic
/// lane sweeps its repos with it, and a `HeadMoved` event — which is about a branch, not a change —
/// fans out through it.
pub async fn check_repo(store: &Store, owner: &str, name: &str) -> Result<Vec<Deep>> {
    let db = store.db_for(owner, name).await?;
    // The row scan first: a repo with no open change — most of them, on every 30 s pass — must
    // not pay a pack sync to learn there is nothing to check.
    let open = open_only(&db, usize::MAX).await?;
    if open.is_empty() {
        return Ok(Vec::new());
    }
    // One `open_repo` for the whole sweep, not one per change: it does marker reconcile and a
    // pack sync, and paying that per PR was most of the background lane's cost.
    let Some(repo) = store.open_repo(owner, name).await? else { return Ok(Vec::new()) };
    let mut deep = Vec::new();
    let mut walked = 0;
    for pr in open {
        if walked >= CHECK_LIMIT {
            break;
        }
        match check_with(store, owner, name, &repo, pr.number).await? {
            Checked::Unchanged => {}
            Checked::Answered => walked += 1,
            Checked::Deep(d) => {
                walked += 1;
                deep.push(d);
            }
        }
    }
    Ok(deep)
}
