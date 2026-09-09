//! The mirrored nixpkgs-multiverse index (`index/pkgs/versions.json`), the fallback when Nixhub
//! is down, refreshed daily by `mirror_beat`.

use super::*;


/// One release row of the mirrored index. Normalised on WRITE (the admin beat), so this reader
/// knows exactly one shape.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct MirrorRow {
    pub version: String,
    pub rev: String,
    pub attr_path: String,
}


pub struct Mirror {
    pub os: Arc<dyn ObjectStore>,
    /// The parsed index, good for `CACHE_TTL`. Every `@` entry of every write asks for it, and it
    /// is one multi-megabyte file refreshed once a day — re-fetching and re-parsing it per entry
    /// was pure load.
    cached: tokio::sync::Mutex<Option<(std::time::Instant, Arc<MirrorIndex>)>>,
}


impl Mirror {
    pub fn new(os: Arc<dyn ObjectStore>) -> Self {
        Mirror {
            os,
            cached: tokio::sync::Mutex::new(None),
        }
    }

    /// The newest row for `attr` under the request. `None` = the attribute or the prefix is not in
    /// the mirror at all.
    pub fn pick(index: &MirrorIndex, attr: &str, version: &VersionReq) -> Option<MirrorRow> {
        let rows = index.get(attr)?;
        let under: Vec<&MirrorRow> = match version {
            VersionReq::Latest => rows.iter().collect(),
            // String prefix on purpose: `@20` means releases named 20 or 20.something, never
            // 200.x, and a leading-zero request matches only a literally leading-zero release.
            VersionReq::Prefix(p) => rows
                .iter()
                .filter(|r| r.version == *p || r.version.starts_with(&format!("{p}.")))
                .collect(),
        };
        under
            .into_iter()
            .max_by_key(|r| numeric(&r.version))
            .cloned()
    }

    /// `Ok(None)` = there is no index at all. A region whose refresh beat has never run knows
    /// nothing; it has not FAILED to tell us, so a typo there is still the person's 422 with
    /// Nixhub's nearest versions rather than a 503 blaming the platform.
    ///
    /// A not-found or a failed refresh keeps the copy already parsed, if there is one: a stale
    /// index resolves an older revision, which is slower to build and never wrong.
    async fn index(&self) -> Result<Option<Arc<MirrorIndex>>, String> {
        let mut slot = self.cached.lock().await;
        if let Some((at, idx)) = slot.as_ref() {
            if at.elapsed() < CACHE_TTL {
                return Ok(Some(idx.clone()));
            }
        }
        let previous = || slot.as_ref().map(|(_, i)| i.clone());
        match self.fetch().await {
            Ok(Some(idx)) => {
                let idx = Arc::new(idx);
                *slot = Some((std::time::Instant::now(), idx.clone()));
                Ok(Some(idx))
            }
            Ok(None) => Ok(previous()),
            Err(e) => previous().map(Some).ok_or(e),
        }
    }

    async fn fetch(&self) -> Result<Option<MirrorIndex>, String> {
        let got = match self.os.get(&OsPath::from(MIRROR_KEY)).await {
            Ok(g) => g,
            Err(slatedb::object_store::Error::NotFound { .. }) => return Ok(None),
            Err(e) => return Err(e.to_string()),
        };
        let bytes = got.bytes().await.map_err(|e| e.to_string())?;
        serde_json::from_slice(&bytes).map(Some).map_err(|e| e.to_string())
    }
}


#[async_trait]
impl Index for Mirror {
    async fn resolve(&self, attr: &str, version: &VersionReq) -> Result<Option<Lock>, String> {
        // An UNPARSABLE index is unavailable (see `index`); an absent one is simply a mirror with
        // nothing in it, which is an answer.
        let Some(index) = self.index().await? else {
            return Ok(None);
        };
        Ok(Mirror::pick(&index, attr, version).map(|row| Lock {
            entry: entry_string(attr, version),
            version: row.version,
            attr_path: row.attr_path,
            rev: row.rev,
            store_path: String::new(), // the mirror has none; the agent evaluates `rev` instead
            resolved_at: String::new(),
            source: LockSource::Mirror,
        }))
    }

    async fn versions(&self, attr: &str) -> Result<Vec<String>, String> {
        let Some(index) = self.index().await? else {
            return Ok(Vec::new());
        };
        let mut rows = index.get(attr).cloned().unwrap_or_default();
        rows.sort_by_key(|a| std::cmp::Reverse(numeric(&a.version)));
        Ok(rows.into_iter().map(|r| r.version).collect())
    }
}
