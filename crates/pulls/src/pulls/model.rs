//! The pull-request row, its numbering, and migration from the directory — everything the
//! worker links. No gix here: that is `check`, behind its own feature.

use crate::directory::{MergeState, MergeableState};
use kloudlite_git_core::{err, Result};
use kloudlite_git_storage::store::Store;
use serde::{Deserialize, Serialize};
use slatedb::Db;

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

/// The list page: newest first, at most `limit`, `comments` replaced by `commentCount`.
///
/// Scanned in DESCENDING key order and stopped at `limit`, so a repo with thousands of closed
/// changes reads only the page it shows. Comment bodies are skipped while parsing rather than
/// decoded and thrown away: the row is the wire shape already (`put` wrote it), so the rest
/// passes through as-is. `state` is compared against the serialized value — a filter, not a
/// validator, so an unrecognized value matches nothing.
pub async fn newest(db: &Db, state: Option<&str>, limit: usize) -> Result<Vec<serde_json::Value>> {
    #[derive(Deserialize)]
    struct Row {
        #[serde(default)]
        comments: Vec<serde::de::IgnoredAny>,
        #[serde(flatten)]
        rest: serde_json::Map<String, serde_json::Value>,
    }
    let opts = slatedb::config::ScanOptions {
        order: slatedb::IterationOrder::Descending,
        ..Default::default()
    };
    let mut it = db.scan_prefix_with_options(PULL_PREFIX.as_bytes(), .., &opts).await?;
    let mut out = Vec::new();
    while out.len() < limit {
        let Some(kv) = it.next().await? else { break };
        let mut row: Row = serde_json::from_slice(&kv.value)?;
        if state.is_some_and(|s| row.rest.get("state").and_then(|v| v.as_str()) != Some(s)) {
            continue;
        }
        row.rest.insert("commentCount".into(), row.comments.len().into());
        out.push(serde_json::Value::Object(row.rest));
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
            .map_err(|e| err(format!("{owner}/{name}: bad meta/next_pull: {e}")))?,
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
            Source::Unavailable => Err(err(format!(
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

/// A change whose mergeability needs a trial merge, and the branches to try.
///
/// Branch NAMES, not oids: the worker resolves them in its own clone of the repo, and a name is
/// what stays true if the branch moves between here and there — a stale oid would answer a
/// question nobody asked.
///
/// Lives here rather than in `check`, deliberately: `bins/worker/src/main.rs` names
/// `kloudlite_git_pulls::pulls::Deep`, and the worker links this crate WITHOUT the `check` feature.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Deep {
    pub number: i64,
    pub base: String,
    pub head: String,
}
