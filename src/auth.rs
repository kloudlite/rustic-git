use crate::store::Store;
use crate::{err, Result};
use rand::RngCore;
use russh::keys::ssh_key::sha2::{Digest, Sha256};
use slatedb::object_store::{path::Path as OsPath, ObjectStoreExt, PutPayload};
use std::time::{Duration, Instant};

/// Credentials live as plain object-store keys rather than in SlateDB.
///
/// SlateDB permits one writer per database, so anything stored there belongs to whichever node is
/// serving that repo — but every node has to authenticate every request. Object keys have no such
/// constraint:
/// each credential is an independent key that any node can read and any admin can write.
///
/// Tokens are stored hashed, so neither the bucket nor a leaked listing yields a usable credential,
/// and the lookup compares a digest rather than the secret.
fn token_key(token: &str) -> OsPath {
    let hex: String = Sha256::digest(token.as_bytes())
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect();
    OsPath::from(format!("auth/token/{hex}"))
}

fn sshkey_key(fingerprint: &str) -> OsPath {
    // fingerprints contain '/' and '+' (base64); percent-free path segments keep the key flat
    let hex: String = Sha256::digest(fingerprint.as_bytes())
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect();
    OsPath::from(format!("auth/sshkey/{hex}"))
}

/// How long a credential lookup is reused. Every authenticated request needs one, and an object
/// store round trip is far slower than the request itself; credentials change rarely.
/// The cost is revocation latency: a deleted token keeps working for up to this long.
const CACHE_TTL: Duration = Duration::from_secs(60);

impl Store {
    async fn lookup(&self, key: OsPath) -> Result<Option<String>> {
        let cache_key = key.to_string();
        if let Some((at, v)) = self.auth_cache.lock().unwrap().get(&cache_key) {
            if at.elapsed() < CACHE_TTL {
                return Ok(v.clone());
            }
        }
        let owner = match self.os.get(&key).await {
            Ok(r) => Some(String::from_utf8_lossy(&r.bytes().await?).to_string()),
            Err(slatedb::object_store::Error::NotFound { .. }) => None,
            Err(e) => return Err(e.into()),
        };
        self.auth_cache
            .lock()
            .unwrap()
            .insert(cache_key, (Instant::now(), owner.clone()));
        Ok(owner)
    }

    pub async fn create_token(&self, owner: &str) -> Result<String> {
        let mut b = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut b);
        let t: String = b.iter().map(|x| format!("{x:02x}")).collect();
        self.os
            .put(&token_key(&t), PutPayload::from(owner.to_string()))
            .await?;
        Ok(t)
    }

    pub async fn owner_for_token(&self, token: &str) -> Result<Option<String>> {
        self.lookup(token_key(token)).await
    }

    pub async fn add_ssh_key(&self, owner: &str, line: &str) -> Result<()> {
        let key = russh::keys::PublicKey::from_openssh(line).map_err(|e| err(e.to_string()))?;
        let fp = key.fingerprint(russh::keys::HashAlg::Sha256).to_string();
        self.os
            .put(&sshkey_key(&fp), PutPayload::from(owner.to_string()))
            .await?;
        Ok(())
    }

    pub async fn owner_for_fingerprint(&self, fp: &str) -> Result<Option<String>> {
        self.lookup(sshkey_key(fp)).await
    }
}

// ponytail: owner-only access; add collaborators/public repos when needed
/// Anonymous callers get in only on a public repo, and only for reads — the caller decides
/// whether this is a read by what it passes for `public_read`.
pub fn authorize(auth_owner: Option<&str>, repo_owner: &str, public_read: bool) -> bool {
    match auth_owner {
        Some(o) => o == repo_owner,
        None => public_read,
    }
}
