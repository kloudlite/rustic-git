//! WebAuthn passkeys per person, with the counter advance a verification writes back.

use super::*;

impl Directory {
    /// `Ok(None)` means this credential id is already registered — which means the
    /// same authenticator was enrolled twice, not that anything is wrong.
    pub async fn add_passkey(&self, p: &Passkey) -> Result<Option<()>> {
        match &self.backend {
            Backend::Mongo(m) => match m.passkeys.insert_one(p).await {
                Ok(_) => Ok(Some(())),
                Err(e) if is_duplicate_key(&e) => Ok(None),
                Err(e) => Err(err(format!("mongo: {e}"))),
            },
            Backend::Memory(s) => {
                let mut s = s.lock().unwrap();
                if s.passkeys.contains_key(&p.id) {
                    return Ok(None);
                }
                s.passkeys.insert(p.id.clone(), p.clone());
                Ok(Some(()))
            }
        }
    }

    /// By credential id — the lookup a sign-in makes, before it knows who is
    /// signing in. That is the whole point of a discoverable credential: the
    /// authenticator names the account.
    pub async fn passkey(&self, id: &str) -> Result<Option<Passkey>> {
        match &self.backend {
            Backend::Mongo(m) => m
                .passkeys
                .find_one(doc! { "_id": id })
                .await
                .map_err(|e| err(format!("mongo: {e}"))),
            Backend::Memory(s) => Ok(s.lock().unwrap().passkeys.get(id).cloned()),
        }
    }

    pub async fn passkeys_for(&self, user: &str) -> Result<Vec<Passkey>> {
        use futures::TryStreamExt;
        let user = user.trim().to_lowercase();
        match &self.backend {
            Backend::Mongo(m) => {
                let cursor = m
                    .passkeys
                    .find(doc! { "user": user })
                    .limit(super::LISTING_LIMIT)
                    .max_time(super::QUERY_MAX_TIME)
                    .sort(doc! { "createdAt": -1 })
                    .await
                    .map_err(|e| err(format!("mongo: {e}")))?;
                cursor.try_collect().await.map_err(|e| err(format!("mongo: {e}")))
            }
            Backend::Memory(s) => {
                let found = s.lock().unwrap().passkeys.values().filter(|p| p.user == user).cloned().collect();
                Ok(newest_first(found, |p| p.created_at))
            }
        }
    }

    /// Record that a passkey was just used. The counter is what detects a cloned
    /// authenticator, so it is stored on every successful sign-in rather than only
    /// when convenient.
    pub async fn advance_passkey(&self, id: &str, counter: i64) -> Result<()> {
        match &self.backend {
            Backend::Mongo(m) => m
                .passkeys
                .update_one(doc! { "_id": id }, doc! { "$set": { "counter": counter } })
                .await
                .map(|_| ())
                .map_err(|e| err(format!("mongo: {e}"))),
            Backend::Memory(s) => {
                if let Some(p) = s.lock().unwrap().passkeys.get_mut(id) {
                    p.counter = counter;
                }
                Ok(())
            }
        }
    }

    // ── pull requests ───────────────────────────────────────────────────────

    /// The ONLY surviving reader of the Mongo `pulls` collection: `pulls::ensure_migrated` uses
    /// it as its row source, which is what makes pull requests opened before the per-repo
    /// databases existed survive. Nothing else may grow a caller — new pull reads and writes
    /// live in the owning repo's own database.
    pub async fn pulls_for(&self, repo: &str) -> Result<Vec<PullRequest>> {
        use futures::TryStreamExt;
        match &self.backend {
            Backend::Mongo(m) => {
                let cursor = m
                    .pulls
                    .find(doc! { "repo": repo })
                    .limit(super::LISTING_LIMIT)
                    .max_time(super::QUERY_MAX_TIME)
                    .sort(doc! { "createdAt": -1 })
                    .await
                    .map_err(|e| err(format!("mongo: {e}")))?;
                cursor.try_collect().await.map_err(|e| err(format!("mongo: {e}")))
            }
            // Nothing writes these rows any more, so there is nothing to migrate from here.
            Backend::Memory(_) => Ok(vec![]),
        }
    }

    pub async fn forget_passkey(&self, id: &str) -> Result<()> {
        match &self.backend {
            Backend::Mongo(m) => m
                .passkeys
                .delete_one(doc! { "_id": id })
                .await
                .map(|_| ())
                .map_err(|e| err(format!("mongo: {e}"))),
            Backend::Memory(s) => {
                s.lock().unwrap().passkeys.remove(id);
                Ok(())
            }
        }
    }

    // ── superadmins ─────────────────────────────────────────────────────────
}
