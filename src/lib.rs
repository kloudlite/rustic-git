// `Result<T, axum::Response>` is the handler idiom here: the Err is an early-return response,
// unwrapped exactly once per request by `?`. Boxing it to please the size lint would add an
// allocation per refusal for no measurable gain.
#![allow(clippy::result_large_err)]

pub mod api;
pub mod auth;
pub mod browse;
pub mod gc;
pub mod gpg;
pub mod http;
pub mod protocol;
pub mod proxy;
pub mod registry;
pub mod ssh;

pub use rustic_git_core::{err, hex, require_jwt_secret, require_jwt_secret_from_env, Error, Result};
pub use rustic_git_core::{jwt, pktline};
pub use rustic_git_storage::{cache, config, events, index, ownership, pool, refmeta, store};
pub use rustic_git_gitbase::{objects, refs};
pub use rustic_git_pulls::{directory, merge_worker, pulls};
pub use rustic_git_app::{App, AddrOf, Patience, RECOVERY_ASK_EVERY};

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
            Some(rest) => (index::Kind::Img, rest),
            None => (index::Kind::Repo, key.as_str()),
        };
        let Some((owner, name)) = rest.split_once('/') else { continue };
        let db_public = match kind {
            index::Kind::Repo => app.store.is_public(owner, name).await,
            index::Kind::Img => {
                use crate::registry::store::ImageExt;
                app.store.image_is_public(owner, name).await
            }
        };
        let db_public = match db_public {
            Ok(v) => v,
            Err(e) => {
                eprintln!("reconcile marker {owner}/{name}: {e}"); // ponytail: eprintln
                continue;
            }
        };
        if let Err(e) = app.store.reconcile_marker(owner, name, kind, db_public).await {
            eprintln!("reconcile marker {owner}/{name}: {e}"); // ponytail: eprintln
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
        if let Err(e) = pulls::check_repo(&app.store, owner, name).await {
            eprintln!("checking mergeability for {owner}/{name}: {e}"); // ponytail: eprintln
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
            match pulls::stranded_merges(&app.store, owner, name, App::MERGE_LEASE).await {
                Ok(v) if v.is_empty() => continue, // nothing waiting; no reason to pace
                Ok(v) => v,
                Err(e) => {
                    eprintln!("looking for stranded merges in {owner}/{name}: {e}"); // ponytail: eprintln
                    continue;
                }
            };
        for pr in stranded {
            let by = pr.merge.as_ref().map(|j| j.requested_by.clone()).unwrap_or_default();
            events::publish(
                &app.store.cache,
                &events::Event {
                    kind: events::Kind::MergeRequested,
                    repo: format!("{owner}/{name}"),
                    number: pr.number,
                    actor: by,
                    at_ms: ownership::now_ms() as i64,
                    title: pr.title.clone(),
                    base: pr.base.clone(),
                    head: pr.head.clone(),
                },
            )
            .await;
            // Stamped AFTER the event, and best-effort: a stamp that fails costs one extra
            // announcement on the next beat, while stamping first would lose the announcement
            // itself if the publish never happened.
            if let Err(e) = pulls::mark_announced(&app.store, owner, name, pr.number).await {
                eprintln!("stamping the merge announcement for {owner}/{name}#{}: {e}", pr.number); // ponytail: eprintln
            }
        }
        tokio::time::sleep(rustic_git_app::RECONCILE_GAP).await;
    }
}

