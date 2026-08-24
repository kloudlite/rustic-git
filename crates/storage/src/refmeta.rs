//! Ref and repo-metadata storage: the gix-free half of what was `src/refs.rs`.
//!
//! `src/refs.rs` (root crate, `crates/gitbase` from Task 4 onward) keeps `protection_verdict` and
//! `is_ancestor`, which walk history via `gix-traverse` — a dependency `storage` must not carry
//! (see `crates/storage/Cargo.toml`; `Repo::odb() -> gix_odb::Handle` is the only gix surface this
//! crate exposes). Everything else that used to live in one `impl Store` block in `refs.rs` is
//! plain SlateDB CRUD with no gix at all, and it has to live wherever `Store` lives — Rust's orphan
//! rule forbids an inherent `impl Store` outside the crate that defines `Store`, so once `Store`
//! moved here, so did every inherent method on it, this included. `update_refs` itself is split at
//! its one gix-touching step: `update_refs_txn` here does the transactional compare-and-swap;
//! `refs::update_refs` in the root crate computes the protection verdicts (the part that needs
//! `gix-traverse`) and then calls it. See task-3-report.md for the full account.

use crate::store::{Repo, Store};
use crate::{err, Result};
use gix_hash::ObjectId;
use slatedb::{ErrorKind, IsolationLevel, WriteBatch};

/// Everything a repo records about itself, read in one pass.
pub struct RepoMeta {
    pub description: String,
    pub created_by: String,
    pub created_at_ms: i64,
    pub public: bool,
}

#[derive(Clone)]
pub struct RefUpdate {
    pub name: String,
    pub old: Option<ObjectId>,
    pub new: Option<ObjectId>,
}

fn ref_key(repo: &Repo, name: &str) -> String {
    format!("ref/{}/{}/{}", repo.owner, repo.name, name)
}
fn ref_prefix(repo: &Repo) -> String {
    format!("ref/{}/{}/", repo.owner, repo.name)
}
fn repo_key(owner: &str, name: &str) -> String {
    format!("repo/{owner}/{name}")
}
fn ref_key_for(owner: &str, name: &str, refname: &str) -> String {
    format!("ref/{owner}/{name}/{refname}")
}

fn parse_oid(b: &[u8]) -> Result<ObjectId> {
    ObjectId::from_hex(b).map_err(|e| err(e.to_string()))
}

/// A repo's visibility. Lives in the repo database rather than as an object key because it is
/// repo state, read on the owner alongside the refs it guards.
const PUBLIC_KEY: &[u8] = b"meta/public";

/// A repo's own description of itself. Discrete keys rather than one encoded blob so a
/// description edit is a single put that a concurrent visibility flip cannot clobber.
const DESCRIPTION_KEY: &[u8] = b"meta/description";
const CREATED_BY_KEY: &[u8] = b"meta/created_by";
/// Milliseconds since epoch, decimal. Also the sentinel for "metadata was never written" —
/// `meta/public` predates this namespace, so it cannot answer that question.
const CREATED_AT_KEY: &[u8] = b"meta/created_at";

/// Branch protection, one key per pattern, in the REPO's own database.
///
/// Not in the directory with teams and repos: the git nodes have no database
/// connection, and a rule that cannot be read by the node accepting the push is
/// not a rule. It lives beside `meta/public` for the same reason — repo state,
/// read by whichever node is serving the repo, written by that same node.
const PROTECT_PREFIX: &str = "meta/protect/";

fn protect_key(owner: &str, name: &str, pattern: &str) -> String {
    format!("{PROTECT_PREFIX}{owner}/{name}/{pattern}")
}

fn protect_scan(owner: &str, name: &str) -> String {
    format!("{PROTECT_PREFIX}{owner}/{name}/")
}

/// What a pattern forbids. Both default to on: a rule that forbids nothing is a
/// rule someone thinks is protecting them.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Protection {
    /// `main`, or a trailing-star glob like `release/*`. Matched against the
    /// SHORT branch name, which is what a person types and what they see.
    pub pattern: String,
    /// Refuse a push that is not a fast-forward — the rewrite-history case.
    pub no_force: bool,
    /// Refuse deleting the branch.
    pub no_delete: bool,
}

/// The stored value is two flags; the pattern is already the key. A JSON document
/// for two booleans would mean a serialiser in the push path and a dependency in
/// the library for nothing.
impl Protection {
    fn encode(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(2);
        if self.no_force {
            v.push(b'f');
        }
        if self.no_delete {
            v.push(b'd');
        }
        v
    }

    fn decode(pattern: &str, value: &[u8]) -> Protection {
        Protection {
            pattern: pattern.to_string(),
            no_force: value.contains(&b'f'),
            no_delete: value.contains(&b'd'),
        }
    }
}

impl Protection {
    /// Exact name, or a trailing `*`. Deliberately not full glob: `release/*` and
    /// `main` cover what people actually protect, and a half-implemented glob is
    /// worse than a small one nobody misreads.
    pub fn matches(&self, branch: &str) -> bool {
        match self.pattern.strip_suffix('*') {
            Some(prefix) => branch.starts_with(prefix),
            None => self.pattern == branch,
        }
    }
}

impl Store {
    pub async fn create_repo(&self, owner: &str, name: &str) -> Result<()> {
        if !crate::store::valid_owner(owner) || !crate::store::valid_segment(name) {
            return Err(err("invalid repo path"));
        }
        if self.repo_exists(owner, name).await? {
            return Err(err("repository already exists"));
        }
        self.db_for(owner, name).await?
            .put(repo_key(owner, name), b"")
            .await?;
        Ok(())
    }

    /// Fork: a new repo with its own copy of the source's objects and refs.
    ///
    /// Objects are copied, not shared. Forks are rare and object storage is cheap, whereas sharing
    /// a pile forces garbage collection to see every repo using it — which constrains where repos
    /// may live and makes cross-repo object exposure possible in the first place.
    ///
    /// Packs are copied first, then the repo row and refs land in one batch, so the fork becomes
    /// visible only once its objects and refs are both in place.
    ///
    /// No cache invalidation, deliberately: this writes refs without going through `update_refs`,
    /// but the target repo cannot exist yet (refused below), a previous repo of that name bumped
    /// the generation when it was deleted, and 404s are never cached — so no entry for the new name
    /// can exist. Allowing a fork OVER an existing repo, or caching negative responses, breaks that
    /// and this needs a `drop_refs`.
    pub async fn fork(&self, src: &Repo, owner: &str, name: &str) -> Result<()> {
        if !crate::store::valid_owner(owner) || !crate::store::valid_segment(name) {
            return Err(err("invalid repo path"));
        }
        if self.repo_exists(owner, name).await? {
            return Err(err("repository already exists"));
        }
        let refs = self.list_refs(src).await?;
        // copy the objects before publishing the repo row: no window where the fork is visible
        // but its objects are missing
        let dst_repo = Repo {
            owner: owner.to_string(),
            name: name.to_string(),
            objects_dir: Default::default(),
            pack_dir: Default::default(),
        };
        self.copy_packs(src, &dst_repo).await?;
        let mut b = WriteBatch::new();
        b.put(repo_key(owner, name), b"");
        for (r, oid) in refs {
            b.put(
                ref_key_for(owner, name, &r),
                oid.to_hex().to_string().as_bytes(),
            );
        }
        self.db_for(owner, name).await?.write(b).await?;
        Ok(())
    }

    /// Remove a repo: its refs and repo row in one batch, then its objects.
    ///
    /// The metadata delete is atomic, so there is no window where the repo is un-openable but its
    /// refs linger (a later create-repo of the same name would resurrect them). Objects are
    /// deleted afterwards: they belong to this repo alone, so nothing else can need them, and a
    /// crash in between only leaves storage to reclaim rather than a broken repo.
    pub async fn delete_repo(&self, owner: &str, name: &str) -> Result<()> {
        if !self.repo_exists(owner, name).await? {
            return Err(err("repository not found"));
        }
        let prefix = format!("ref/{owner}/{name}/");
        let db = self.db_for(owner, name).await?;
        let mut it = db.scan_prefix(prefix.as_bytes(), ..).await?;
        let mut b = WriteBatch::new();
        while let Some(kv) = it.next().await? {
            b.delete(&kv.key);
        }
        b.delete(repo_key(owner, name));
        db.write(b).await?;
        self.delete_objects(owner, name).await?;
        // The database's own files too, not just the git objects: a surviving `repo/{owner}/{name}/`
        // directory is storage nobody reclaims AND a repo the GC sweep resurrects a marker for.
        self.delete_repo_db(owner, name).await?;
        // Orphans every cached answer for this repo: a name can be recreated, and a hit from the
        // deleted repo's life would be served for the new one. Propagated, not swallowed: the
        // repo is already gone from the database by here, so a silent failure would leave its
        // cached bodies readable for their full TTL.
        self.cache.bump_generation(&format!("{owner}/{name}")).await.map_err(|e| {
            err(format!(
                "{e}: {owner}/{name} is deleted but its cached responses are still being served; \
                 retry with `admin purge-cache {owner}/{name}`"
            ))
        })?;
        Ok(())
    }

    pub async fn set_public(&self, owner: &str, name: &str, public: bool) -> Result<()> {
        let db = self.db_for(owner, name).await?;
        db.put(PUBLIC_KEY, if public { b"1".as_slice() } else { b"0".as_slice() }).await?;
        // The instant a repo goes private, no previously cached answer for it may be served to
        // anyone — including the cached visibility flag itself. Bumping the generation orphans
        // them all at once. The visibility write has already landed, so a failure here is not a
        // no-op to report away: the repo IS private and the cache still says otherwise.
        self.cache.bump_generation(&format!("{owner}/{name}")).await.map_err(|e| {
            err(format!(
                "{e}: {owner}/{name} is now {} in the database but its cached responses are \
                 unchanged; retry with `admin purge-cache {owner}/{name}`",
                if public { "public" } else { "private" }
            ))
        })?;
        Ok(())
    }

    pub async fn set_repo_meta(
        &self,
        owner: &str,
        name: &str,
        description: &str,
        created_by: &str,
        created_at_ms: i64,
    ) -> Result<()> {
        let db = self.db_for(owner, name).await?;
        let mut b = WriteBatch::new();
        b.put(DESCRIPTION_KEY, description.as_bytes());
        b.put(CREATED_BY_KEY, created_by.as_bytes());
        // Last in the batch is cosmetic — the batch is atomic — but keeps the sentinel's
        // ordering intent visible next to the readers that rely on it.
        b.put(CREATED_AT_KEY, created_at_ms.to_string().as_bytes());
        db.write(b).await?;
        Ok(())
    }

    /// One read of the whole of a repo's own account of itself, visibility included, so a caller
    /// gets one coherent answer instead of two round trips that can disagree.
    pub async fn repo_meta(&self, owner: &str, name: &str) -> Result<Option<RepoMeta>> {
        let db = self.db_for(owner, name).await?;
        let Some(at) = db.get(CREATED_AT_KEY).await? else {
            return Ok(None);
        };
        let created_at_ms = String::from_utf8_lossy(&at)
            .parse()
            .map_err(|e| err(format!("{owner}/{name}: bad meta/created_at: {e}")))?;
        let text = |v: Option<slatedb::bytes::Bytes>| {
            v.map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default()
        };
        Ok(Some(RepoMeta {
            description: text(db.get(DESCRIPTION_KEY).await?),
            created_by: text(db.get(CREATED_BY_KEY).await?),
            created_at_ms,
            public: db.get(PUBLIC_KEY).await?.as_deref() == Some(b"1"),
        }))
    }

    /// No cache generation bump, unlike `set_public`: a description authorizes nothing, so a
    /// stale cached copy is wrong text, never wrong access.
    pub async fn set_repo_description(&self, owner: &str, name: &str, description: &str) -> Result<()> {
        self.db_for(owner, name).await?.put(DESCRIPTION_KEY, description.as_bytes()).await?;
        Ok(())
    }

    pub async fn is_public(&self, owner: &str, name: &str) -> Result<bool> {
        Ok(self.db_for(owner, name).await?.get(PUBLIC_KEY).await?.as_deref() == Some(b"1"))
    }

    pub async fn repo_exists(&self, owner: &str, name: &str) -> Result<bool> {
        // Ask the object store first: opening a database creates it, so probing an unknown repo
        // through the pool would conjure one for every mistyped path.
        if !self.repo_db_exists(owner, name).await? {
            return Ok(false);
        }
        Ok(self
            .db_for(owner, name).await?
            .get(repo_key(owner, name))
            .await?
            .is_some())
    }

    /// Exists AND public, in one database open: the git front door asks both questions on every
    /// request, and asking them through `repo_exists` + `is_public` paid two `db_for` resolutions
    /// and three sequential gets. The object-store probe still runs first — `db_for` CREATES a
    /// database for whatever name it is handed, and this is reachable anonymously.
    pub async fn repo_public(&self, owner: &str, name: &str) -> Result<bool> {
        if !self.repo_db_exists(owner, name).await? {
            return Ok(false);
        }
        let db = self.db_for(owner, name).await?;
        let (exists, public) = tokio::join!(db.get(repo_key(owner, name)), db.get(PUBLIC_KEY));
        Ok(exists?.is_some() && public?.as_deref() == Some(b"1"))
    }

    pub async fn get_ref(&self, repo: &Repo, name: &str) -> Result<Option<ObjectId>> {
        match self
            .db_for(&repo.owner, &repo.name).await?
            .get(ref_key(repo, name))
            .await?
        {
            Some(v) => Ok(Some(parse_oid(&v)?)),
            None => Ok(None),
        }
    }

    pub async fn list_refs(&self, repo: &Repo) -> Result<Vec<(String, ObjectId)>> {
        let prefix = ref_prefix(repo);
        let mut it = self
            .db_for(&repo.owner, &repo.name).await?
            .scan_prefix(prefix.as_bytes(), ..)
            .await?;
        let mut out = Vec::new();
        while let Some(kv) = it.next().await? {
            let name = String::from_utf8_lossy(&kv.key[prefix.len()..]).to_string();
            out.push((name, parse_oid(&kv.value)?));
        }
        out.sort();
        Ok(out)
    }

    /// Every protection rule on this repo.
    pub async fn protections(&self, owner: &str, name: &str) -> Result<Vec<Protection>> {
        let prefix = protect_scan(owner, name);
        let db = self.db_for(owner, name).await?;
        let mut it = db.scan_prefix(prefix.as_bytes(), ..).await?;
        let mut out = Vec::new();
        while let Some(kv) = it.next().await? {
            let Some(pattern) = std::str::from_utf8(&kv.key)
                .ok()
                .and_then(|k| k.strip_prefix(prefix.as_str()))
            else {
                continue;
            };
            out.push(Protection::decode(pattern, &kv.value));
        }
        out.sort_by(|a, b| a.pattern.cmp(&b.pattern));
        Ok(out)
    }

    pub async fn set_protection(&self, owner: &str, name: &str, p: &Protection) -> Result<()> {
        if p.pattern.trim().is_empty() {
            return Err(err("a branch pattern is required"));
        }
        // `/` is the key separator, and a pattern carrying one would be
        // indistinguishable from a different repo's key.
        if p.pattern.contains("//") || p.pattern.starts_with('/') {
            return Err(err("that is not a branch pattern"));
        }
        // `matches` honours a trailing `*` and nothing else; a pattern with one elsewhere would
        // be stored, match nothing, and read as protection to whoever wrote it.
        let stem = p.pattern.strip_suffix('*').unwrap_or(&p.pattern);
        if stem.contains('*') {
            return Err(err("only a trailing * is supported in a branch pattern"));
        }
        self.db_for(owner, name)
            .await?
            .put(protect_key(owner, name, &p.pattern), &p.encode())
            .await?;
        Ok(())
    }

    pub async fn remove_protection(&self, owner: &str, name: &str, pattern: &str) -> Result<()> {
        self.db_for(owner, name)
            .await?
            .delete(protect_key(owner, name, pattern))
            .await?;
        Ok(())
    }

    /// All-or-nothing compare-and-swap of refs in one serializable txn, given the protection
    /// verdict for each update already decided.
    ///
    /// Split out of the original `update_refs` (all of `refs.rs`) at its one gix-touching step:
    /// deciding those verdicts needs `protection_verdict`/`is_ancestor` (`gix-traverse`), which
    /// `storage` must not depend on — see this file's module doc. `crate::refs::update_refs` in
    /// the root crate computes the verdicts and calls this. Per update: `None` = applied,
    /// `Some(reason)` = rejected (then nothing is applied).
    pub async fn update_refs_txn(
        &self,
        repo: &Repo,
        updates: &[RefUpdate],
        verdicts: Vec<Option<String>>,
    ) -> Result<Vec<Option<String>>> {
        let txn = self
            .db_for(&repo.owner, &repo.name).await?
            .begin(IsolationLevel::SerializableSnapshot)
            .await?;
        let mut results = Vec::with_capacity(updates.len());
        let mut any_rejected = false;

        for (u, verdict) in updates.iter().zip(verdicts) {
            if let Some(reason) = verdict {
                results.push(Some(reason));
                any_rejected = true;
                continue;
            }
            let key = ref_key(repo, &u.name);
            let cur = match txn.get(&key).await? {
                Some(v) => Some(parse_oid(&v)?),
                None => None,
            };
            if cur != u.old {
                results.push(Some("fetch first".to_string()));
                any_rejected = true;
                continue;
            }
            match u.new {
                Some(n) => txn.put(&key, n.to_hex().to_string().as_bytes())?,
                None => txn.delete(&key)?,
            }
            results.push(None);
        }
        if any_rejected {
            txn.rollback();
            return Ok(results);
        }
        match txn.commit().await {
            Ok(_) => {
                // Done here rather than in the push path so every caller of update_refs is covered.
                // The ref list is the only cached answer a ref move can invalidate; everything else
                // is keyed by an object id. Best effort: drop_refs fails open and a missed drop
                // costs at most the 5s TTL, so this never fails a push.
                self.cache.drop_refs(&format!("{}/{}", repo.owner, repo.name)).await;
                Ok(results)
            }
            // concurrent push touched the same refs -> reject the whole batch
            Err(e) if e.kind() == ErrorKind::Transaction => {
                let msg = format!("conflict: {e}");
                Ok(updates.iter().map(|_| Some(msg.clone())).collect())
            }
            Err(e) => Err(e.into()),
        }
    }
}
