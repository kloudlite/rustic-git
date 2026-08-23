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
/// The cost is revocation latency: a deleted token keeps working for up to this long. A miss is
/// cached for the same time, except that registering the credential clears it.
const CACHE_TTL: Duration = Duration::from_secs(60);

/// Entries (hits and misses together) past which a miss sweeps the map: every cached miss and
/// every expired hit is dropped, and if that frees nothing the whole map goes.
const NEG_CAP: usize = 4096;

impl Store {
    /// The credential cache, poisoning ignored: a panic while the lock was held (a bug somewhere
    /// else) must not turn every later authentication into a panic, and the map holds nothing a
    /// half-finished insert can leave inconsistent.
    pub(crate) fn auth_cache(
        &self,
    ) -> std::sync::MutexGuard<'_, std::collections::HashMap<String, (Instant, Option<String>)>> {
        self.auth_cache.lock().unwrap_or_else(|p| p.into_inner())
    }

    async fn lookup(&self, key: OsPath) -> Result<Option<String>> {
        let cache_key = key.to_string();
        if let Some((at, v)) = self.auth_cache().get(&cache_key) {
            if at.elapsed() < CACHE_TTL {
                return Ok(v.clone());
            }
        }
        let owner = match self.os.get(&key).await {
            Ok(r) => Some(String::from_utf8_lossy(&r.bytes().await?).to_string()),
            Err(slatedb::object_store::Error::NotFound { .. }) => None,
            Err(e) => return Err(e.into()),
        };
        // Misses are cached too, or a sprayed bogus credential is one object-store GET each —
        // but bounded: there is an unbounded supply of bogus tokens and none of valid ones, so
        // when the map fills, misses and stale entries are dropped and the (few) live hits kept.
        // Registration evicts the miss for its own key (see `create_token`/`add_ssh_key`), which
        // is what makes "ssh failed, add the key, ssh again" work inside one TTL.
        //
        // The sweep must also drop EXPIRED hits, and clear outright if that was not enough:
        // nothing else ever removes a positive entry, so keeping them unconditionally would let
        // enough accumulated hits pin the map at the cap and hand the miss path back its
        // unbounded growth.
        // ponytail: sweep-on-overflow, not LRU; an LRU crate only if a profile says so.
        let mut cache = self.auth_cache();
        if owner.is_none() && cache.len() >= NEG_CAP {
            cache.retain(|_, (at, v)| v.is_some() && at.elapsed() < CACHE_TTL);
            if cache.len() >= NEG_CAP {
                cache.clear();
            }
        }
        cache.insert(cache_key, (Instant::now(), owner.clone()));
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
        self.auth_cache().remove(&format!("auth/token/{digest}"));
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
        self.auth_cache().remove(&key.to_string());
        Ok(())
    }

    pub async fn create_token(&self, owner: &str) -> Result<String> {
        let mut b = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut b);
        let t: String = b.iter().map(|x| format!("{x:02x}")).collect();
        self.os
            .put(&token_key(&t), PutPayload::from(owner.to_string()))
            .await?;
        self.auth_cache().remove(&token_key(&t).to_string());
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
        self.auth_cache().remove(&sshkey_key(&fp).to_string());
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

#[cfg(test)]
mod tests {
    use crate::store::Store;
    use std::time::Instant;
    use slatedb::object_store::memory::InMemory;
    use std::sync::Arc;

    /// Misses are cached — a sprayed bogus token must not be one object-store GET each — but
    /// bounded, because there is an unbounded supply of bogus tokens and none of valid ones.
    #[tokio::test]
    async fn negative_auth_cache_is_bounded() {
        let os = Arc::new(InMemory::new());
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(os, dir.path().to_path_buf(), false).await.unwrap();
        for i in 0..10_000 {
            let _ = store.owner_for_token(&format!("bogus-token-{i}")).await;
        }
        assert!(store.auth_cache_len() <= super::NEG_CAP, "{}", store.auth_cache_len());
        assert!(store.auth_cache_len() > 0, "misses are cached at all");
    }

    /// Nothing but a sweep ever removes a positive entry, so a fleet that has simply been up a
    /// long time can hold the map at the cap on hits alone. If that pinned the sweep, misses
    /// would grow unbounded again behind a cap that can never be met.
    #[tokio::test]
    async fn a_cache_full_of_hits_does_not_disable_the_cap() {
        let os = Arc::new(InMemory::new());
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(os, dir.path().to_path_buf(), false).await.unwrap();
        {
            let mut c = store.auth_cache();
            for i in 0..super::NEG_CAP + 900 {
                c.insert(format!("auth/token/hit-{i}"), (Instant::now(), Some("alice".into())));
            }
        }
        for i in 0..500 {
            let _ = store.owner_for_token(&format!("bogus-token-{i}")).await;
        }
        assert!(store.auth_cache_len() <= super::NEG_CAP, "{}", store.auth_cache_len());
    }

    /// The common sequence is "ssh fails, add the key, ssh again" — the cached miss must not make
    /// the second attempt fail for another minute.
    #[tokio::test]
    async fn a_key_added_after_a_failed_login_works_immediately() {
        let os = Arc::new(InMemory::new());
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(os, dir.path().to_path_buf(), false).await.unwrap();
        let line = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMOC8YcsFBuWUwnSZkPymFzXnbPlZth+fBP34XGNN+d test@example.com";
        let fp = Store::ssh_fingerprint(line).unwrap();
        assert_eq!(store.owner_for_fingerprint(&fp).await.unwrap(), None);
        store.add_ssh_key("alice", line).await.unwrap();
        assert_eq!(store.owner_for_fingerprint(&fp).await.unwrap().as_deref(), Some("alice"));
    }

    /// One panic while holding the cache lock — a bug anywhere — must not turn every later
    /// authentication into a panic.
    #[tokio::test]
    async fn a_poisoned_auth_cache_does_not_panic_every_request() {
        let os = Arc::new(InMemory::new());
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(os, dir.path().to_path_buf(), false).await.unwrap());
        let token = store.create_token("alice").await.unwrap();
        let s = store.clone();
        let _ = std::thread::spawn(move || {
            let _g = s.auth_cache.lock().unwrap();
            panic!("poison the lock on purpose");
        })
        .join();
        assert!(store.auth_cache.is_poisoned());
        assert_eq!(store.owner_for_token(&token).await.unwrap().as_deref(), Some("alice"));
        store.revoke_token_digest(&Store::token_digest(&token)).await.unwrap();
    }
}
