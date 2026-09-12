//! SSH and signing keys, personal tokens: rows keyed by fingerprint, owned by the person (the
//! 2026-09-07 migration that made that true lives here too).

use super::*;

impl Directory {
    /// Record a credential. `Ok(None)` means this exact credential is already
    /// registered — which for an ssh key means the same key, and is worth saying
    /// rather than silently re-adding.
    pub async fn add_credential(&self, c: &Credential) -> Result<Option<()>> {
        match &self.backend {
            Backend::Mongo(m) => match m.credentials.insert_one(c).await {
                Ok(_) => Ok(Some(())),
                Err(e) if is_duplicate_key(&e) => Ok(None),
                Err(e) => Err(err(format!("mongo: {e}"))),
            },
            Backend::Memory(s) => {
                let mut s = s.lock().unwrap();
                if s.credentials.contains_key(&c.id) {
                    return Ok(None);
                }
                s.credentials.insert(c.id.clone(), c.clone());
                Ok(Some(()))
            }
        }
    }

    pub async fn credentials_for(&self, owner: &str, kind: CredentialKind) -> Result<Vec<Credential>> {
        use futures::TryStreamExt;
        match &self.backend {
            Backend::Mongo(m) => {
                let kind = mongodb::bson::to_bson(&kind).map_err(|e| err(format!("bson: {e}")))?;
                let cursor = m
                    .credentials
                    .find(doc! { "owner": owner, "kind": kind })
                    .sort(doc! { "createdAt": -1 })
                    .limit(super::LISTING_LIMIT)
                    .max_time(super::QUERY_MAX_TIME)
                    .await
                    .map_err(|e| err(format!("mongo: {e}")))?;
                cursor.try_collect().await.map_err(|e| err(format!("mongo: {e}")))
            }
            Backend::Memory(s) => {
                let found = s
                    .lock()
                    .unwrap()
                    .credentials
                    .values()
                    .filter(|c| c.owner == owner && c.kind == kind)
                    .cloned()
                    .collect();
                Ok(newest_first(found, |c| c.created_at))
            }
        }
    }

    /// Look one up to check its owner before revoking it. Revocation is authorized
    /// against the credential's OWNER, not against whoever holds the id — an id is
    /// a digest, and a digest is guessable in principle if the secret is known.
    pub async fn credential(&self, id: &str) -> Result<Option<Credential>> {
        match &self.backend {
            Backend::Mongo(m) => m
                .credentials
                .find_one(doc! { "_id": id })
                .await
                .map_err(|e| err(format!("mongo: {e}"))),
            Backend::Memory(s) => Ok(s.lock().unwrap().credentials.get(id).cloned()),
        }
    }

    /// A signing key by ANY of the fingerprints or key ids it answers to.
    ///
    /// A commit is normally signed by a subkey, and older signatures name their
    /// issuer by key id — the last eight bytes of a fingerprint — rather than
    /// the full fingerprint. Rather than match that as a suffix here (which
    /// would need a scan), `fingerprints_of` stores each key's 16-hex key-id
    /// suffix alongside its full fingerprint at registration, so this stays an
    /// exact, indexed `$in`.
    ///
    /// Attribution is by a FULL fingerprint when the signature carries one; a 16-hex key id is
    /// only what an older signature can offer, and two keys can share one. A candidate set that
    /// matches more than one row names nobody — a wrong "verified" badge is worse than none.
    pub async fn signer_by_any(&self, candidates: &[String]) -> Result<Option<Credential>> {
        use futures::TryStreamExt;
        if candidates.is_empty() {
            return Ok(None);
        }
        let lower: Vec<String> = candidates.iter().map(|c| c.to_lowercase()).collect();
        let full: Vec<String> = lower.iter().filter(|c| c.len() >= 40).cloned().collect();
        let any = if full.is_empty() { lower } else { full };
        let found: Vec<Credential> = match &self.backend {
            Backend::Mongo(m) => {
                let kind = mongodb::bson::to_bson(&CredentialKind::SigningKey)
                    .map_err(|e| err(format!("bson: {e}")))?;
                let any: Vec<mongodb::bson::Bson> = any.into_iter().map(mongodb::bson::Bson::String).collect();
                let cursor = m
                    .credentials
                    .find(doc! { "kind": kind, "fingerprints": { "$in": any } })
                    // Bounded, though a fingerprint names at most a handful of rows: `signer_by_any`
                    // answers `None` on more than one anyway, so an unbounded read could only ever
                    // have cost more to reach the same answer.
                    .limit(super::LISTING_LIMIT)
                    .max_time(super::QUERY_MAX_TIME)
                    .await
                    .map_err(|e| err(format!("mongo: {e}")))?;
                cursor.try_collect().await.map_err(|e| err(format!("mongo: {e}")))?
            }
            Backend::Memory(s) => s
                .lock()
                .unwrap()
                .credentials
                .values()
                .filter(|c| c.kind == CredentialKind::SigningKey && c.fingerprints.iter().any(|f| any.contains(f)))
                .cloned()
                .collect(),
        };
        match found.len() {
            0 => Ok(None),
            1 => Ok(found.into_iter().next()),
            n => {
                tracing::warn!(rows = n, "signer.ambiguous");
                Ok(None)
            }
        }
    }

    pub async fn forget_credential(&self, id: &str) -> Result<()> {
        match &self.backend {
            Backend::Mongo(m) => m
                .credentials
                .delete_one(doc! { "_id": id })
                .await
                .map(|_| ())
                .map_err(|e| err(format!("mongo: {e}"))),
            Backend::Memory(s) => {
                s.lock().unwrap().credentials.remove(id);
                Ok(())
            }
        }
    }

    /// Re-file every key registered against a team onto the person who added it, once.
    ///
    /// A key is the PERSON's now, so a row whose `owner` is a team handle is filed where its
    /// holder can no longer see or revoke it. Marker-guarded rather than run every boot because
    /// the move is a judgement about rows written by an older build — once it has run, a row
    /// under a team handle is a bug to look at, not something to silently rewrite.
    ///
    /// Returns `(email, fingerprint)` for every ssh key, moved or not: the object store's
    /// fingerprint index is a separate store that this cannot write, and re-adding a pair it
    /// already holds costs one PUT.
    ///
    /// Does NOT write the marker — `mark_keys_migrated` does, and only the caller knows whether
    /// the index it is handed these pairs for actually took them. Running the pass twice is
    /// harmless (a moved row is skipped by the same test that selected it, and re-indexing a
    /// pair is one PUT), so the safe order is: pass, index, then mark.
    pub async fn migrate_keys_to_people(&self) -> Result<Vec<(String, String)>> {
        // A row whose `created_by` is not an address cannot be re-filed onto a person, and a
        // fingerprint indexed under a team handle would authenticate the wrong namespace — so
        // both the move and the returned pair insist on one.
        let person = |s: &str| s.contains('@');
        let mut keys = Vec::new();
        match &self.backend {
            Backend::Mongo(m) => {
                use futures::TryStreamExt;
                if m.meta
                    .find_one(doc! { "_id": KEYS_V2 })
                    .await
                    .map_err(|e| err(format!("mongo: {e}")))?
                    .is_some()
                {
                    return Ok(vec![]);
                }
                let kinds = [CredentialKind::SshKey, CredentialKind::SigningKey]
                    .iter()
                    .map(|k| mongodb::bson::to_bson(k).map_err(|e| err(format!("bson: {e}"))))
                    .collect::<Result<Vec<_>>>()?;
                // Drained before a single update: writing to the collection a cursor is
                // streaming lets the server skip or re-yield a rewritten document.
                let rows: Vec<Credential> = m
                    .credentials
                    .find(doc! { "kind": { "$in": kinds } })
                    .await
                    .map_err(|e| err(format!("mongo: {e}")))?
                    .try_collect()
                    .await
                    .map_err(|e| err(format!("mongo: {e}")))?;
                for c in rows {
                    let mut owner = c.owner;
                    if !person(&owner) && person(&c.created_by) {
                        m.credentials
                            .update_one(doc! { "_id": &c.id }, doc! { "$set": { "owner": &c.created_by } })
                            .await
                            .map_err(|e| err(format!("mongo: {e}")))?;
                        owner = c.created_by;
                    }
                    if c.kind == CredentialKind::SshKey && person(&owner) {
                        keys.push((owner, c.id));
                    }
                }
            }
            Backend::Memory(s) => {
                let mut s = s.lock().unwrap();
                if s.meta.contains_key(KEYS_V2) {
                    return Ok(vec![]);
                }
                for c in s.credentials.values_mut() {
                    if !matches!(c.kind, CredentialKind::SshKey | CredentialKind::SigningKey) {
                        continue;
                    }
                    if !person(&c.owner) && person(&c.created_by) {
                        c.owner = c.created_by.clone();
                    }
                    if c.kind == CredentialKind::SshKey && person(&c.owner) {
                        keys.push((c.owner.clone(), c.id.clone()));
                    }
                }
            }
        }
        Ok(keys)
    }

    /// Records that `migrate_keys_to_people`'s pairs have been indexed, so it stops returning
    /// them. Separate from the pass on purpose: until the caller has written every pair into the
    /// fingerprint index, a marker would strand the ones it never got to.
    pub async fn mark_keys_migrated(&self) -> Result<()> {
        match &self.backend {
            // An upsert because two replicas booting together both run the pass, and the loser
            // of that race must not fail a duplicate key into an abandoned boot.
            Backend::Mongo(m) => m
                .meta
                .replace_one(doc! { "_id": KEYS_V2 }, doc! { "_id": KEYS_V2, "at": DateTime::now() })
                .upsert(true)
                .await
                .map(|_| ())
                .map_err(|e| err(format!("mongo: {e}"))),
            Backend::Memory(s) => {
                s.lock().unwrap().meta.insert(KEYS_V2.to_string(), String::new());
                Ok(())
            }
        }
    }

    // ── passkeys ────────────────────────────────────────────────────────────
}
