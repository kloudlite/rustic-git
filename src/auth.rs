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

    /// The token's storage name — sha256 hex. Callers keep this to revoke later:
    /// it is what the object key is named after, and it reveals nothing.
    pub fn token_digest(token: &str) -> String {
        Sha256::digest(token.as_bytes()).iter().map(|x| format!("{x:02x}")).collect()
    }

    /// Revoke by digest, so a caller can revoke a token it can no longer read.
    /// Idempotent: revoking twice is not an error, and neither is revoking one the
    /// fleet never had — the desired end state is the same.
    pub async fn revoke_token_digest(&self, digest: &str) -> Result<()> {
        match self.os.delete(&OsPath::from(format!("auth/token/{digest}"))).await {
            Ok(()) | Err(slatedb::object_store::Error::NotFound { .. }) => {}
            Err(e) => return Err(e.into()),
        }
        // The lookup cache holds the old answer for up to CACHE_TTL, on THIS node
        // only. Dropping it here makes revocation immediate for the process that
        // performed it; other nodes still take up to a minute.
        self.auth_cache.lock().unwrap().remove(&format!("auth/token/{digest}"));
        Ok(())
    }

    /// The fingerprint of an OpenSSH public key line, or an error naming what is
    /// wrong with it. Used to validate and identify a key before it is stored.
    pub fn ssh_fingerprint(line: &str) -> Result<String> {
        let key = russh::keys::PublicKey::from_openssh(line.trim())
            .map_err(|_| err("that does not look like an OpenSSH public key"))?;
        Ok(key.fingerprint(russh::keys::HashAlg::Sha256).to_string())
    }

    pub async fn remove_ssh_key(&self, fingerprint: &str) -> Result<()> {
        let key = sshkey_key(fingerprint);
        match self.os.delete(&key).await {
            Ok(()) | Err(slatedb::object_store::Error::NotFound { .. }) => {}
            Err(e) => return Err(e.into()),
        }
        self.auth_cache.lock().unwrap().remove(&key.to_string());
        Ok(())
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

// ponytail: owner-or-public access; add collaborators when needed
/// Public grants READ to everyone — anonymous or authenticated, owner or stranger. It grants
/// identity to nobody: writes and admin still need the owner's credential, which callers express
/// by passing `public_read: false` on any non-read path.
///
/// Callers that assume success implies an identity must keep passing `false` (ssh, proxy): with
/// `public_read` true this can return true for an anonymous caller.
pub fn authorize(auth_owner: Option<&str>, repo_owner: &str, public_read: bool) -> bool {
    public_read || auth_owner == Some(repo_owner)
}
