//! Pull requests, in the repo's own database.
//!
//! No HTTP and no Mongo here: this is the key encoding and the numbering sequence over a
//! SlateDB handle, so it is testable without a fleet.
//!
//! Timestamps are milliseconds since epoch, not `bson::DateTime`: a bson type survives a
//! non-bson serializer only by accident of its `Serialize` impl, and repo-local truth should
//! not carry a MongoDB-shaped value once Mongo is gone. The serde names still say `createdAt`
//! and friends, because those are the wire names the web app already reads.

use crate::store::Store;
use crate::Result;
use serde::{Deserialize, Serialize};
use slatedb::Db;

pub use crate::directory::{MergeState, MergeableState};

/// Zero-padded so lexical order over `pull/` IS numeric order — `scan_prefix` is the only
/// listing there is, and a bare decimal sorts `10` before `9`.
pub fn pull_key(number: i64) -> String {
    format!("pull/{number:08}")
}

const PULL_PREFIX: &str = "pull/";
/// The next number to hand out, decimal. In the `meta/` namespace beside `meta/public` and
/// `meta/created_at`: repo state, read and written by the node that owns the repo.
const NEXT_PULL_KEY: &[u8] = b"meta/next_pull";

/// A proposed change: take what is on `head` and put it on `base`.
///
/// Metadata only. The commits, the diff and the merge are git's, computed from
/// the refs this names — nothing here duplicates what the object database already
/// knows, so a PR cannot drift from the branch it is about.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PullRequest {
    /// `owner/name#number`. Redundant with the SlateDB key now that the repo owns the database,
    /// but the web app keys its list on it — see `web/.../components/repo/pulls.tsx`.
    #[serde(rename = "_id")]
    pub id: String,
    pub repo: String,
    /// Per repo, starting at 1. What people call it.
    pub number: i64,
    pub title: String,
    #[serde(default)]
    pub body: String,
    /// Branch SHORT names. Stored rather than resolved oids: a PR follows its
    /// branch, so a push to `head` updates what the PR contains, which is what
    /// everyone expects and what makes review iterative.
    pub base: String,
    pub head: String,
    pub state: PullState,
    pub author: String,
    #[serde(rename = "createdAt", deserialize_with = "ms")]
    pub created_at_ms: i64,
    #[serde(rename = "mergedAt", default, deserialize_with = "ms_opt", skip_serializing_if = "Option::is_none")]
    pub merged_at_ms: Option<i64>,
    #[serde(default)]
    pub comments: Vec<Comment>,
    /// Present once someone has asked for it to be merged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge: Option<MergeJob>,
    /// Kept fresh by the worker; read by the page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mergeability: Option<Mergeability>,
    /// When a worker last TOOK this change to look at — which is not the same as
    /// when it last answered. Top-level and separate from `mergeability` so a
    /// claim can be stamped without writing a half-built answer into it.
    #[serde(rename = "checkAt", default, deserialize_with = "ms_opt", skip_serializing_if = "Option::is_none")]
    pub check_at_ms: Option<i64>,
}

/// Whether a change could be merged, worked out ahead of being asked.
///
/// Computed in the background because the page must be able to say "this
/// conflicts" BEFORE anyone clicks — and because working it out is a real merge
/// attempt, not a lookup.
///
/// It records the two tips it was computed FROM. That is what makes it safe to
/// cache: the git nodes that accept pushes hold no directory connection and
/// cannot invalidate anything, so the only honest test of "is this still true" is
/// whether the branches have moved since.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Mergeability {
    pub state: MergeableState,
    /// The tips this answer was computed from.
    pub base_oid: String,
    pub head_oid: String,
    #[serde(rename = "checkedAt", deserialize_with = "ms")]
    pub checked_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Whether the base can simply MOVE to the head — the one strategy that writes no commit.
    /// `Clean` no longer implies it: a diverged branch that a trial merge combined cleanly is
    /// clean too, and offering fast-forward there would refuse at the click.
    /// `#[serde(default)]` so a row written before this field reads as "no", which is the safe
    /// direction: it hides an option rather than offering one that cannot work.
    #[serde(default)]
    pub fast_forward: bool,
}

/// A merge someone asked for, and how far it got.
///
/// Merging is a job rather than a request/response because it can be slow: a
/// three-way merge on a large tree is real work, and doing it inside the HTTP
/// call would tie up a request for as long as it takes — on the git nodes, which
/// are also serving pushes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MergeJob {
    pub state: MergeState,
    /// `fast-forward` | `squash` | `merge` | `rebase`.
    pub strategy: String,
    pub requested_by: String,
    #[serde(rename = "requestedAt", deserialize_with = "ms")]
    pub requested_at_ms: i64,
    /// When a worker took it. Also the lease: a job claimed long ago is assumed
    /// abandoned and may be claimed again, so a worker dying mid-merge does not
    /// strand the change forever.
    #[serde(rename = "claimedAt", default, deserialize_with = "ms_opt", skip_serializing_if = "Option::is_none")]
    pub claimed_at_ms: Option<i64>,
    /// Who took it — a token unique to one claimant, so winning the claim can be
    /// CONFIRMED rather than assumed. See `claim_merge`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimed_by: Option<String>,
    /// Why it stopped, when it did not succeed — written for the person waiting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// When the owner last RE-announced this job to the workers. Not the same as
    /// `requested_at_ms`, which never moves: this is what rate-limits the safety
    /// net, so a job nothing can claim cannot turn a 15s beat into a permanent
    /// event stream. See `stranded_merges`.
    #[serde(rename = "announcedAt", default, deserialize_with = "ms_opt", skip_serializing_if = "Option::is_none")]
    pub announced_at_ms: Option<i64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PullState {
    Open,
    Merged,
    Closed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Comment {
    pub author: String,
    pub body: String,
    #[serde(rename = "at", deserialize_with = "ms")]
    pub at_ms: i64,
}

/// Accepts a plain number OR a bson date, because rows written before this move still hold
/// `{"$date": …}` in Mongo and must keep reading. Serialization is always the plain number.
fn ms<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<i64, D::Error> {
    struct V;
    impl<'de> serde::de::Visitor<'de> for V {
        type Value = i64;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("milliseconds since epoch, or a bson date")
        }
        fn visit_i64<E: serde::de::Error>(self, v: i64) -> std::result::Result<i64, E> {
            Ok(v)
        }
        fn visit_u64<E: serde::de::Error>(self, v: u64) -> std::result::Result<i64, E> {
            Ok(v as i64)
        }
        fn visit_f64<E: serde::de::Error>(self, v: f64) -> std::result::Result<i64, E> {
            Ok(v as i64)
        }
        fn visit_str<E: serde::de::Error>(self, v: &str) -> std::result::Result<i64, E> {
            // Digits first, then RFC3339. A row written by the old code reaches us through
            // bson's deserializer as `{"$date": "2025-08-21T10:40:00Z"}` — a `$date` map whose
            // value is a STRING, not the number extended JSON shows. Parsing only digits here
            // failed every pre-existing row, which would have broken the migration for every PR
            // that already exists rather than for none of them.
            if let Ok(n) = v.parse::<i64>() {
                return Ok(n);
            }
            mongodb::bson::DateTime::parse_rfc3339_str(v)
                .map(|d| d.timestamp_millis())
                .map_err(serde::de::Error::custom)
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(
            self,
            mut m: A,
        ) -> std::result::Result<i64, A::Error> {
            // Extended JSON: `{"$date": 1700000000000}` or `{"$date": {"$numberLong": "…"}}`,
            // and bson's own deserializer presents a DateTime the same way.
            let mut out = None;
            while let Some(k) = m.next_key::<String>()? {
                if k == "$date" || k == "$numberLong" {
                    out = Some(m.next_value_seed(Ms)?);
                } else {
                    m.next_value::<serde::de::IgnoredAny>()?;
                }
            }
            out.ok_or_else(|| serde::de::Error::custom("no $date in a timestamp"))
        }
    }
    struct Ms;
    impl<'de> serde::de::DeserializeSeed<'de> for Ms {
        type Value = i64;
        fn deserialize<D: serde::Deserializer<'de>>(
            self,
            d: D,
        ) -> std::result::Result<i64, D::Error> {
            d.deserialize_any(V)
        }
    }
    d.deserialize_any(V)
}

fn ms_opt<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Option<i64>, D::Error> {
    #[derive(Deserialize)]
    struct Wrap(#[serde(deserialize_with = "ms")] i64);
    Ok(Option::<Wrap>::deserialize(d)?.map(|w| w.0))
}

pub async fn get(db: &Db, number: i64) -> Result<Option<PullRequest>> {
    match db.get(pull_key(number).as_bytes()).await? {
        Some(v) => Ok(Some(serde_json::from_slice(&v)?)),
        None => Ok(None),
    }
}

pub async fn put(db: &Db, pr: &PullRequest) -> Result<()> {
    db.put(pull_key(pr.number).as_bytes(), &serde_json::to_vec(pr)?).await?;
    Ok(())
}

/// Every change in the repo, oldest number first — the padded key does the sorting.
pub async fn list(db: &Db) -> Result<Vec<PullRequest>> {
    let mut it = db.scan_prefix(PULL_PREFIX.as_bytes(), ..).await?;
    let mut out = Vec::new();
    while let Some(kv) = it.next().await? {
        out.push(serde_json::from_slice(&kv.value)?);
    }
    Ok(out)
}

/// The changes that carry a merge job, without deserializing the ones that don't.
///
/// `merge` is `skip_serializing_if = Option::is_none`, so a jobless row has no `"merge":` key
/// in its bytes at all — and jobless (closed, merged, never-asked) rows are the unbounded
/// majority on the 15s announce beat. A comment body containing the literal is only a false
/// positive: it deserializes one extra row, which the `is_some` filter then drops.
pub async fn with_merge_jobs(db: &Db) -> Result<Vec<PullRequest>> {
    let mut it = db.scan_prefix(PULL_PREFIX.as_bytes(), ..).await?;
    let mut out = Vec::new();
    while let Some(kv) = it.next().await? {
        if !kv.value.windows(8).any(|w| w == b"\"merge\":") {
            continue;
        }
        let pr: PullRequest = serde_json::from_slice(&kv.value)?;
        if pr.merge.is_some() {
            out.push(pr);
        }
    }
    Ok(out)
}

/// The open ones only, capped — for a caller that does real work per row and must not be
/// handed every change the repo ever had.
pub async fn open_only(db: &Db, limit: usize) -> Result<Vec<PullRequest>> {
    let mut it = db.scan_prefix(PULL_PREFIX.as_bytes(), ..).await?;
    let mut out = Vec::new();
    while out.len() < limit {
        let Some(kv) = it.next().await? else { break };
        let pr: PullRequest = serde_json::from_slice(&kv.value)?;
        if pr.state == PullState::Open {
            out.push(pr);
        }
    }
    Ok(out)
}

/// The next free number for this repo, claimed. Read-increment-write under the repo's lock:
/// the number IS the key, so two callers reading the same value would have one change
/// overwrite the other.
pub async fn next_number(store: &Store, owner: &str, name: &str) -> Result<i64> {
    let lock = store.keyed_lock(&format!("pulls/{owner}/{name}"));
    let _guard = lock.lock().await;
    let db = store.db_for(owner, name).await?;
    let n: i64 = match db.get(NEXT_PULL_KEY).await? {
        Some(v) => String::from_utf8_lossy(&v)
            .parse()
            .map_err(|e| crate::err(format!("{owner}/{name}: bad meta/next_pull: {e}")))?,
        // A repo that has never had a change starts at 1, the number people expect to see first.
        None => 1,
    };
    db.put(NEXT_PULL_KEY, (n + 1).to_string().as_bytes()).await?;
    Ok(n)
}

/// `1` once this repo's Mongo pull requests have been copied in. Written LAST, always.
const MIGRATED_KEY: &[u8] = b"meta/pulls_migrated";

/// Where a repo's pre-move pull requests come from.
///
/// Three states, because "no handle" means two opposite things: a deployment without a directory
/// has nothing to migrate, while a deployment WITH one that could not be reached may have changes
/// nobody can see. Collapsing them into an `Option` is how a Mongo blip turns into data loss.
pub enum Source {
    /// No directory configured — a single-node deployment. Nothing to migrate, safe to record.
    Absent,
    /// Configured and reachable.
    Directory(std::sync::Arc<crate::directory::Directory>),
    /// Configured but NOT reachable. Migration must neither proceed nor be recorded.
    // ponytail: a node that failed to connect at startup stays here for its whole life, so pull
    // routes 500 until it restarts. Upgrade path: hold the uri and retry `Directory::connect`
    // behind an `ArcSwap`/`RwLock` here, promoting to `Directory` on the first success.
    Unavailable,
}

/// Copy this repo's pull requests out of Mongo and into its own database, once, on first touch.
///
/// Lazy and per-repo rather than a big-bang backfill, and it runs only on the node that owns the
/// repo — so it is a single writer by construction, like every other write in this design.
pub async fn ensure_migrated(
    store: &Store,
    src: &Source,
    owner: &str,
    name: &str,
) -> Result<()> {
    migrate_from(store, owner, name, || async {
        match src {
            Source::Directory(d) => d.pulls_for(&format!("{owner}/{name}")).await,
            Source::Absent => Ok(Vec::new()),
            // Through the row closure rather than an early return, so it takes the same path as
            // any other failed read: the marker is never written and the next touch retries.
            Source::Unavailable => Err(crate::err(format!(
                "{owner}/{name}: directory configured but unreachable; refusing to record a \
                 migration of changes this node cannot read"
            ))),
        }
    })
    .await
}

/// The migration itself, over an injected row source — the only Mongo-shaped thing about it is
/// the caller. Taking the source as a closure keeps the read LAZY: the fast path stays one `get`
/// and never queries anything, and every property below is testable without a live Mongo.
pub async fn migrate_from<F, Fut>(store: &Store, owner: &str, name: &str, rows: F) -> Result<()>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<Vec<PullRequest>>>,
{
    let db = store.db_for(owner, name).await?;
    if is_migrated(&db).await? {
        return Ok(());
    }
    let lock = store.keyed_lock(&format!("pulls/{owner}/{name}"));
    let _guard = lock.lock().await;
    // Re-check UNDER the lock: without it two concurrent first touches both migrate.
    if is_migrated(&db).await? {
        return Ok(());
    }

    // A failed read must NOT be remembered as done — marking migrated here would lose every
    // existing change for this repo, silently and permanently. Return and let the next call retry.
    let rows = rows().await?;

    let mut next = 1;
    for pr in &rows {
        put(&db, pr).await?;
        next = next.max(pr.number + 1);
    }
    // From the rows, never from Mongo's `counters` or its sort order: rows written before and
    // after the timestamp change hold Date and Int64, so `sort({createdAt:-1})` mixes types and
    // its order is not trustworthy. An existing value only ever wins upward, so a crash that got
    // as far as handing out numbers cannot have one reissued.
    if let Some(v) = db.get(NEXT_PULL_KEY).await? {
        next = next.max(String::from_utf8_lossy(&v).parse().unwrap_or(1));
    }
    db.put(NEXT_PULL_KEY, next.to_string().as_bytes()).await?;

    // LAST, for the same reason truth precedes views everywhere else here: a crash mid-copy
    // leaves work to redo (re-`put`ting identical keys, which cannot duplicate), never a repo
    // that believes it migrated when it did not.
    db.put(MIGRATED_KEY, b"1").await?;
    Ok(())
}

async fn is_migrated(db: &Db) -> Result<bool> {
    Ok(db.get(MIGRATED_KEY).await?.as_deref() == Some(b"1".as_ref()))
}

/// The most open changes one repo-wide sweep will look at. A `HeadMoved` fan-out and the owner's
/// periodic lane share it: neither may turn one push into an unbounded serial graph walk that
/// starves the node it runs on.
/// ponytail: a flat cap, not a queue — a repo with more open changes than this leaves the tail to
/// the next pass. Upgrade to a cursor over `pull/` if a repo regularly exceeds it.
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
    repo: &crate::store::Repo,
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
                repo2.odb().map(|odb| crate::browse::merge_base(&odb, b, h, BUDGET))
            })
            .await
            .map_err(|e| crate::err(format!("comparing: {e}")))??;
            let fast_forward = mb == Some(b);
            // Three answers this node can give for free, and one it cannot. Ancestry is a graph
            // walk over data already here; whether two diverged trees COMBINE is a merge, and a
            // merge is the worker's job — see the module doc on `crate::merge_worker`.
            let (state, ff, detail) = match (&mb, fast_forward) {
                (Some(_), true) => (MergeableState::Clean, true, None),
                (Some(m), _) if *m == h => (
                    MergeableState::Behind,
                    false,
                    Some(format!("this branch is already in {}", pr.base)),
                ),
                (None, _) => (
                    MergeableState::Dirty,
                    false,
                    Some("these branches share no history".to_string()),
                ),
                // Diverged: both branches have commits the other does not.
                (Some(_), false) => {
                    deep = true;
                    (MergeableState::Unknown, false, Some("checking…".to_string()))
                }
            };
            Mergeability {
                state,
                base_oid: now_base.clone(),
                head_oid: now_head.clone(),
                checked_at_ms: crate::ownership::now_ms() as i64,
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
            checked_at_ms: crate::ownership::now_ms() as i64,
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

/// A change whose mergeability needs a trial merge, and the branches to try.
///
/// Branch NAMES, not oids: the worker resolves them in its own clone of the repo, and a name is
/// what stays true if the branch moves between here and there — a stale oid would answer a
/// question nobody asked.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Deep {
    pub number: i64,
    pub base: String,
    pub head: String,
}

/// Every open change in one repo, checked. Both discovery paths land here: the owner's periodic
/// lane sweeps its repos with it, and a `HeadMoved` event — which is about a branch, not a change —
/// fans out through it.
pub async fn check_repo(store: &Store, owner: &str, name: &str) -> Result<Vec<Deep>> {
    let db = store.db_for(owner, name).await?;
    // One `open_repo` for the whole sweep, not one per change: it does marker reconcile and a
    // pack sync, and paying that per PR was most of the background lane's cost.
    let Some(repo) = store.open_repo(owner, name).await? else { return Ok(Vec::new()) };
    let mut deep = Vec::new();
    for pr in open_only(&db, CHECK_LIMIT).await? {
        if let Checked::Deep(d) = check_with(store, owner, name, &repo, pr.number).await? {
            deep.push(d);
        }
    }
    Ok(deep)
}

// ---------------------------------------------------------------------------
// Merge jobs.
//
// A merge job hangs off a `PullRequest`, so it lives in the repo's own database like everything
// else here, and only the node that owns the repo may touch it.
//
// Mongo's `claim_merge` needed `find_one_and_update` because ANY worker replica could claim, so
// atomicity had to hold across processes. Repo-local there is exactly ONE writer by construction
// — the owning node — so the repo's `pulls/{owner}/{name}` lock is sufficient, and in fact
// stronger: the race the compare-and-swap was defending against cannot be reached at all.
// ---------------------------------------------------------------------------

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

/// Take the next merge this repo has waiting, marking it taken. `None` means there is nothing.
///
/// The scan happens INSIDE the lock, not just the write: a candidate chosen outside it could be
/// claimed by someone else before the write lands, which is the exact double-merge this exists
/// to prevent.
///
/// `lease` is how long a claim stands before the job is assumed abandoned and may be taken again
/// — so a node dying mid-merge delays the change rather than stranding it forever.
pub async fn claim_merge(
    store: &Store,
    owner: &str,
    name: &str,
    lease: std::time::Duration,
    me: &str,
) -> Result<Option<PullRequest>> {
    let db = store.db_for(owner, name).await?;
    let lock = store.keyed_lock(&format!("pulls/{owner}/{name}"));
    let _guard = lock.lock().await;
    let now = crate::ownership::now_ms() as i64;
    let lease_ms = lease.as_millis() as i64;
    for mut pr in with_merge_jobs(&db).await? {
        if !takeable(&pr, now, lease_ms) {
            continue;
        }
        if let Some(job) = pr.merge.as_mut() {
            job.state = MergeState::Running;
            job.claimed_at_ms = Some(now);
            job.claimed_by = Some(me.to_string());
        }
        put(&db, &pr).await?;
        return Ok(Some(pr));
    }
    Ok(None)
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
/// The by-number twin of `claim_merge`, for the worker: a nudge is about ONE change, and scanning
/// the repo for "any queued merge" would have a worker claim a job some other worker was already
/// nudged about.
pub async fn claim_merge_number(
    store: &Store,
    owner: &str,
    name: &str,
    number: i64,
    lease: std::time::Duration,
    me: &str,
) -> Result<Option<PullRequest>> {
    let now = crate::ownership::now_ms() as i64;
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
    let now = crate::ownership::now_ms() as i64;
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
            job.announced_at_ms = Some(crate::ownership::now_ms() as i64);
            true
        }
        None => false,
    })
    .await
    .map(|_| ())
}

/// Record how a merge ended, leaving the job in place: the state and the reason are what the
/// person waiting is shown, and a failed job may be retried from there.
pub async fn finish_merge(
    store: &Store,
    owner: &str,
    name: &str,
    number: i64,
    state: MergeState,
    detail: Option<&str>,
) -> Result<()> {
    modify(store, owner, name, number, |pr| {
        let Some(job) = pr.merge.as_mut() else { return false };
        job.state = state;
        job.detail = detail.map(str::to_string);
        true
    })
    .await
    .map(|_| ())
}

/// Drop the job entirely. `Queued` is not a state a finished job stays in, and a merged change
/// already records that it merged in its own `state` — so clearing is the honest end.
pub async fn clear_merge(store: &Store, owner: &str, name: &str, number: i64) -> Result<()> {
    modify(store, owner, name, number, |pr| pr.merge.take().is_some()).await.map(|_| ())
}

/// Open, merged or closed. `merged_at` is stamped here rather than by the caller so the two can
/// never disagree.
pub async fn set_state(
    store: &Store,
    owner: &str,
    name: &str,
    number: i64,
    state: PullState,
) -> Result<()> {
    modify(store, owner, name, number, |pr| {
        pr.state = state;
        if state == PullState::Merged && pr.merged_at_ms.is_none() {
            pr.merged_at_ms = Some(crate::ownership::now_ms() as i64);
        }
        true
    })
    .await
    .map(|_| ())
}
