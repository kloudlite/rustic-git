# kloudlite Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A Rust git server serving Smart HTTP + SSH, storing packs in S3 and refs/auth in embedded SlateDB.

**Architecture:** Every push is stored as a pack (`.pack`+`.idx`) in S3 under `objects/{owner}/{name}/pack/`; each instance mirrors the pack list into a local cache dir and uses `gix_odb` over it for all object access. Refs, repo metadata, tokens and SSH keys are SlateDB keys; ref updates are serializable transactions. Protocol code is synchronous (`BufRead`/`Write`) and runs in `spawn_blocking`; axum and russh are thin adapters.

**Tech Stack:** Rust 2021, tokio, slatedb 0.15 (re-exports object_store 0.14), gix-odb 0.83 / gix-pack 0.73 / gix-object 0.63 / gix-hash 0.26 / gix-traverse 0.60, axum 0.8, russh 0.62 (ssh-key 0.7 rc), flate2, tokio-util, tempfile (dev), sha2 (fingerprint via ssh-key), hex.

**Spec:** `docs/superpowers/specs/2026-08-16-kloudlite-design.md`

## Global Constraints
- Single crate `kloudlite`: `src/lib.rs` (modules) + `src/main.rs` (config, admin CLI, serve).
- Commit messages: no Claude/AI reference.
- Only packs in S3, never loose objects. S3 keys: `objects/{owner}/{name}/pack/{packid}.pack|.idx`. SlateDB path prefix: `slatedb`.
- SlateDB keys: `repo/{owner}/{name}`, `ref/{owner}/{name}/{refname}`, `auth/token/{token}`, `auth/sshkey/{fingerprint}`.
- Repo path in URLs/SSH: `{owner}/{name}.git`. Owner/name regex `[A-Za-z0-9._-]+`, no `..`.
- receive-pack = protocol v0; upload-pack = protocol v2 only.
- No new abstractions: no traits with one impl. Mark cut corners with `// ponytail:` comments.
- Every task ends with `cargo test` green and a commit.
- Tests that need the `git` CLI skip (return early with eprintln) if `git --version` fails.
- Errors: one `pub type Error = Box<dyn std::error::Error + Send + Sync>` and `pub type Result<T> = std::result::Result<T, Error>` in `lib.rs`. No thiserror.

---

## File Structure

```
Cargo.toml
src/lib.rs           mod decls, Error/Result, `pub struct App { store, cache_dir }` shared by http/ssh
src/main.rs          env config, admin subcommands, start http+ssh
src/pktline.rs       read_pkt / write_pkt / write_flush / write_delim / sideband
src/store.rs         Store (object_store + slatedb + cache_dir), key builders, Repo::open (pack sync), upload_pack
src/refs.rs          repo create/exists, get_ref/list_refs/set_refs (txn CAS)
src/auth.rs          token/sshkey lookup+create, authorize()
src/protocol/mod.rs  pub mod receive; pub mod upload; parse_repo_path()
src/protocol/receive.rs  v0 advertise + receive-pack
src/protocol/upload.rs   v2 capabilities, ls-refs, fetch (negotiation + pack build)
src/http.rs          axum router
src/ssh.rs           russh server
tests/common/mod.rs  test App with InMemory store, temp cache dir; git CLI helpers
tests/protocol.rs    receive/upload over in-memory bytes
tests/http_e2e.rs    real git clone/push over HTTP
tests/ssh_e2e.rs     real git clone/push over SSH
```

---

### Task 1: Scaffold crate

**Files:**
- Create: `Cargo.toml`, `src/lib.rs`, `src/main.rs`, `.gitignore`

**Interfaces:**
- Produces: `kloudlite::{Error, Result, App}`; module tree.

- [ ] **Step 1: Cargo.toml**

```toml
[package]
name = "kloudlite"
version = "0.1.0"
edition = "2021"

[lib]
name = "kloudlite"
path = "src/lib.rs"

[[bin]]
name = "kloudlite"
path = "src/main.rs"

[dependencies]
tokio = { version = "1", features = ["full"] }
tokio-util = { version = "0.7", features = ["io", "io-util"] }
slatedb = { version = "0.15", features = ["aws"] }
axum = { version = "0.8", features = ["http1", "tokio", "query"] }
russh = "0.62"
gix-odb = "0.83"
gix-pack = "0.73"
gix-object = "0.63"
gix-hash = "0.26"
gix-traverse = "0.60"
gix-features = { version = "0.49", features = ["progress"] }
flate2 = "1"
bytes = "1"
futures = "0.3"
hex = "0.4"
rand = "0.8"
base64 = "0.22"

[dev-dependencies]
tempfile = "3"
```
Note: if `slatedb` feature `aws` is not a valid feature name, check `~/.cargo/registry/src/*/slatedb-0.15.0/Cargo.toml` `[features]` and use the S3 one; if gix-hash complains about missing hash impl, add `gix-hash = { version = "0.26", features = ["sha1"] }` (verify feature exists in its Cargo.toml).

- [ ] **Step 2: src/lib.rs**

```rust
pub mod auth;
pub mod http;
pub mod pktline;
pub mod protocol;
pub mod refs;
pub mod ssh;
pub mod store;

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;

pub fn err(msg: impl Into<String>) -> Error {
    msg.into().into()
}
```
Create empty files `src/auth.rs src/http.rs src/pktline.rs src/protocol/mod.rs src/refs.rs src/ssh.rs src/store.rs` (mod.rs contents: `pub mod receive; pub mod upload;` with empty `receive.rs`, `upload.rs`).

- [ ] **Step 3: src/main.rs**

```rust
fn main() {
    println!("kloudlite");
}
```

- [ ] **Step 4: .gitignore** — `target/`

- [ ] **Step 5: Build**

Run: `cargo build`
Expected: compiles (warnings ok). Fix feature-flag errors per notes above.

- [ ] **Step 6: Commit** — `git add -A && git commit -m "Scaffold kloudlite crate"`

---

### Task 2: pkt-line codec

**Files:**
- Create: `src/pktline.rs`

**Interfaces:**
- Produces:
  - `pub enum Pkt { Data(Vec<u8>), Flush, Delim, ResponseEnd }`
  - `pub fn read_pkt(r: &mut dyn BufRead) -> io::Result<Option<Pkt>>` (None at EOF)
  - `pub fn write_pkt(w: &mut dyn Write, data: &[u8]) -> io::Result<()>`
  - `pub fn write_text(w: &mut dyn Write, s: &str)` — write_pkt with `\n` appended
  - `pub fn write_flush / write_delim / write_response_end(w)`
  - `pub fn write_band(w: &mut dyn Write, band: u8, data: &[u8])` — sideband-64k, chunks of ≤65515
  - `pub struct BandWriter<'a> { w: &'a mut dyn Write, band: u8 }` implementing `Write` (each write → write_band)

- [ ] **Step 1: Test (in `src/pktline.rs` `#[cfg(test)]`)**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    #[test]
    fn roundtrip() {
        let mut buf = Vec::new();
        write_text(&mut buf, "hello");
        write_flush(&mut buf).unwrap();
        write_delim(&mut buf).unwrap();
        write_pkt(&mut buf, b"raw").unwrap();
        assert_eq!(&buf[..10], b"000ahello\n");
        let mut c = Cursor::new(buf);
        assert!(matches!(read_pkt(&mut c).unwrap(), Some(Pkt::Data(d)) if d == b"hello\n"));
        assert!(matches!(read_pkt(&mut c).unwrap(), Some(Pkt::Flush)));
        assert!(matches!(read_pkt(&mut c).unwrap(), Some(Pkt::Delim)));
        assert!(matches!(read_pkt(&mut c).unwrap(), Some(Pkt::Data(d)) if d == b"raw"));
        assert!(read_pkt(&mut c).unwrap().is_none());
    }
    #[test]
    fn band_chunks() {
        let mut buf = Vec::new();
        write_band(&mut buf, 1, &vec![7u8; 70000]).unwrap();
        let mut c = Cursor::new(buf);
        let mut total = 0;
        while let Some(Pkt::Data(d)) = read_pkt(&mut c).unwrap() { assert_eq!(d[0], 1); total += d.len() - 1; }
        assert_eq!(total, 70000);
    }
}
```

- [ ] **Step 2: Run** `cargo test pktline` → FAIL (unresolved).

- [ ] **Step 3: Implement**

```rust
use std::io::{self, BufRead, Read, Write};

pub enum Pkt { Data(Vec<u8>), Flush, Delim, ResponseEnd }

pub fn read_pkt(r: &mut dyn BufRead) -> io::Result<Option<Pkt>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let s = std::str::from_utf8(&len).map_err(|_| io::Error::other("bad pkt len"))?;
    let n = usize::from_str_radix(s, 16).map_err(|_| io::Error::other("bad pkt len"))?;
    Ok(Some(match n {
        0 => Pkt::Flush,
        1 => Pkt::Delim,
        2 => Pkt::ResponseEnd,
        3 => return Err(io::Error::other("bad pkt len 3")),
        _ => { let mut d = vec![0u8; n - 4]; r.read_exact(&mut d)?; Pkt::Data(d) }
    }))
}

pub fn write_pkt(w: &mut dyn Write, data: &[u8]) -> io::Result<()> {
    write!(w, "{:04x}", data.len() + 4)?;
    w.write_all(data)
}
pub fn write_text(w: &mut dyn Write, s: &str) -> io::Result<()> {
    let mut v = s.as_bytes().to_vec(); v.push(b'\n'); write_pkt(w, &v)
}
pub fn write_flush(w: &mut dyn Write) -> io::Result<()> { w.write_all(b"0000") }
pub fn write_delim(w: &mut dyn Write) -> io::Result<()> { w.write_all(b"0001") }
pub fn write_response_end(w: &mut dyn Write) -> io::Result<()> { w.write_all(b"0002") }

pub const MAX_BAND: usize = 65515;
pub fn write_band(w: &mut dyn Write, band: u8, data: &[u8]) -> io::Result<()> {
    for chunk in data.chunks(MAX_BAND) {
        let mut v = Vec::with_capacity(chunk.len() + 1);
        v.push(band); v.extend_from_slice(chunk);
        write_pkt(w, &v)?;
    }
    Ok(())
}

pub struct BandWriter<'a> { pub w: &'a mut dyn Write, pub band: u8 }
impl Write for BandWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> { write_band(self.w, self.band, buf)?; Ok(buf.len()) }
    fn flush(&mut self) -> io::Result<()> { self.w.flush() }
}

/// Read all Data pkts until a Flush; returns lines (with trailing \n stripped).
pub fn read_lines_until_flush(r: &mut dyn BufRead) -> io::Result<Vec<Vec<u8>>> {
    let mut out = Vec::new();
    loop {
        match read_pkt(r)? {
            Some(Pkt::Data(mut d)) => { if d.last() == Some(&b'\n') { d.pop(); } out.push(d); }
            Some(Pkt::Flush) | None => return Ok(out),
            Some(_) => {}
        }
    }
}
```
(`use std::io::Read` needed for `read_exact` on `dyn BufRead`; adjust imports to silence warnings.)

- [ ] **Step 4: Run** `cargo test pktline` → PASS.
- [ ] **Step 5: Commit** — `git commit -am "Add pkt-line codec"`

---

### Task 3: Store, Repo (pack sync), refs

**Files:**
- Create: `src/store.rs`, `src/refs.rs`, `tests/common/mod.rs`, `tests/store.rs`

**Interfaces:**
- Produces (store.rs):
  ```rust
  pub struct Store { pub os: Arc<dyn ObjectStore>, pub db: Arc<Db>, pub cache_dir: PathBuf }
  impl Store {
      pub async fn open(os: Arc<dyn ObjectStore>, cache_dir: PathBuf) -> Result<Store>; // Db::open("slatedb", os)
      pub async fn open_repo(&self, owner: &str, name: &str) -> Result<Repo>;    // errors if repo missing
      pub async fn upload_pack_files(&self, repo: &Repo, pack: &Path, idx: &Path) -> Result<()>;
  }
  pub struct Repo { pub owner: String, pub name: String, pub pack_dir: PathBuf /* .../objects/pack */, pub objects_dir: PathBuf }
  impl Repo { pub fn odb(&self) -> Result<gix_odb::Handle> /* gix_odb::at(objects_dir) */ ; pub fn key(&self) -> String /* "owner/name" */ }
  pub fn valid_segment(s: &str) -> bool
  ```
- Produces (refs.rs):
  ```rust
  pub struct RefUpdate { pub name: String, pub old: Option<ObjectId>, pub new: Option<ObjectId> } // None old = must not exist; None new = delete
  impl Store {
      pub async fn create_repo(&self, owner:&str, name:&str) -> Result<()>;   // key repo/{o}/{n} = b"{\"default_branch\":\"main\"}"
      pub async fn repo_exists(&self, owner:&str, name:&str) -> Result<bool>;
      pub async fn get_ref(&self, repo:&Repo, name:&str) -> Result<Option<ObjectId>>;
      pub async fn list_refs(&self, repo:&Repo) -> Result<Vec<(String, ObjectId)>>; // sorted by name
      pub async fn default_branch(&self, repo:&Repo) -> Result<String>;
      pub async fn update_refs(&self, repo:&Repo, updates:&[RefUpdate]) -> Result<Vec<Option<String>>>; // per update: None=ok, Some(reason)=rejected; whole batch is one txn — if any rejected, none applied
  }
  ```

- [ ] **Step 1: tests/common/mod.rs**

```rust
#![allow(dead_code)]
use kloudlite::store::Store;
use slatedb::object_store::memory::InMemory;
use std::sync::Arc;

pub struct TestEnv { pub store: Arc<Store>, pub _tmp: tempfile::TempDir }

pub async fn env() -> TestEnv {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(Arc::new(InMemory::new()), tmp.path().join("cache")).await.unwrap();
    TestEnv { store: Arc::new(store), _tmp: tmp }
}

pub fn have_git() -> bool {
    std::process::Command::new("git").arg("--version").output().map(|o| o.status.success()).unwrap_or(false)
}

/// Run git in `dir`, panic on failure, return stdout.
pub fn git(dir: &std::path::Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git").args(args).current_dir(dir)
        .env("GIT_AUTHOR_NAME","t").env("GIT_AUTHOR_EMAIL","t@t").env("GIT_COMMITTER_NAME","t").env("GIT_COMMITTER_EMAIL","t@t")
        .output().unwrap();
    assert!(out.status.success(), "git {:?} failed: {}", args, String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}
```

- [ ] **Step 2: tests/store.rs**

```rust
mod common;
use kloudlite::refs::RefUpdate;
use gix_hash::ObjectId;

#[tokio::test]
async fn repo_and_refs() {
    let e = common::env().await;
    let s = &e.store;
    assert!(!s.repo_exists("a", "r").await.unwrap());
    s.create_repo("a", "r").await.unwrap();
    assert!(s.repo_exists("a", "r").await.unwrap());
    let repo = s.open_repo("a", "r").await.unwrap();
    assert!(repo.pack_dir.is_dir());
    let oid1 = ObjectId::from_hex(b"1111111111111111111111111111111111111111").unwrap();
    let oid2 = ObjectId::from_hex(b"2222222222222222222222222222222222222222").unwrap();
    // create
    let r = s.update_refs(&repo, &[RefUpdate{ name:"refs/heads/main".into(), old:None, new:Some(oid1)}]).await.unwrap();
    assert_eq!(r, vec![None]);
    assert_eq!(s.get_ref(&repo, "refs/heads/main").await.unwrap(), Some(oid1));
    // stale old → rejected
    let r = s.update_refs(&repo, &[RefUpdate{ name:"refs/heads/main".into(), old:Some(oid2), new:Some(oid2)}]).await.unwrap();
    assert!(r[0].is_some());
    // correct old → ok
    let r = s.update_refs(&repo, &[RefUpdate{ name:"refs/heads/main".into(), old:Some(oid1), new:Some(oid2)}]).await.unwrap();
    assert_eq!(r, vec![None]);
    // list
    s.update_refs(&repo, &[RefUpdate{ name:"refs/tags/v1".into(), old:None, new:Some(oid1)}]).await.unwrap();
    let l = s.list_refs(&repo).await.unwrap();
    assert_eq!(l.iter().map(|(n,_)| n.as_str()).collect::<Vec<_>>(), vec!["refs/heads/main","refs/tags/v1"]);
    // delete
    s.update_refs(&repo, &[RefUpdate{ name:"refs/tags/v1".into(), old:Some(oid1), new:None}]).await.unwrap();
    assert_eq!(s.list_refs(&repo).await.unwrap().len(), 1);
    assert_eq!(s.default_branch(&repo).await.unwrap(), "main");
}

#[tokio::test]
async fn pack_sync_roundtrip() {
    let e = common::env().await;
    let s = &e.store;
    s.create_repo("a", "r").await.unwrap();
    let repo = s.open_repo("a", "r").await.unwrap();
    let p = repo.pack_dir.join("pack-abc.pack"); let i = repo.pack_dir.join("pack-abc.idx");
    std::fs::write(&p, b"PACKDATA").unwrap(); std::fs::write(&i, b"IDX").unwrap();
    s.upload_pack_files(&repo, &p, &i).await.unwrap();
    // wipe cache, reopen → files re-downloaded
    std::fs::remove_dir_all(&repo.pack_dir).unwrap();
    let repo2 = s.open_repo("a", "r").await.unwrap();
    assert_eq!(std::fs::read(repo2.pack_dir.join("pack-abc.pack")).unwrap(), b"PACKDATA");
    assert_eq!(std::fs::read(repo2.pack_dir.join("pack-abc.idx")).unwrap(), b"IDX");
}
```

- [ ] **Step 3: Run** `cargo test --test store` → FAIL to compile.

- [ ] **Step 4: Implement src/store.rs**

```rust
use crate::{err, Result};
use futures::TryStreamExt;
use slatedb::object_store::{path::Path as OsPath, ObjectStore, PutPayload};
use slatedb::Db;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct Store { pub os: Arc<dyn ObjectStore>, pub db: Arc<Db>, pub cache_dir: PathBuf }

pub struct Repo { pub owner: String, pub name: String, pub objects_dir: PathBuf, pub pack_dir: PathBuf }

impl Repo {
    pub fn key(&self) -> String { format!("{}/{}", self.owner, self.name) }
    pub fn s3_prefix(&self) -> String { format!("objects/{}/{}/pack", self.owner, self.name) }
    pub fn odb(&self) -> Result<gix_odb::Handle> { Ok(gix_odb::at(&self.objects_dir)?) }
}

pub fn valid_segment(s: &str) -> bool {
    !s.is_empty() && s != "." && s != ".." && s.chars().all(|c| c.is_ascii_alphanumeric() || c=='.' || c=='_' || c=='-')
}

impl Store {
    pub async fn open(os: Arc<dyn ObjectStore>, cache_dir: PathBuf) -> Result<Store> {
        let db = Db::open("slatedb", os.clone()).await?;
        std::fs::create_dir_all(&cache_dir)?;
        Ok(Store { os, db: Arc::new(db), cache_dir })
    }

    /// Ensure local cache mirrors S3 pack list; returns Repo. Errors if repo does not exist.
    pub async fn open_repo(&self, owner: &str, name: &str) -> Result<Repo> {
        if !valid_segment(owner) || !valid_segment(name) { return Err(err("invalid repo path")); }
        if !self.repo_exists(owner, name).await? { return Err(err("repository not found")); }
        let objects_dir = self.cache_dir.join(owner).join(name).join("objects");
        let pack_dir = objects_dir.join("pack");
        std::fs::create_dir_all(&pack_dir)?;
        std::fs::create_dir_all(objects_dir.join("info"))?; // gix-odb wants a normal objects dir
        let repo = Repo { owner: owner.into(), name: name.into(), objects_dir, pack_dir };
        // ponytail: LIST + download on every open; cache listing when it shows in latency
        let prefix = OsPath::from(repo.s3_prefix());
        let mut listing = self.os.list(Some(&prefix));
        while let Some(meta) = listing.try_next().await? {
            let fname = meta.location.filename().ok_or_else(|| err("bad key"))?.to_string();
            let local = repo.pack_dir.join(&fname);
            if local.metadata().map(|m| m.len() == meta.size).unwrap_or(false) { continue; }
            let bytes = self.os.get(&meta.location).await?.bytes().await?;
            let tmp = repo.pack_dir.join(format!(".{}.tmp", fname));
            std::fs::write(&tmp, &bytes)?;
            std::fs::rename(&tmp, &local)?; // idx may land before pack; gix-odb tolerates and refreshes
        }
        Ok(repo)
    }

    pub async fn upload_pack_files(&self, repo: &Repo, pack: &Path, idx: &Path) -> Result<()> {
        for p in [pack, idx] {
            let fname = p.file_name().and_then(|s| s.to_str()).ok_or_else(|| err("bad pack path"))?;
            let key = OsPath::from(format!("{}/{}", repo.s3_prefix(), fname));
            let data = tokio::fs::read(p).await?;
            self.os.put(&key, PutPayload::from(data)).await?;
        }
        Ok(())
    }
}
```
Note: `meta.size` type may be `u64` or `usize` depending on object_store version — cast as the compiler asks. Upload `.pack` before `.idx`? gix-odb discovers packs by `.idx`; upload idx LAST so a concurrent reader listing S3 never sees idx without pack. Order in loop above is `[pack, idx]` — correct. In `open_repo`, sort listing so `.pack` files download before `.idx` (collect metas, sort by extension with `.pack` first).

- [ ] **Step 5: Implement src/refs.rs**

```rust
use crate::store::{Repo, Store};
use crate::{err, Result};
use gix_hash::ObjectId;
use slatedb::config::IsolationLevel;   // adjust path if compiler says (grep "pub enum IsolationLevel" in slatedb src)

pub struct RefUpdate { pub name: String, pub old: Option<ObjectId>, pub new: Option<ObjectId> }

fn ref_key(repo: &Repo, name: &str) -> String { format!("ref/{}/{}", repo.key(), name) }
fn ref_prefix(repo: &Repo) -> String { format!("ref/{}/", repo.key()) }
fn repo_key(owner: &str, name: &str) -> String { format!("repo/{}/{}", owner, name) }

fn parse_oid(b: &[u8]) -> Result<ObjectId> { Ok(ObjectId::from_hex(b).map_err(|e| err(e.to_string()))?) }

impl Store {
    pub async fn create_repo(&self, owner: &str, name: &str) -> Result<()> {
        if !crate::store::valid_segment(owner) || !crate::store::valid_segment(name) { return Err(err("invalid repo path")); }
        self.db.put(repo_key(owner, name), br#"{"default_branch":"main"}"#).await?;
        Ok(())
    }
    pub async fn repo_exists(&self, owner: &str, name: &str) -> Result<bool> {
        Ok(self.db.get(repo_key(owner, name)).await?.is_some())
    }
    pub async fn default_branch(&self, _repo: &Repo) -> Result<String> {
        Ok("main".into()) // ponytail: fixed default branch; parse repo/{..} json when configurable
    }
    pub async fn get_ref(&self, repo: &Repo, name: &str) -> Result<Option<ObjectId>> {
        match self.db.get(ref_key(repo, name)).await? { Some(v) => Ok(Some(parse_oid(&v)?)), None => Ok(None) }
    }
    pub async fn list_refs(&self, repo: &Repo) -> Result<Vec<(String, ObjectId)>> {
        let prefix = ref_prefix(repo);
        let mut it = self.db.scan_prefix(prefix.as_bytes(), ..).await?;
        let mut out = Vec::new();
        while let Some(kv) = it.next().await? {
            let name = String::from_utf8_lossy(&kv.key[prefix.len()..]).to_string();
            out.push((name, parse_oid(&kv.value)?));
        }
        out.sort();
        Ok(out)
    }
    /// All-or-nothing compare-and-swap of refs in one serializable txn.
    pub async fn update_refs(&self, repo: &Repo, updates: &[RefUpdate]) -> Result<Vec<Option<String>>> {
        let txn = self.db.begin(IsolationLevel::SerializableSnapshot).await?;
        let mut results = Vec::with_capacity(updates.len());
        let mut any_rejected = false;
        for u in updates {
            let key = ref_key(repo, &u.name);
            let cur = match txn.get(&key).await? { Some(v) => Some(parse_oid(&v)?), None => None };
            if cur != u.old { results.push(Some("fetch first".to_string())); any_rejected = true; continue; }
            match u.new { Some(n) => txn.put(&key, n.to_hex().to_string().as_bytes())?, None => txn.delete(&key)? }
            results.push(None);
        }
        if any_rejected { txn.rollback(); return Ok(results); }
        match txn.commit().await {
            Ok(_) => Ok(results),
            Err(e) => { // conflict with concurrent push → reject all
                let msg = format!("conflict: {e}");
                Ok(updates.iter().map(|_| Some(msg.clone())).collect())
            }
        }
    }
}
```
Notes: `txn.get` signature — check `db_transaction.rs` (it is `pub async fn get<K: AsRef<[u8]>+Send>(&self, key)`). Distinguish real txn conflict errors from other errors if `slatedb::Error` exposes a variant (grep `TransactionConflict` in `slatedb-0.15.0/src/error.rs`); if it does, only map that variant to rejection and propagate others with `?`.

- [ ] **Step 6: Run** `cargo test --test store` → PASS.
- [ ] **Step 7: Commit** — `git add -A && git commit -m "Add store, repo pack sync and refs"`

---

### Task 4: receive-pack (v0)

**Files:**
- Create: `src/protocol/receive.rs`, `src/protocol/mod.rs` (add helper), `tests/protocol.rs`

**Interfaces:**
- Produces:
  ```rust
  // src/protocol/receive.rs
  pub fn advertise(store:&Store, repo:&Repo, out:&mut dyn Write) -> Result<()>;   // sync; uses Handle::current().block_on
  pub fn serve(store:&Store, repo:&Repo, input:&mut dyn BufRead, out:&mut dyn Write) -> Result<()>;
  // src/protocol/mod.rs
  pub fn block_on<F: std::future::Future>(f: F) -> F::Output  // tokio::runtime::Handle::current().block_on wrapped in block_in_place
  pub const AGENT: &str = "agent=kloudlite/0.1";
  ```
  Callers must invoke `advertise`/`serve` inside `tokio::task::spawn_blocking` on a multi-thread runtime.

- [ ] **Step 1: tests/protocol.rs (receive part)**

```rust
mod common;
use kloudlite::protocol::{receive, upload};
use kloudlite::pktline;
use std::io::{Cursor, Write};

/// Build a local repo with one commit; return (dir, head oid).
fn local_repo() -> (tempfile::TempDir, String) {
    let d = tempfile::tempdir().unwrap();
    common::git(d.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(d.path().join("a.txt"), "hello\n").unwrap();
    common::git(d.path(), &["add", "."]);
    common::git(d.path(), &["commit", "-q", "-m", "one"]);
    let head = common::git(d.path(), &["rev-parse", "HEAD"]);
    (d, head)
}

/// git pack-objects --revs → pack bytes
fn pack_of(dir: &std::path::Path, revs: &str) -> Vec<u8> {
    use std::process::{Command, Stdio};
    let mut c = Command::new("git").args(["pack-objects", "--stdout", "--revs", "-q"]).current_dir(dir)
        .stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    c.stdin.take().unwrap().write_all(revs.as_bytes()).unwrap();
    let out = c.wait_with_output().unwrap();
    assert!(out.status.success());
    out.stdout
}

#[tokio::test(flavor = "multi_thread")]
async fn receive_then_fetch() {
    if !common::have_git() { eprintln!("skip: no git"); return; }
    let e = common::env().await;
    let s = e.store.clone();
    s.create_repo("a", "r").await.unwrap();
    let repo = s.open_repo("a", "r").await.unwrap();
    let (local, head) = local_repo();

    // --- advertise (empty repo)
    let mut adv = Vec::new();
    let s2 = s.clone(); let repo2 = s.open_repo("a","r").await.unwrap();
    tokio::task::spawn_blocking(move || receive::advertise(&s2, &repo2, &mut adv).map(|_| adv)).await.unwrap().unwrap();
    // (content check below after we know the format)

    // --- push
    let mut req = Vec::new();
    pktline::write_pkt(&mut req, format!("{} {} refs/heads/main\0report-status side-band-64k", "0".repeat(40), head).as_bytes()).unwrap();
    pktline::write_flush(&mut req).unwrap();
    req.extend(pack_of(local.path(), &format!("{head}\n")));
    let s2 = s.clone(); let repo2 = s.open_repo("a","r").await.unwrap();
    let resp = tokio::task::spawn_blocking(move || { let mut out = Vec::new(); receive::serve(&s2, &repo2, &mut Cursor::new(req), &mut out).map(|_| out) }).await.unwrap().unwrap();
    // decode sideband band-1 payload
    let mut c = Cursor::new(resp); let mut payload = Vec::new();
    while let Some(p) = pktline::read_pkt(&mut c).unwrap() { if let pktline::Pkt::Data(d) = p { if d[0]==1 { payload.extend_from_slice(&d[1..]); } } }
    let text = String::from_utf8_lossy(&payload).to_string();
    assert!(text.contains("unpack ok"), "{text}");
    assert!(text.contains("ok refs/heads/main"), "{text}");
    let repo = s.open_repo("a", "r").await.unwrap();
    assert_eq!(s.get_ref(&repo, "refs/heads/main").await.unwrap().unwrap().to_hex().to_string(), head);
    // pack landed in S3 (re-open after wiping cache)
    std::fs::remove_dir_all(&repo.pack_dir).unwrap();
    let repo = s.open_repo("a", "r").await.unwrap();
    assert!(std::fs::read_dir(&repo.pack_dir).unwrap().count() >= 2);

    // --- advertise now lists main
    let mut adv = Vec::new();
    let s2 = s.clone(); let repo2 = s.open_repo("a","r").await.unwrap();
    let adv = tokio::task::spawn_blocking(move || receive::advertise(&s2, &repo2, &mut adv).map(|_| adv)).await.unwrap().unwrap();
    assert!(String::from_utf8_lossy(&adv).contains(&format!("{head} refs/heads/main")));

    // --- delete-only push (no pack)
    let mut req = Vec::new();
    pktline::write_pkt(&mut req, format!("{} {} refs/heads/main\0report-status", head, "0".repeat(40)).as_bytes()).unwrap();
    pktline::write_flush(&mut req).unwrap();
    let s2 = s.clone(); let repo2 = s.open_repo("a","r").await.unwrap();
    let resp = tokio::task::spawn_blocking(move || { let mut out = Vec::new(); receive::serve(&s2, &repo2, &mut Cursor::new(req), &mut out).map(|_| out) }).await.unwrap().unwrap();
    assert!(String::from_utf8_lossy(&resp).contains("ok refs/heads/main"));
    assert!(s.get_ref(&repo, "refs/heads/main").await.unwrap().is_none());

    // ---- fetch part added in Task 5 (see below); leave a marker
    let _ = upload::AGENT_LINE; // placeholder reference removed in Task 5
}
```
(In Task 5 the last two lines are replaced with the fetch test; for now delete those two lines so it compiles.)

- [ ] **Step 2: Run** `cargo test --test protocol` → FAIL to compile.

- [ ] **Step 3: src/protocol/mod.rs**

```rust
pub mod receive;
pub mod upload;

pub const AGENT: &str = "agent=kloudlite/0.1";

/// Run a future to completion from sync code inside spawn_blocking.
pub fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(f))
}
```

- [ ] **Step 4: src/protocol/receive.rs**

```rust
use super::{block_on, AGENT};
use crate::pktline::{self, BandWriter, Pkt};
use crate::refs::RefUpdate;
use crate::store::{Repo, Store};
use crate::{err, Result};
use gix_hash::ObjectId;
use std::io::{BufRead, Write};
use std::sync::atomic::AtomicBool;

const CAPS: &str = "report-status delete-refs side-band-64k ofs-delta";

pub fn advertise(store: &Store, repo: &Repo, out: &mut dyn Write) -> Result<()> {
    let refs = block_on(store.list_refs(repo))?;
    let caps = format!("{CAPS} {AGENT}");
    if refs.is_empty() {
        pktline::write_pkt(out, format!("{} capabilities^{{}}\0{caps}\n", "0".repeat(40)).as_bytes())?;
    } else {
        for (i, (name, oid)) in refs.iter().enumerate() {
            if i == 0 { pktline::write_pkt(out, format!("{} {name}\0{caps}\n", oid.to_hex()).as_bytes())?; }
            else { pktline::write_text(out, &format!("{} {name}", oid.to_hex()))?; }
        }
    }
    pktline::write_flush(out)?;
    Ok(())
}

pub fn serve(store: &Store, repo: &Repo, input: &mut dyn BufRead, out: &mut dyn Write) -> Result<()> {
    // 1. commands
    let mut updates = Vec::new();
    let mut client_caps = String::new();
    for line in pktline::read_lines_until_flush(input)? {
        let (cmd, caps) = match line.iter().position(|&b| b == 0) { Some(p) => (&line[..p], Some(&line[p+1..])), None => (&line[..], None) };
        if let Some(c) = caps { client_caps = String::from_utf8_lossy(c).to_string(); }
        let s = String::from_utf8_lossy(cmd).to_string();
        let mut parts = s.split(' ');
        let (old, new, name) = (parts.next().ok_or_else(|| err("bad cmd"))?, parts.next().ok_or_else(|| err("bad cmd"))?, parts.next().ok_or_else(|| err("bad cmd"))?);
        let zero = "0".repeat(40);
        let parse = |h: &str| -> Result<Option<ObjectId>> { if h == zero { Ok(None) } else { Ok(Some(ObjectId::from_hex(h.as_bytes()).map_err(|e| err(e.to_string()))?)) } };
        if !name.starts_with("refs/") || name.contains("..") { return Err(err("bad ref name")); }
        updates.push(RefUpdate { name: name.to_string(), old: parse(old)?, new: parse(new)? });
    }
    if updates.is_empty() { return Ok(()); }
    let sideband = client_caps.split(' ').any(|c| c == "side-band-64k");
    let report = client_caps.split(' ').any(|c| c == "report-status");

    // 2. pack (only if some update creates/moves a ref)
    let mut unpack_status = "ok".to_string();
    let mut fatal: Option<String> = None;
    let needs_pack = updates.iter().any(|u| u.new.is_some());
    if needs_pack {
        // input may have no more bytes if client sends only deletes; peek
        let has_data = input.fill_buf().map(|b| !b.is_empty()).unwrap_or(false);
        if has_data {
            match write_pack(repo, input) {
                Ok(Some((pack, idx))) => { block_on(store.upload_pack_files(repo, &pack, &idx))?; }
                Ok(None) => {}
                Err(e) => { unpack_status = format!("error {e}"); fatal = Some(format!("unpack failed: {e}")); }
            }
        }
    }

    // 3. connectivity: every new tip must exist locally
    let mut results: Vec<Option<String>> = vec![None; updates.len()];
    if fatal.is_none() {
        let odb = repo.odb()?;
        for (i, u) in updates.iter().enumerate() {
            if let Some(n) = u.new { if !gix_object::Exists::exists(&odb, &n) { results[i] = Some("missing necessary objects".into()); } }
        }
        // ponytail: only tips are checked; full reachability walk when we care about clients sending broken packs
        let ok: Vec<&RefUpdate> = updates.iter().enumerate().filter(|(i,_)| results[*i].is_none()).map(|(_,u)| u).collect();
        if !ok.is_empty() {
            let owned: Vec<RefUpdate> = ok.iter().map(|u| RefUpdate{ name:u.name.clone(), old:u.old, new:u.new }).collect();
            let r = block_on(store.update_refs(repo, &owned))?;
            let mut j = 0;
            for i in 0..updates.len() { if results[i].is_none() { results[i] = r[j].clone(); j += 1; } }
        }
    } else {
        for r in results.iter_mut() { *r = Some("unpack failed".into()); }
    }

    // 4. report
    if report {
        let mut body = Vec::new();
        pktline::write_text(&mut body, &format!("unpack {unpack_status}"))?;
        for (u, r) in updates.iter().zip(&results) {
            match r { None => pktline::write_text(&mut body, &format!("ok {}", u.name))?, Some(m) => pktline::write_text(&mut body, &format!("ng {} {}", u.name, m))? }
        }
        pktline::write_flush(&mut body)?;
        if sideband { let mut bw = BandWriter { w: out, band: 1 }; bw.write_all(&body)?; pktline::write_flush(out)?; }
        else { out.write_all(&body)?; }
    }
    if let Some(m) = fatal { if sideband { pktline::write_band(out, 3, m.as_bytes())?; } }
    Ok(())
}

/// Index the incoming pack into repo.pack_dir; returns (pack_path, idx_path), or None if pack was empty.
fn write_pack(repo: &Repo, input: &mut dyn BufRead) -> Result<Option<(std::path::PathBuf, std::path::PathBuf)>> {
    let odb = repo.odb()?;
    let should_interrupt = AtomicBool::new(false);
    let mut progress = gix_features::progress::Discard;
    let opts = gix_pack::bundle::write::Options {
        thread_limit: None,
        iteration_mode: gix_pack::data::input::Mode::Verify,
        index_version: gix_pack::index::Version::V2,
        object_hash: gix_hash::Kind::Sha1,
        alloc_limit_bytes: None,
        compression: Default::default(),
    };
    let outcome = gix_pack::Bundle::write_to_directory(input, Some(&repo.pack_dir), &mut progress, &should_interrupt, Some(odb), opts)?;
    if let Some(k) = outcome.keep_path { let _ = std::fs::remove_file(k); }
    match (outcome.data_path, outcome.index_path) {
        (Some(p), Some(i)) => Ok(Some((p, i))),
        _ => Ok(None),
    }
}
```
Adjust to compiler: `gix_features::progress::Discard` must implement `DynNestedProgress` (if not, use `gix_features::progress::prodash::progress::Discard` or `&mut gix_features::progress::Discard as &mut dyn DynNestedProgress`); `Options` field names verified in `bundle/write/types.rs`; `gix_object::Exists` trait — if absent use `gix_object::Find::try_find(&odb, &n, &mut Vec::new())?.is_some()`. Deleting `.keep`: fine because refs are the source of truth in slatedb, no GC exists.

- [ ] **Step 5: Run** `cargo test --test protocol` → PASS (receive part).
- [ ] **Step 6: Commit** — `git add -A && git commit -m "Add receive-pack"`

---

### Task 5: upload-pack (v2)

**Files:**
- Create: `src/protocol/upload.rs`; extend `tests/protocol.rs`

**Interfaces:**
- Produces:
  ```rust
  pub fn advertise(out:&mut dyn Write) -> Result<()>;   // v2 capability list
  pub fn serve(store:&Store, repo:&Repo, input:&mut dyn BufRead, out:&mut dyn Write) -> Result<()>; // handles one or more commands until EOF
  ```

- [ ] **Step 1: Extend test** — replace the placeholder lines at end of `receive_then_fetch` with:

```rust
    // re-push main so we have something to fetch
    let mut req = Vec::new();
    pktline::write_pkt(&mut req, format!("{} {} refs/heads/main\0report-status", "0".repeat(40), head).as_bytes()).unwrap();
    pktline::write_flush(&mut req).unwrap();
    req.extend(pack_of(local.path(), &format!("{head}\n")));
    let s2 = s.clone(); let repo2 = s.open_repo("a","r").await.unwrap();
    tokio::task::spawn_blocking(move || { let mut out = Vec::new(); receive::serve(&s2, &repo2, &mut Cursor::new(req), &mut out) }).await.unwrap().unwrap();

    // ls-refs
    let mut req = Vec::new();
    pktline::write_text(&mut req, "command=ls-refs").unwrap();
    pktline::write_delim(&mut req).unwrap();
    pktline::write_text(&mut req, "symrefs").unwrap();
    pktline::write_text(&mut req, "ref-prefix refs/heads/").unwrap();
    pktline::write_flush(&mut req).unwrap();
    let s2 = s.clone(); let repo2 = s.open_repo("a","r").await.unwrap();
    let resp = tokio::task::spawn_blocking(move || { let mut out = Vec::new(); upload::serve(&s2, &repo2, &mut Cursor::new(req), &mut out).map(|_| out) }).await.unwrap().unwrap();
    let text = String::from_utf8_lossy(&resp).to_string();
    assert!(text.contains(&format!("{head} refs/heads/main")), "{text}");

    // fetch with done → packfile
    let mut req = Vec::new();
    pktline::write_text(&mut req, "command=fetch").unwrap();
    pktline::write_delim(&mut req).unwrap();
    pktline::write_text(&mut req, "no-progress").unwrap();
    pktline::write_text(&mut req, &format!("want {head}")).unwrap();
    pktline::write_text(&mut req, "done").unwrap();
    pktline::write_flush(&mut req).unwrap();
    let s2 = s.clone(); let repo2 = s.open_repo("a","r").await.unwrap();
    let resp = tokio::task::spawn_blocking(move || { let mut out = Vec::new(); upload::serve(&s2, &repo2, &mut Cursor::new(req), &mut out).map(|_| out) }).await.unwrap().unwrap();
    // parse: sections until "packfile", then band-1 data
    let mut c = Cursor::new(resp); let mut pack = Vec::new(); let mut in_pack = false;
    while let Some(p) = pktline::read_pkt(&mut c).unwrap() {
        if let pktline::Pkt::Data(d) = p { if in_pack { if d[0]==1 { pack.extend_from_slice(&d[1..]); } } else if d == b"packfile\n" { in_pack = true; } }
    }
    assert!(pack.starts_with(b"PACK"));
    // verify with git index-pack in a scratch repo
    let scratch = tempfile::tempdir().unwrap();
    common::git(scratch.path(), &["init", "-q"]);
    let mut c = std::process::Command::new("git").args(["index-pack", "--stdin", "--fix-thin"]).current_dir(scratch.path())
        .stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().unwrap();
    c.stdin.take().unwrap().write_all(&pack).unwrap();
    let out = c.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
```

- [ ] **Step 2: Run** `cargo test --test protocol` → FAIL (upload::serve missing).

- [ ] **Step 3: Implement src/protocol/upload.rs**

```rust
use super::{block_on, AGENT};
use crate::pktline::{self, BandWriter, Pkt};
use crate::store::{Repo, Store};
use crate::{err, Result};
use gix_hash::ObjectId;
use std::io::{BufRead, Write};
use std::sync::atomic::AtomicBool;

pub fn advertise(out: &mut dyn Write) -> Result<()> {
    pktline::write_text(out, "version 2")?;
    pktline::write_text(out, AGENT)?;
    pktline::write_text(out, "ls-refs=unborn")?;
    pktline::write_text(out, "fetch=shallow wait-for-done")?; // ponytail: 'shallow' advertised only because git requires it in list; depth requests are rejected below
    pktline::write_text(out, "object-format=sha1")?;
    pktline::write_flush(out)?;
    Ok(())
}

pub fn serve(store: &Store, repo: &Repo, input: &mut dyn BufRead, out: &mut dyn Write) -> Result<()> {
    loop {
        // command line, then optional capability lines until delim (or flush)
        let cmd = match pktline::read_pkt(input)? { Some(Pkt::Data(d)) => String::from_utf8_lossy(&d).trim_end().to_string(), Some(Pkt::Flush) => continue, None | Some(_) => return Ok(()) };
        let cmd = cmd.strip_prefix("command=").ok_or_else(|| err("expected command="))?.to_string();
        let mut args = Vec::new();
        loop { match pktline::read_pkt(input)? { Some(Pkt::Delim) => { args = read_args(input)?; break; } Some(Pkt::Flush) | None => break, Some(Pkt::Data(_)) => {} /* capability like agent=..., object-format */ Some(Pkt::ResponseEnd) => break } }
        match cmd.as_str() {
            "ls-refs" => ls_refs(store, repo, &args, out)?,
            "fetch" => fetch(store, repo, &args, out)?,
            _ => return Err(err(format!("unknown command {cmd}"))),
        }
    }
}

fn read_args(input: &mut dyn BufRead) -> Result<Vec<String>> {
    Ok(pktline::read_lines_until_flush(input)?.into_iter().map(|l| String::from_utf8_lossy(&l).to_string()).collect())
}

fn ls_refs(store: &Store, repo: &Repo, args: &[String], out: &mut dyn Write) -> Result<()> {
    let symrefs = args.iter().any(|a| a == "symrefs");
    let prefixes: Vec<&str> = args.iter().filter_map(|a| a.strip_prefix("ref-prefix ")).collect();
    let want = |name: &str| prefixes.is_empty() || prefixes.iter().any(|p| name.starts_with(p));
    let refs = block_on(store.list_refs(repo))?;
    let default = block_on(store.default_branch(repo))?;
    let head_target = format!("refs/heads/{default}");
    if want("HEAD") {
        match refs.iter().find(|(n, _)| *n == head_target) {
            Some((_, oid)) => pktline::write_text(out, &if symrefs { format!("{} HEAD symref-target:{head_target}", oid.to_hex()) } else { format!("{} HEAD", oid.to_hex()) })?,
            None => if symrefs { pktline::write_text(out, &format!("unborn HEAD symref-target:{head_target}"))?; },
        }
    }
    for (name, oid) in &refs { if want(name) { pktline::write_text(out, &format!("{} {name}", oid.to_hex()))?; } }
    // ponytail: no 'peel' support (annotated tags not peeled in ls-refs); git works without it
    pktline::write_flush(out)?;
    Ok(())
}

fn fetch(store: &Store, repo: &Repo, args: &[String], out: &mut dyn Write) -> Result<()> {
    let mut wants = Vec::new(); let mut haves = Vec::new(); let mut done = false; let mut no_progress = false;
    for a in args {
        if let Some(h) = a.strip_prefix("want ") { wants.push(ObjectId::from_hex(h.as_bytes()).map_err(|e| err(e.to_string()))?); }
        else if let Some(h) = a.strip_prefix("have ") { haves.push(ObjectId::from_hex(h.as_bytes()).map_err(|e| err(e.to_string()))?); }
        else if a == "done" { done = true; }
        else if a == "no-progress" { no_progress = true; }
        else if a.starts_with("deepen") || a.starts_with("shallow") || a.starts_with("filter") { pktline::write_text(out, &format!("ERR {} not supported", a.split(' ').next().unwrap_or("")))?; return Ok(()); }
        // thin-pack, ofs-delta, include-tag, sideband-all: accepted/ignored
    }
    let _ = no_progress;
    let odb = repo.odb()?;
    let common: Vec<ObjectId> = haves.iter().copied().filter(|h| gix_object::Exists::exists(&odb, h)).collect();
    // ponytail: no ref-in-want, no include-tag; add if clients complain

    if !done {
        pktline::write_text(out, "acknowledgments")?;
        if common.is_empty() { pktline::write_text(out, "NAK")?; pktline::write_flush(out)?; return Ok(()); }
        for c in &common { pktline::write_text(out, &format!("ACK {}", c.to_hex()))?; }
        pktline::write_text(out, "ready")?;
        pktline::write_delim(out)?;
    }
    for w in &wants { if !gix_object::Exists::exists(&odb, w) { pktline::write_text(out, &format!("ERR upload-pack: not our ref {}", w.to_hex()))?; return Ok(()); } }

    pktline::write_text(out, "packfile")?;
    let mut band = BandWriter { w: out, band: 1 };
    write_pack(&odb, wants, common, &mut band)?;
    pktline::write_flush(out)?;
    Ok(())
}

/// Stream a pack containing everything reachable from `wants` and not from `haves`.
fn write_pack(odb: &gix_odb::Handle, wants: Vec<ObjectId>, haves: Vec<ObjectId>, out: &mut dyn Write) -> Result<()> {
    use gix_pack::data::output;
    let interrupt = AtomicBool::new(false);
    // 1. commits to send
    let walk = gix_traverse::commit::Simple::new(wants.iter().copied(), odb.clone()).hide(haves.iter().copied())?;
    let mut ids: Vec<ObjectId> = Vec::new();
    for info in walk { ids.push(info?.id); }
    // wants may also be tags/trees/blobs (not commits): include them as-is
    for w in &wants { if !ids.contains(w) { ids.push(*w); } }
    // 2. count objects (expand trees), then entries, then bytes
    let (counts, _) = output::count::objects_unthreaded(
        odb, &mut ids.into_iter().map(Ok), &gix_features::progress::Discard, &interrupt, output::count::objects::ObjectExpansion::TreeContents,
    )?;
    let num = counts.len() as u32;
    let entries = output::entry::iter_from_counts(
        counts, odb.clone(), Box::new(gix_features::progress::Discard),
        output::entry::iter_from_counts::Options { thread_limit: Some(1), mode: output::entry::iter_from_counts::Mode::PackCopyAndBaseObjects, allow_thin_pack: false, chunk_size: 1000, version: gix_pack::data::Version::V2 },
    );
    // ponytail: PackCopyAndBaseObjects reuses existing deltas but computes no new ones; fine until clones are measurably fat
    let mut writer = output::bytes::FromEntriesIter::new(entries, out, num, gix_pack::data::Version::V2, gix_hash::Kind::Sha1);
    for r in &mut writer { r?; }
    Ok(())
}
```
Notes for the implementer: `Simple::hide` returns Result — check; if `hide` isn't on `Simple` in this version, look for `.with_hidden(...)`/`hidden(...)` in `gix-traverse-0.60.0/src/commit/simple.rs` (line ~335). `output::count::objects_unthreaded` needs `&dyn gix_pack::Find` — `gix_odb::Handle` implements it. `iter_from_counts` Options fields verified at `entry/iter_from_counts.rs:372`. `FromEntriesIter` needs `W: Write` and yields `Result<u64, ..>`. `Mode` variants: check enum in `entry/iter_from_counts.rs`; if `PackCopyAndBaseObjects` is absent, use the variant that reuses pack entries. If the counted-object ordering (haves as tree contents) reintroduces objects the client already has, that only bloats the pack — acceptable v1.

Wants beyond `haves` that are trees/blobs: `Simple::new` returns an error on non-commit tips? If so, partition wants: try_find kind first, only feed commits to Simple; add non-commits directly to `ids`.

- [ ] **Step 4: Run** `cargo test --test protocol` → PASS.
- [ ] **Step 5: Commit** — `git add -A && git commit -m "Add upload-pack v2"`

---

### Task 6: HTTP transport + token auth + e2e

**Files:**
- Create: `src/http.rs`, `src/auth.rs`, `tests/http_e2e.rs`; modify `src/lib.rs` (App)

**Interfaces:**
- Produces:
  ```rust
  // lib.rs
  pub struct App { pub store: Arc<Store> }
  // auth.rs
  impl Store {
      pub async fn create_token(&self, owner:&str) -> Result<String>;             // random 32 hex chars; key auth/token/{t} = owner
      pub async fn owner_for_token(&self, token:&str) -> Result<Option<String>>;
      pub async fn add_ssh_key(&self, owner:&str, openssh_pubkey_line:&str) -> Result<()>;   // key auth/sshkey/{SHA256 fp} = owner
      pub async fn owner_for_fingerprint(&self, fp:&str) -> Result<Option<String>>;
  }
  pub fn authorize(auth_owner: Option<&str>, repo_owner: &str) -> bool  // == ; ponytail: no read-only/public repos
  // http.rs
  pub fn router(app: Arc<App>) -> axum::Router
  ```
  Routes: `GET /{owner}/{name}.git/info/refs?service=git-upload-pack|git-receive-pack`, `POST /{owner}/{name}.git/git-upload-pack`, `POST /{owner}/{name}.git/git-receive-pack`. Basic auth `x:<token>`; missing/invalid → 401 with `WWW-Authenticate: Basic realm="kloudlite"`; owner mismatch → 403; unknown repo → 404.

- [ ] **Step 1: tests/http_e2e.rs**

```rust
mod common;
use std::sync::Arc;

async fn serve(app: Arc<kloudlite::App>) -> u16 {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(l, kloudlite::http::router(app)).await.unwrap(); });
    port
}

#[tokio::test(flavor = "multi_thread")]
async fn clone_push_fetch() {
    if !common::have_git() { eprintln!("skip: no git"); return; }
    let e = common::env().await;
    let s = e.store.clone();
    s.create_repo("alice", "proj").await.unwrap();
    let token = s.create_token("alice").await.unwrap();
    let port = serve(Arc::new(kloudlite::App { store: s.clone() })).await;
    let url = format!("http://x:{token}@127.0.0.1:{port}/alice/proj.git");

    // clone empty
    let w = tempfile::tempdir().unwrap();
    common::git(w.path(), &["clone", "-q", &url, "c1"]);
    let c1 = w.path().join("c1");
    std::fs::write(c1.join("f.txt"), "one\n").unwrap();
    common::git(&c1, &["add", "."]); common::git(&c1, &["commit", "-q", "-m", "one"]);
    common::git(&c1, &["push", "-q", "origin", "HEAD:refs/heads/main"]);
    // second commit + push (incremental pack, thin)
    std::fs::write(c1.join("f.txt"), "one\ntwo\n").unwrap();
    common::git(&c1, &["commit", "-qam", "two"]);
    common::git(&c1, &["push", "-q", "origin", "HEAD:refs/heads/main"]);
    let head = common::git(&c1, &["rev-parse", "HEAD"]);

    // fresh clone sees both commits
    common::git(w.path(), &["clone", "-q", &url, "c2"]);
    let c2 = w.path().join("c2");
    assert_eq!(common::git(&c2, &["rev-parse", "HEAD"]), head);
    assert_eq!(std::fs::read_to_string(c2.join("f.txt")).unwrap(), "one\ntwo\n");
    assert_eq!(common::git(&c2, &["log", "--oneline"]).lines().count(), 2);

    // c2 pushes a branch, c1 fetches (negotiation with haves)
    common::git(&c2, &["checkout", "-qb", "feat"]);
    std::fs::write(c2.join("g.txt"), "g\n").unwrap();
    common::git(&c2, &["add", "."]); common::git(&c2, &["commit", "-qm", "feat"]);
    common::git(&c2, &["push", "-q", "origin", "feat"]);
    common::git(&c1, &["fetch", "-q", "origin"]);
    assert_eq!(common::git(&c1, &["rev-parse", "origin/feat"]), common::git(&c2, &["rev-parse", "HEAD"]));

    // delete branch, force push
    common::git(&c2, &["push", "-q", "origin", "--delete", "feat"]);
    common::git(&c1, &["fetch", "-q", "--prune", "origin"]);
    assert!(std::process::Command::new("git").args(["rev-parse","-q","--verify","origin/feat"]).current_dir(&c1).output().unwrap().status.success() == false);
    common::git(&c1, &["reset", "-q", "--hard", "HEAD~1"]);
    common::git(&c1, &["push", "-q", "-f", "origin", "HEAD:refs/heads/main"]);
    common::git(&c2, &["fetch", "-q", "origin"]);
    assert_eq!(common::git(&c2, &["rev-parse", "origin/main"]), common::git(&c1, &["rev-parse", "HEAD"]));

    // auth: wrong token → fails; other owner → fails
    let bad = format!("http://x:nope@127.0.0.1:{port}/alice/proj.git");
    assert!(!std::process::Command::new("git").args(["ls-remote", &bad]).output().unwrap().status.success());
    let bob = s.create_token("bob").await.unwrap();
    let bobu = format!("http://x:{bob}@127.0.0.1:{port}/alice/proj.git");
    assert!(!std::process::Command::new("git").args(["ls-remote", &bobu]).output().unwrap().status.success());
}
```

- [ ] **Step 2: Run** `cargo test --test http_e2e` → FAIL to compile.

- [ ] **Step 3: lib.rs** — add:
```rust
pub struct App { pub store: std::sync::Arc<store::Store> }
```

- [ ] **Step 4: src/auth.rs**

```rust
use crate::store::Store;
use crate::{err, Result};
use rand::RngCore;

impl Store {
    pub async fn create_token(&self, owner: &str) -> Result<String> {
        let mut b = [0u8; 16]; rand::thread_rng().fill_bytes(&mut b);
        let t = hex::encode(b);
        self.db.put(format!("auth/token/{t}"), owner.as_bytes()).await?;
        Ok(t)
    }
    pub async fn owner_for_token(&self, token: &str) -> Result<Option<String>> {
        Ok(self.db.get(format!("auth/token/{token}")).await?.map(|v| String::from_utf8_lossy(&v).to_string()))
    }
    pub async fn add_ssh_key(&self, owner: &str, line: &str) -> Result<()> {
        let key = russh::keys::PublicKey::from_openssh(line).map_err(|e| err(e.to_string()))?;
        let fp = key.fingerprint(russh::keys::HashAlg::Sha256).to_string();
        self.db.put(format!("auth/sshkey/{fp}"), owner.as_bytes()).await?;
        Ok(())
    }
    pub async fn owner_for_fingerprint(&self, fp: &str) -> Result<Option<String>> {
        Ok(self.db.get(format!("auth/sshkey/{fp}")).await?.map(|v| String::from_utf8_lossy(&v).to_string()))
    }
}

pub fn authorize(auth_owner: Option<&str>, repo_owner: &str) -> bool {
    auth_owner == Some(repo_owner) // ponytail: owner-only access; add collaborators/public when needed
}
```
(`russh::keys` re-exports `ssh_key`; if `PublicKey::from_openssh` path differs, `grep -rn "pub fn from_openssh" ~/.cargo/registry/src/*/ssh-key-0.7*/src/public.rs`.)

- [ ] **Step 5: src/http.rs**

```rust
use crate::protocol::{receive, upload};
use crate::store::Repo;
use crate::App;
use axum::{body::Bytes, extract::{Path, Query, State}, http::{header, HeaderMap, StatusCode}, response::{IntoResponse, Response}, routing::{get, post}, Router};
use base64::Engine;
use std::collections::HashMap;
use std::io::{Cursor, Read};
use std::sync::Arc;

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/{owner}/{name}/info/refs", get(info_refs))
        .route("/{owner}/{name}/git-upload-pack", post(upload_pack))
        .route("/{owner}/{name}/git-receive-pack", post(receive_pack))
        .with_state(app)
}

fn strip_git(name: &str) -> &str { name.strip_suffix(".git").unwrap_or(name) }

fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, [(header::WWW_AUTHENTICATE, "Basic realm=\"kloudlite\"")], "auth required").into_response()
}

async fn open(app: &App, headers: &HeaderMap, owner: &str, name: &str) -> Result<Repo, Response> {
    let token = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Basic "))
        .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok())
        .and_then(|d| String::from_utf8(d).ok())
        .and_then(|s| s.split_once(':').map(|(_, p)| p.to_string()));
    let Some(token) = token else { return Err(unauthorized()); };
    let auth_owner = app.store.owner_for_token(&token).await.map_err(internal)?;
    if auth_owner.is_none() { return Err(unauthorized()); }
    if !crate::auth::authorize(auth_owner.as_deref(), owner) { return Err(StatusCode::FORBIDDEN.into_response()); }
    app.store.open_repo(owner, strip_git(name)).await.map_err(|e| (StatusCode::NOT_FOUND, e.to_string()).into_response())
}

fn internal(e: crate::Error) -> Response { (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response() }

fn body_reader(headers: &HeaderMap, body: Bytes) -> Box<dyn Read + Send> {
    if headers.get(header::CONTENT_ENCODING).map(|v| v == "gzip").unwrap_or(false) {
        Box::new(flate2::read::GzDecoder::new(Cursor::new(body)))
    } else { Box::new(Cursor::new(body)) }
}

async fn info_refs(State(app): State<Arc<App>>, Path((owner, name)): Path<(String, String)>, Query(q): Query<HashMap<String, String>>, headers: HeaderMap) -> Response {
    let repo = match open(&app, &headers, &owner, &name).await { Ok(r) => r, Err(r) => return r };
    let service = q.get("service").cloned().unwrap_or_default();
    let v2 = headers.get("git-protocol").and_then(|v| v.to_str().ok()).map(|v| v.contains("version=2")).unwrap_or(false);
    let store = app.store.clone();
    let res = tokio::task::spawn_blocking(move || -> crate::Result<Vec<u8>> {
        let mut out = Vec::new();
        match service.as_str() {
            "git-upload-pack" => {
                if !v2 { return Err(crate::err("protocol v2 required")); }
                upload::advertise(&mut out)?;
            }
            "git-receive-pack" => {
                crate::pktline::write_text(&mut out, "# service=git-receive-pack")?;
                crate::pktline::write_flush(&mut out)?;
                receive::advertise(&store, &repo, &mut out)?;
            }
            _ => return Err(crate::err("unknown service")),
        }
        Ok(out)
    }).await.unwrap();
    match res {
        Ok(out) => ([(header::CONTENT_TYPE, format!("application/x-{service}-advertisement")), (header::CACHE_CONTROL, "no-cache".into())], out).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

async fn upload_pack(State(app): State<Arc<App>>, Path((owner, name)): Path<(String, String)>, headers: HeaderMap, body: Bytes) -> Response {
    let repo = match open(&app, &headers, &owner, &name).await { Ok(r) => r, Err(r) => return r };
    let store = app.store.clone();
    let mut input = std::io::BufReader::new(body_reader(&headers, body));
    // ponytail: whole request/response buffered in memory; stream when repos get big
    let res = tokio::task::spawn_blocking(move || { let mut out = Vec::new(); upload::serve(&store, &repo, &mut input, &mut out).map(|_| out) }).await.unwrap();
    respond("application/x-git-upload-pack-result", res)
}

async fn receive_pack(State(app): State<Arc<App>>, Path((owner, name)): Path<(String, String)>, headers: HeaderMap, body: Bytes) -> Response {
    let repo = match open(&app, &headers, &owner, &name).await { Ok(r) => r, Err(r) => return r };
    let store = app.store.clone();
    let mut input = std::io::BufReader::new(body_reader(&headers, body));
    let res = tokio::task::spawn_blocking(move || { let mut out = Vec::new(); receive::serve(&store, &repo, &mut input, &mut out).map(|_| out) }).await.unwrap();
    respond("application/x-git-receive-pack-result", res)
}

fn respond(ct: &'static str, res: crate::Result<Vec<u8>>) -> Response {
    match res {
        Ok(out) => ([(header::CONTENT_TYPE, ct), (header::CACHE_CONTROL, "no-cache")], out).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}
```
Notes: axum 0.8 path syntax is `/{owner}/{name}`; the `.git` suffix arrives inside `name`. Set `DefaultBodyLimit::disable()` on the router (`.layer(axum::extract::DefaultBodyLimit::disable())`) so pushes >2MB work. The `Read` for `spawn_blocking` must be `Send` — `Box<dyn Read + Send>` is.

- [ ] **Step 6: Run** `cargo test --test http_e2e` → PASS. If git complains during clone about v2, check `git -c protocol.version=2` default (git ≥ 2.26); test env may set `GIT_PROTOCOL`? Not needed — git sends `Git-Protocol: version=2` header by default. Debug with `GIT_TRACE_PACKET=1`.
- [ ] **Step 7: Run all** `cargo test` → PASS.
- [ ] **Step 8: Commit** — `git add -A && git commit -m "Add HTTP transport and token auth"`

---

### Task 7: SSH transport

**Files:**
- Create: `src/ssh.rs`, `tests/ssh_e2e.rs`

**Interfaces:**
- Produces: `pub async fn serve(app: Arc<App>, listener: tokio::net::TcpListener, host_key: russh::keys::PrivateKey) -> Result<()>` — runs forever.
- Behaviour: pubkey auth → owner (via `owner_for_fingerprint`); `exec` of `git-upload-pack '<path>'` / `git-receive-pack '<path>'` (path `owner/name.git`, optional leading `/`); env `GIT_PROTOCOL=version=2` honoured (upload-pack requires v2 → if absent, reply ERR); channel stdin/stdout bridged to sync protocol fns via `tokio_util::io::SyncIoBridge`; exit status 0/1; then EOF+close.

- [ ] **Step 1: tests/ssh_e2e.rs**

```rust
mod common;
use std::sync::Arc;

#[tokio::test(flavor = "multi_thread")]
async fn ssh_clone_push() {
    if !common::have_git() || std::process::Command::new("ssh").arg("-V").output().is_err() { eprintln!("skip"); return; }
    let e = common::env().await;
    let s = e.store.clone();
    s.create_repo("alice", "proj").await.unwrap();
    // client key
    let kd = tempfile::tempdir().unwrap();
    let key = kd.path().join("id_ed25519");
    assert!(std::process::Command::new("ssh-keygen").args(["-q","-t","ed25519","-N","","-f", key.to_str().unwrap()]).status().unwrap().success());
    let pubkey = std::fs::read_to_string(kd.path().join("id_ed25519.pub")).unwrap();
    s.add_ssh_key("alice", &pubkey).await.unwrap();

    let host_key = russh::keys::PrivateKey::random(&mut rand::thread_rng(), russh::keys::Algorithm::Ed25519).unwrap();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    let app = Arc::new(kloudlite::App { store: s.clone() });
    tokio::spawn(async move { kloudlite::ssh::serve(app, l, host_key).await.unwrap(); });

    let ssh_cmd = format!("ssh -i {} -p {port} -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o IdentitiesOnly=yes", key.display());
    let w = tempfile::tempdir().unwrap();
    let git = |dir: &std::path::Path, args: &[&str]| {
        let out = std::process::Command::new("git").args(args).current_dir(dir).env("GIT_SSH_COMMAND", &ssh_cmd)
            .env("GIT_AUTHOR_NAME","t").env("GIT_AUTHOR_EMAIL","t@t").env("GIT_COMMITTER_NAME","t").env("GIT_COMMITTER_EMAIL","t@t").output().unwrap();
        assert!(out.status.success(), "git {:?}: {}", args, String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    let url = format!("ssh://git@127.0.0.1:{port}/alice/proj.git");
    git(w.path(), &["clone", "-q", &url, "c1"]);
    let c1 = w.path().join("c1");
    std::fs::write(c1.join("f.txt"), "one\n").unwrap();
    git(&c1, &["add", "."]); git(&c1, &["commit", "-qm", "one"]);
    git(&c1, &["push", "-q", "origin", "HEAD:refs/heads/main"]);
    git(w.path(), &["clone", "-q", &url, "c2"]);
    assert_eq!(git(&w.path().join("c2"), &["rev-parse", "HEAD"]), git(&c1, &["rev-parse", "HEAD"]));
}
```

- [ ] **Step 2: Run** → FAIL to compile.

- [ ] **Step 3: src/ssh.rs**

```rust
use crate::protocol::{receive, upload};
use crate::{App, Result};
use russh::keys::{HashAlg, PrivateKey, PublicKey};
use russh::server::{Auth, Handler, Msg, Server, Session};
use russh::{Channel, ChannelId, MethodSet};
use std::collections::HashMap;
use std::sync::Arc;
use tokio_util::io::SyncIoBridge;

pub async fn serve(app: Arc<App>, listener: tokio::net::TcpListener, host_key: PrivateKey) -> Result<()> {
    let config = Arc::new(russh::server::Config {
        keys: vec![host_key],
        methods: MethodSet::from_iter([russh::MethodKind::PublicKey]),   // adjust to actual API (grep MethodSet in russh src)
        inactivity_timeout: Some(std::time::Duration::from_secs(600)),
        ..Default::default()
    });
    let mut srv = SshServer { app };
    srv.run_on_socket(config, &listener).await?;
    Ok(())
}

struct SshServer { app: Arc<App> }
impl Server for SshServer {
    type Handler = Conn;
    fn new_client(&mut self, _: Option<std::net::SocketAddr>) -> Conn { Conn { app: self.app.clone(), owner: None, channels: HashMap::new(), env_v2: false } }
}

struct Conn { app: Arc<App>, owner: Option<String>, channels: HashMap<ChannelId, Channel<Msg>>, env_v2: bool }

impl Handler for Conn {
    type Error = crate::Error;

    async fn auth_publickey(&mut self, _user: &str, key: &PublicKey) -> std::result::Result<Auth, Self::Error> {
        let fp = key.fingerprint(HashAlg::Sha256).to_string();
        match self.app.store.owner_for_fingerprint(&fp).await? {
            Some(o) => { self.owner = Some(o); Ok(Auth::Accept) }
            None => Ok(Auth::reject()),
        }
    }
    async fn channel_open_session(&mut self, channel: Channel<Msg>, session: &mut Session) -> std::result::Result<bool, Self::Error> {
        self.channels.insert(channel.id(), channel);
        Ok(true)
    }
    async fn env_request(&mut self, _c: ChannelId, name: &str, value: &str, _s: &mut Session) -> std::result::Result<(), Self::Error> {
        if name == "GIT_PROTOCOL" && value.contains("version=2") { self.env_v2 = true; }
        Ok(())
    }
    async fn exec_request(&mut self, id: ChannelId, data: &[u8], session: &mut Session) -> std::result::Result<(), Self::Error> {
        let cmd = String::from_utf8_lossy(data).to_string();
        let Some(channel) = self.channels.remove(&id) else { return Ok(()); };
        let (service, path) = parse_cmd(&cmd).ok_or_else(|| crate::err("bad command"))?;
        let (owner, name) = crate::protocol::parse_repo_path(&path).ok_or_else(|| crate::err("bad path"))?;
        session.channel_success(id)?;
        let handle = session.handle();
        let app = self.app.clone();
        let auth_owner = self.owner.clone();
        let v2 = self.env_v2;
        tokio::spawn(async move {
            let status = run(app, auth_owner, service, owner, name, v2, channel).await;
            let code = match status { Ok(()) => 0, Err(e) => { let _ = handle.data(id, format!("{:04x}ERR {e}", 4 + 4 + e.to_string().len()).into_bytes().into()).await; 1 } };
            let _ = handle.exit_status_request(id, code).await;
            let _ = handle.eof(id).await;
            let _ = handle.close(id).await;
        });
        Ok(())
    }
}

/// "git-upload-pack 'owner/name.git'" → ("git-upload-pack", "owner/name.git")
fn parse_cmd(cmd: &str) -> Option<(String, String)> {
    let cmd = cmd.trim();
    let (svc, rest) = cmd.split_once(' ')?;
    if svc != "git-upload-pack" && svc != "git-receive-pack" { return None; }
    let path = rest.trim().trim_matches('\'').trim_matches('"').trim_start_matches('/');
    Some((svc.to_string(), path.to_string()))
}

async fn run(app: Arc<App>, auth_owner: Option<String>, service: String, owner: String, name: String, v2: bool, channel: Channel<Msg>) -> Result<()> {
    if !crate::auth::authorize(auth_owner.as_deref(), &owner) { return Err(crate::err("access denied")); }
    let repo = app.store.open_repo(&owner, &name).await?;
    let store = app.store.clone();
    let stream = channel.into_stream();
    let (rd, wr) = tokio::io::split(stream);
    let mut input = std::io::BufReader::new(SyncIoBridge::new(rd));
    let mut output = SyncIoBridge::new(wr);
    tokio::task::spawn_blocking(move || -> Result<()> {
        use std::io::Write;
        match service.as_str() {
            "git-upload-pack" => {
                if !v2 { return Err(crate::err("protocol v2 required")); }
                upload::advertise(&mut output)?;
                upload::serve(&store, &repo, &mut input, &mut output)?;
            }
            _ => {
                receive::advertise(&store, &repo, &mut output)?;
                receive::serve(&store, &repo, &mut input, &mut output)?;
            }
        }
        output.flush()?;
        Ok(())
    }).await??;
    Ok(())
}
```
Add to `src/protocol/mod.rs`:
```rust
/// "owner/name.git" or "owner/name" → (owner, name)
pub fn parse_repo_path(p: &str) -> Option<(String, String)> {
    let p = p.trim_start_matches('/');
    let (o, n) = p.split_once('/')?;
    let n = n.strip_suffix(".git").unwrap_or(n);
    if !crate::store::valid_segment(o) || !crate::store::valid_segment(n) { return None; }
    Some((o.to_string(), n.to_string()))
}
```
Implementer notes: exact russh 0.62 signatures — check `russh-0.62.6/src/server/mod.rs` for `channel_open_session` (it may take `reply: ChannelOpenHandle` — then call `reply.accept()`/equivalent instead of returning bool), `env_request`, `exec_request`, `Session::handle()`, `Handle::{data, exit_status_request, eof, close}` (`server/session.rs:152-194`), and `Config.methods` type. `Channel::into_stream()` exists (`channels/mod.rs:661`). If `into_stream` isn't AsyncRead+AsyncWrite splittable, use `channel.make_reader()` + `channel.make_writer()` and hold the channel in the task. In v2 over SSH, the server must send the capability advertisement first — done in `run` before `serve`. Note git sends the whole v2 request only after reading the advertisement; SyncIoBridge handles that.

- [ ] **Step 4: Run** `cargo test --test ssh_e2e` → PASS. Debug with `GIT_TRACE_PACKET=1` / `ssh -vvv` if it hangs (usual culprits: not sending `channel_success`, not flushing, or not closing the channel).
- [ ] **Step 5: Commit** — `git add -A && git commit -m "Add SSH transport"`

---

### Task 8: main.rs — config, admin CLI, serve

**Files:**
- Modify: `src/main.rs`; create `README.md`

**Interfaces:**
- CLI:
  - `kloudlite serve` — env: `KLOUDLITE_S3_URL` (e.g. `s3://bucket`; `object_store` picks up `AWS_*` env vars; use `mem://` for testing), `KLOUDLITE_CACHE_DIR` (default `./cache`), `KLOUDLITE_HTTP_ADDR` (default `0.0.0.0:8080`), `KLOUDLITE_SSH_ADDR` (default `0.0.0.0:2222`), `KLOUDLITE_HOST_KEY` (path to OpenSSH private key; generated at that path if missing).
  - `kloudlite admin create-repo <owner>/<name>`
  - `kloudlite admin add-token <owner>` → prints token
  - `kloudlite admin add-key <owner> <pubkey-file>`

- [ ] **Step 1: src/main.rs**

```rust
use kloudlite::{store::Store, App, Result};
use std::sync::Arc;

fn env(k: &str, d: &str) -> String { std::env::var(k).unwrap_or_else(|_| d.to_string()) }

async fn open_store() -> Result<Arc<Store>> {
    let url = env("KLOUDLITE_S3_URL", "mem://");
    let os: Arc<dyn slatedb::object_store::ObjectStore> = if url == "mem://" { Arc::new(slatedb::object_store::memory::InMemory::new()) } else { slatedb::Db::resolve_object_store(&url)? };
    Ok(Arc::new(Store::open(os, env("KLOUDLITE_CACHE_DIR", "./cache").into()).await?))
}

fn host_key(path: &str) -> Result<russh::keys::PrivateKey> {
    let p = std::path::Path::new(path);
    if p.exists() { return Ok(russh::keys::PrivateKey::read_openssh_file(p)?); }
    let k = russh::keys::PrivateKey::random(&mut rand::thread_rng(), russh::keys::Algorithm::Ed25519)?;
    k.write_openssh_file(p, russh::keys::LineEnding::LF)?;
    Ok(k)
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let a: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let store = open_store().await?;
    match a.as_slice() {
        ["serve"] => {
            let app = Arc::new(App { store });
            let http = tokio::net::TcpListener::bind(env("KLOUDLITE_HTTP_ADDR", "0.0.0.0:8080")).await?;
            let ssh = tokio::net::TcpListener::bind(env("KLOUDLITE_SSH_ADDR", "0.0.0.0:2222")).await?;
            let key = host_key(&env("KLOUDLITE_HOST_KEY", "./host_key"))?;
            eprintln!("http on {} ssh on {}", http.local_addr()?, ssh.local_addr()?);
            let a2 = app.clone();
            tokio::select! {
                r = axum::serve(http, kloudlite::http::router(a2)) => { r?; }
                r = kloudlite::ssh::serve(app, ssh, key) => { r?; }
            }
        }
        ["admin", "create-repo", path] => { let (o, n) = path.split_once('/').ok_or("owner/name")?; store.create_repo(o, n).await?; }
        ["admin", "add-token", owner] => { println!("{}", store.create_token(owner).await?); }
        ["admin", "add-key", owner, file] => { store.add_ssh_key(owner, &std::fs::read_to_string(file)?).await?; }
        _ => { eprintln!("usage: kloudlite serve | admin create-repo <owner>/<name> | admin add-token <owner> | admin add-key <owner> <pubkey-file>"); std::process::exit(2); }
    }
    store.db.close().await?;
    Ok(())
}
```
Note: `slatedb::Db::resolve_object_store` exists (`db.rs:1883`); it needs slatedb's S3 feature enabled (Task 1). `PrivateKey::write_openssh_file` signature: check `ssh-key/src/private.rs:438` for the `LineEnding` arg.

- [ ] **Step 2: Build & smoke** — `cargo build`; `KLOUDLITE_S3_URL=mem:// cargo run -- admin add-token alice` prints a token (in mem mode the DB is discarded, that's fine for smoke).

- [ ] **Step 3: README.md** — 20 lines: what it is, env vars, the three admin commands, and `git clone http://x:<token>@host:8080/owner/name.git` / `ssh://git@host:2222/owner/name.git`.

- [ ] **Step 4: Run** `cargo test` → all PASS. `cargo clippy` warnings fixed if trivial.
- [ ] **Step 5: Commit** — `git add -A && git commit -m "Add CLI, config and README"`

---

## Self-review notes
- Spec coverage: storage (T3), receive (T4), upload (T5), HTTP+auth (T6), SSH (T7), admin CLI+config (T8), tests per task. Spec's "connectivity check" is reduced to tip-existence (marked `ponytail:`); "LRU cache" removed by revision 1; "unpack ≤100 to loose" removed by revision 1.
- Type consistency: `Store::{open, open_repo, upload_pack_files, create_repo, repo_exists, get_ref, list_refs, default_branch, update_refs, create_token, owner_for_token, add_ssh_key, owner_for_fingerprint}`; `Repo::{owner,name,objects_dir,pack_dir,key(),s3_prefix(),odb()}`; `RefUpdate{name,old,new}`; `receive::{advertise(store,repo,out), serve(store,repo,input,out)}`; `upload::{advertise(out), serve(store,repo,input,out)}`; `protocol::{block_on, AGENT, parse_repo_path}`; `http::router(Arc<App>)`; `ssh::serve(Arc<App>, TcpListener, PrivateKey)`; `App{store}`.
- Known API-uncertainty points are flagged inline ("adjust to compiler") with the exact registry file to grep.
