use crate::{err, Result};
use futures::{StreamExt, TryStreamExt};
use slatedb::object_store::{path::Path as OsPath, ObjectStore, ObjectStoreExt, PutPayload};
use slatedb::Db;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct Store {
    pub os: Arc<dyn ObjectStore>,
    /// Repo databases, opened on demand and kept warm. Which repos reach this node is the load
    /// balancer's decision, so there is nothing to elect here.
    pub pool: Arc<crate::pool::Pool>,
    pub cache_dir: PathBuf,
    /// Credential lookups, cached briefly (see auth.rs).
    pub(crate) auth_cache:
        std::sync::Mutex<std::collections::HashMap<String, (std::time::Instant, Option<String>)>>,
}

pub struct Repo {
    pub owner: String,
    pub name: String,
    pub objects_dir: PathBuf,
    pub pack_dir: PathBuf,
}

impl Repo {
    /// Every repo owns its objects outright. Forks copy rather than share: forks are rare, storage
    /// is cheap, and sharing a pile is what forced garbage collection to see every repo using it —
    /// which in turn constrained where repos could live and made cross-repo object exposure
    /// possible at all.
    pub fn s3_prefix(&self) -> String {
        format!("objects/{}/{}/pack", self.owner, self.name)
    }
    pub fn odb(&self) -> Result<gix_odb::Handle> {
        Ok(gix_odb::at(&self.objects_dir)?)
    }
}

fn pack_index_prefix(owner: &str, name: &str) -> String {
    format!("pack/{owner}/{name}/")
}

pub fn valid_segment(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
}

impl Store {
    /// `background`: run SlateDB's compactor and garbage collector inside each repo database.
    pub async fn open(
        os: Arc<dyn ObjectStore>,
        cache_dir: PathBuf,
        background: bool,
    ) -> Result<Store> {
        std::fs::create_dir_all(&cache_dir)?;
        Ok(Store {
            pool: Arc::new(crate::pool::Pool::new(os.clone(), background)),
            os,
            cache_dir,
            auth_cache: Default::default(),
        })
    }

    /// Whether a repo exists, without creating its database as a side effect of asking.
    pub async fn repo_db_exists(&self, owner: &str, name: &str) -> Result<bool> {
        self.pool.exists(owner, name).await
    }

    /// A repo's database. Every node that serves a repo serves it for reads and writes both.
    pub async fn db_for(&self, owner: &str, name: &str) -> Result<Arc<Db>> {
        self.pool.get(owner, name).await
    }

    /// Ensure local cache mirrors S3 pack list. `Ok(None)` if the repo (or path) does not exist.
    pub async fn open_repo(&self, owner: &str, name: &str) -> Result<Option<Repo>> {
        if !valid_segment(owner) || !valid_segment(name) {
            return Ok(None);
        }
        if !self.repo_exists(owner, name).await? {
            return Ok(None);
        }
        let objects_dir = self.cache_dir.join(owner).join(name).join("objects");
        let pack_dir = objects_dir.join("pack");
        tokio::fs::create_dir_all(&pack_dir).await?;
        tokio::fs::create_dir_all(objects_dir.join("info")).await?; // gix-odb wants a normal objects dir
        let repo = Repo {
            owner: owner.into(),
            name: name.into(),
            objects_dir,
            pack_dir,
        };
        // Which files the repo has comes from the ref store, not from listing the object store:
        // the writer records each pack as it uploads it, so this is a local read instead of a
        // network round trip on every request. It also keeps the pack list consistent with the
        // refs alongside it, since both come from the same database.
        let files = self.pack_index(owner, name).await?;
        // .pack before .idx: gix-odb discovers packs via .idx, so the idx must land last.
        let (packs, idxs): (Vec<_>, Vec<_>) = files
            .into_iter()
            .partition(|(fname, _)| !fname.ends_with(".idx"));
        for batch in [packs, idxs] {
            futures::stream::iter(batch)
                .map(|(fname, size)| self.fetch_pack_file(&repo, fname, size))
                .buffer_unordered(8)
                .try_collect::<Vec<_>>()
                .await?;
        }
        Ok(Some(repo))
    }

    /// The repo's pack files as `(filename, size)`, from the ref store.
    ///
    /// Falls back to listing the object store when the index is empty, which covers repos written
    /// before the index existed; the listing is then recorded so the fallback happens once.
    pub async fn pack_index(&self, owner: &str, name: &str) -> Result<Vec<(String, u64)>> {
        let prefix = pack_index_prefix(owner, name);
        let mut it = self
            .db_for(owner, name).await?
            .scan_prefix(prefix.as_bytes(), ..)
            .await?;
        let mut out = Vec::new();
        while let Some(kv) = it.next().await? {
            let fname = String::from_utf8_lossy(&kv.key[prefix.len()..]).to_string();
            let size = String::from_utf8_lossy(&kv.value).parse().unwrap_or(0);
            out.push((fname, size));
        }
        if !out.is_empty() {
            return Ok(out);
        }
        // no index yet: list once, and record it
        let listing: Vec<_> = self
            .os
            .list(Some(&OsPath::from(format!("objects/{owner}/{name}/pack"))))
            .try_collect()
            .await?;
        for meta in &listing {
            if let Some(f) = meta.location.filename() {
                out.push((f.to_string(), meta.size));
                let _ = self.record_pack(owner, name, f, meta.size).await;
            }
        }
        Ok(out)
    }

    /// Note that a pack file exists, so serving a repo needs no object-store listing.
    pub async fn record_pack(&self, owner: &str, name: &str, fname: &str, size: u64) -> Result<()> {
        self.db_for(owner, name).await?
            .put(
                format!("{}{}", pack_index_prefix(owner, name), fname),
                size.to_string().as_bytes(),
            )
            .await?;
        Ok(())
    }

    pub async fn forget_pack_public(&self, owner: &str, name: &str, fname: &str) -> Result<()> {
        self.forget_pack(owner, name, fname).await
    }

    async fn forget_pack(&self, owner: &str, name: &str, fname: &str) -> Result<()> {
        self.db_for(owner, name).await?
            .delete(format!("{}{}", pack_index_prefix(owner, name), fname))
            .await?;
        Ok(())
    }

    /// Download one pack file unless an identically sized copy is already cached.
    async fn fetch_pack_file(&self, repo: &Repo, fname: String, size: u64) -> Result<()> {
        let pack_dir = &repo.pack_dir;
        let local = pack_dir.join(&fname);
        if local.metadata().map(|m| m.len() == size).unwrap_or(false) {
            return Ok(());
        }
        let key = OsPath::from(format!("{}/{}", repo.s3_prefix(), fname));
        let bytes = self.os.get(&key).await?.bytes().await?;
        // unique per process+call: concurrent opens must not share a temp path
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = pack_dir.join(format!(".{fname}.{}.{seq}.tmp", std::process::id()));
        // fsync the data before the rename: otherwise a host crash can leave a renamed file with
        // the right length but unwritten contents, and the size-only skip above would then serve
        // that corrupt pack forever without re-fetching.
        {
            let f = tokio::fs::File::create(&tmp).await?;
            let mut w = f;
            tokio::io::AsyncWriteExt::write_all(&mut w, &bytes).await?;
            w.sync_all().await?;
        }
        tokio::fs::rename(&tmp, &local).await?;
        Ok(())
    }

    /// Copy every pack from one repo's prefix to another's. Uses the object store's own copy, so
    /// the bytes never travel through this process.
    pub async fn copy_packs(&self, from: &Repo, to: &Repo) -> Result<usize> {
        let src = OsPath::from(from.s3_prefix());
        let metas: Vec<_> = self.os.list(Some(&src)).try_collect().await?;
        // .pack before .idx, as everywhere: a reader must never see an index without its data
        let (packs, idxs): (Vec<_>, Vec<_>) = metas
            .into_iter()
            .partition(|m| m.location.extension() != Some("idx"));
        let mut copied = 0;
        for batch in [packs, idxs] {
            for meta in batch {
                let Some(fname) = meta.location.filename() else {
                    continue;
                };
                let dst = OsPath::from(format!("{}/{}", to.s3_prefix(), fname));
                self.os.copy(&meta.location, &dst).await?;
                self.record_pack(&to.owner, &to.name, fname, meta.size)
                    .await?;
                copied += 1;
            }
        }
        Ok(copied)
    }

    /// Delete every object belonging to a repo. Safe unconditionally now that objects are never
    /// shared between repos.
    pub async fn delete_objects(&self, owner: &str, name: &str) -> Result<()> {
        let prefix = OsPath::from(format!("objects/{owner}/{name}/pack"));
        let locs: Vec<OsPath> = self
            .os
            .list(Some(&prefix))
            .map_ok(|m| m.location)
            .try_collect()
            .await?;
        for loc in locs {
            if let Some(f) = loc.filename() {
                let _ = self.forget_pack(owner, name, f).await;
            }
            self.os.delete(&loc).await?;
        }
        let _ = std::fs::remove_dir_all(self.cache_dir.join(owner).join(name));
        Ok(())
    }

    pub async fn upload_pack_files(&self, repo: &Repo, pack: &Path, idx: &Path) -> Result<()> {
        // pack first, idx last: a concurrent reader must never see an idx without its pack.
        for p in [pack, idx] {
            let fname = p
                .file_name()
                .and_then(|s| s.to_str())
                .ok_or_else(|| err("bad pack path"))?;
            let key = OsPath::from(format!("{}/{}", repo.s3_prefix(), fname));
            let data = tokio::fs::read(p).await?;
            let size = data.len() as u64;
            self.os.put(&key, PutPayload::from(data)).await?;
            // record after the upload, so the index never names a file that is not there yet
            self.record_pack(&repo.owner, &repo.name, fname, size)
                .await?;
        }
        Ok(())
    }
}
