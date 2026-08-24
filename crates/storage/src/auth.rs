use crate::store::Store;
use crate::Result;
use rand::RngCore;
use sha2::{Digest, Sha256};
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
    let hex = crate::hex(&Sha256::digest(token.as_bytes()));
    OsPath::from(format!("auth/token/{hex}"))
}

fn sshkey_key(fingerprint: &str) -> OsPath {
    // fingerprints contain '/' and '+' (base64); percent-free path segments keep the key flat
    let hex = crate::hex(&Sha256::digest(fingerprint.as_bytes()));
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
        crate::hex(&Sha256::digest(token.as_bytes()))
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

    /// Every git token minted for `owner`, gone — the admin escape hatch for tokens that came
    /// from `admin add-token` and so have no Mongo row to revoke through the api. The token
    /// objects store only the owner, which is all the filter needs; reads are one small GET per
    /// token across the whole fleet's token set, fine at the scale a by-hand command runs at.
    /// Returns how many were revoked.
    pub async fn revoke_tokens_for(&self, owner: &str) -> Result<usize> {
        use futures::TryStreamExt;
        let prefix = OsPath::from("auth/token");
        let metas: Vec<_> = self.os.list(Some(&prefix)).try_collect().await?;
        let mut n = 0;
        for m in metas {
            let body = self.os.get(&m.location).await?.bytes().await?;
            if body.as_ref() == owner.as_bytes() {
                let digest = m.location.filename().unwrap_or_default().to_string();
                self.revoke_token_digest(&digest).await?;
                n += 1;
            }
        }
        Ok(n)
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
        let t = crate::hex(&b);
        self.os
            .put(&token_key(&t), PutPayload::from(owner.to_string()))
            .await?;
        self.auth_cache().remove(&token_key(&t).to_string());
        Ok(t)
    }

    pub async fn owner_for_token(&self, token: &str) -> Result<Option<String>> {
        self.lookup(token_key(token)).await
    }

    /// Registers a public key's fingerprint against `owner`. The caller (`bins/server`, which alone
    /// has the ssh dependency — see `bins/server/src/boot.rs::ssh_fingerprint`) has already parsed the
    /// OpenSSH line and computed the fingerprint; `storage` never parses an ssh key itself, so it
    /// stays free of the ssh-key-parsing dependency.
    pub async fn add_ssh_key(&self, owner: &str, fingerprint: &str) -> Result<()> {
        self.os
            .put(&sshkey_key(fingerprint), PutPayload::from(owner.to_string()))
            .await?;
        self.auth_cache().remove(&sshkey_key(fingerprint).to_string());
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

/// The credential inside an Authorization header of the named scheme, or `None` for another
/// scheme. Matched case-insensitively: RFC 7235 says `basic` and `Basic` are the same scheme,
/// and some proxies lowercase it. One definition, because a call site that spells the match
/// itself is a call site that spells it case-sensitively.
///
/// Header-parsing lives here even though it takes no `axum` type directly (`scheme` itself is
/// axum-free); the sibling functions that DO take `axum::http::HeaderMap` stay in `crates/core`
/// (`crates/core/src/httpx.rs`) — `storage` must not depend on `axum` (see `crates/storage/Cargo.toml`).
pub fn scheme<'a>(v: &'a str, name: &str) -> Option<&'a str> {
    let (head, rest) = v.split_at_checked(name.len())?;
    (head.eq_ignore_ascii_case(name) && rest.starts_with(' ')).then(|| rest.trim_start())
}

/// The judgement half of `basic_user_names` (`rustic-git-core`'s `httpx`) — the header decode lives there since it
/// needs `axum::http::HeaderMap`; this half is pure and has no reason to depend on `axum`.
pub fn user_names(user: &str, owner: &str, git_placeholder: bool) -> bool {
    user == owner || (git_placeholder && user == GIT_PLACEHOLDER)
}

/// git's placeholder username, the shape every token-based git URL uses: `https://x:<token>@host`.
/// The token IS the identity there and git has no other way to send one, so the username carries
/// no information and must not be held against the caller.
const GIT_PLACEHOLDER: &str = "x";

#[cfg(test)]
mod tests {
    use crate::store::Store;
    use std::time::Instant;
    use slatedb::object_store::memory::InMemory;
    use std::sync::Arc;

    /// Revocation is immediate on the node that performed it: the cached hit is dropped with the
    /// object, so a revoked token does not keep working for the rest of the cache TTL here.
    #[tokio::test]
    async fn a_revoked_credential_stops_authenticating_at_once() {
        let os = Arc::new(InMemory::new());
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(os, dir.path().to_path_buf(), false).await.unwrap();
        let token = store.create_token("alice").await.unwrap();
        assert_eq!(store.owner_for_token(&token).await.unwrap().as_deref(), Some("alice"));
        store.revoke_token_digest(&Store::token_digest(&token)).await.unwrap();
        assert_eq!(store.owner_for_token(&token).await.unwrap(), None);
        // Twice is not an error: the desired end state is the same.
        store.revoke_token_digest(&Store::token_digest(&token)).await.unwrap();

        // The ssh-key parsing/fingerprinting itself lives in `crates/api` (needs `russh`); here
        // a stand-in fingerprint string exercises the storage round trip only.
        let fp = "SHA256:test-fingerprint-stand-in";
        store.add_ssh_key("alice", fp).await.unwrap();
        assert_eq!(store.owner_for_fingerprint(fp).await.unwrap().as_deref(), Some("alice"));
        store.remove_ssh_key(fp).await.unwrap();
        assert_eq!(store.owner_for_fingerprint(fp).await.unwrap(), None);
    }

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
    async fn revoke_tokens_for_removes_only_that_owners_tokens() {
        let os = Arc::new(InMemory::new());
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(os, dir.path().to_path_buf(), false).await.unwrap();
        let a1 = s.create_token("alice").await.unwrap();
        let a2 = s.create_token("alice").await.unwrap();
        let b = s.create_token("bob").await.unwrap();
        assert_eq!(s.revoke_tokens_for("alice").await.unwrap(), 2);
        assert_eq!(s.owner_for_token(&a1).await.unwrap(), None);
        assert_eq!(s.owner_for_token(&a2).await.unwrap(), None);
        assert_eq!(s.owner_for_token(&b).await.unwrap().as_deref(), Some("bob"));
        assert_eq!(s.revoke_tokens_for("alice").await.unwrap(), 0, "idempotent");
    }

    #[tokio::test]
    async fn a_key_added_after_a_failed_login_works_immediately() {
        let os = Arc::new(InMemory::new());
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(os, dir.path().to_path_buf(), false).await.unwrap();
        let fp = "SHA256:test-fingerprint-stand-in-2";
        assert_eq!(store.owner_for_fingerprint(fp).await.unwrap(), None);
        store.add_ssh_key("alice", fp).await.unwrap();
        assert_eq!(store.owner_for_fingerprint(fp).await.unwrap().as_deref(), Some("alice"));
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
