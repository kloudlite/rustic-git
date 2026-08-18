use crate::store::{Repo, Store};
use crate::{err, Result};
use gix_hash::ObjectId;
use slatedb::{ErrorKind, IsolationLevel, WriteBatch};

/// ponytail: fixed default branch; store per-repo when it becomes configurable
pub const DEFAULT_BRANCH: &str = "main";

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
        // Orphans every cached answer for this repo: a name can be recreated, and a hit from the
        // deleted repo's life would be served for the new one.
        self.cache.bump_generation(&format!("{owner}/{name}")).await;
        Ok(())
    }

    pub async fn set_public(&self, owner: &str, name: &str, public: bool) -> Result<()> {
        let db = self.db_for(owner, name).await?;
        db.put(PUBLIC_KEY, if public { b"1".as_slice() } else { b"0".as_slice() }).await?;
        // The instant a repo goes private, no previously cached answer for it may be served to
        // anyone — including the cached visibility flag itself. Bumping the generation orphans
        // them all at once.
        self.cache.bump_generation(&format!("{owner}/{name}")).await;
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

    /// All-or-nothing compare-and-swap of refs in one serializable txn.
    /// Per update: `None` = applied, `Some(reason)` = rejected (then nothing is applied).
    pub async fn update_refs(
        &self,
        repo: &Repo,
        updates: &[RefUpdate],
    ) -> Result<Vec<Option<String>>> {
        let txn = self
            .db_for(&repo.owner, &repo.name).await?
            .begin(IsolationLevel::SerializableSnapshot)
            .await?;
        let mut results = Vec::with_capacity(updates.len());
        let mut any_rejected = false;
        for u in updates {
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
