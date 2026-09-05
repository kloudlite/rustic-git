# Ownership Map Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task.

**Goal:** Replace rank-and-probe routing with an ownership map that pod zero writes and every node reads.

**Architecture:** `cluster/ownership` is a SlateDB database. `kloudlite-0` opens it for writing and is the only writer; every other node opens it read-only (`DbReader`, `FollowLatest`, 200ms poll). Routing reads the map locally; claims are an HTTP call to pod zero over the existing peer port. The pool's lifecycle drives claim/renew/release.

**Spec:** `docs/superpowers/specs/2026-08-18-ownership-registry-design.md`

## Global Constraints

- **Only pod zero writes the map.** Followers never open it for writing. `kloudlite-0` is derived from `KLOUDLITE_SELF` by stripping the trailing `-{ordinal}` and appending `-0`.
- **`manifest_poll_interval` = 200ms** for follower readers. **`flush_interval` = 10ms** for the ownership DB.
- **Follower staleness must never grant.** A follower's read can be wrong; only pod zero's decision grants ownership. A node asked for a repo it does not hold consults pod zero rather than serving.
- **The lifecycle invariant:** a node holds a repo's lease exactly as long as it holds that repo's database open. Claim precedes open; release precedes close.
- **Release order:** extend-with-short-expiry → keep serving `drain` → close DB. Never delete-then-close.
- **No failover to ordinal 1.** If pod zero is unreachable, claims fail with 503; leadership is never reassigned.
- Comments explain *why*. Mark deliberate shortcuts with `ponytail:`.
- `cargo test --release` and `cargo clippy --all-targets` clean before every commit.
- Commit messages: no `Co-Authored-By`, no Claude references. Author `karthik@kloudlite.io`.

---

### Task 1: The ownership map

Pure state and decisions, no I/O: what an entry is, what the leader decides on claim/renew/release, and when an entry has expired. Unit-tested with a scripted clock.

**Files:** Create `src/ownership.rs`, `src/ownership/tests.rs`. Modify `src/lib.rs`.

**Produces:**
- `pub struct Entry { pub node: String, pub expires_ms: u64 }` — `expires_ms` is Unix epoch millis, so it survives the round trip through SlateDB
- `pub const LEASE_TTL: Duration = Duration::from_secs(10)`, `pub const RENEW_EVERY: Duration = Duration::from_secs(3)`, `pub const DRAIN: Duration = Duration::from_millis(500)`
- `pub fn leader_of(self_name: &str) -> crate::Result<String>` — `"kloudlite-2"` → `"kloudlite-0"`; `Err` if the name has no `-{ordinal}` suffix
- `pub enum Grant { Granted(Entry), HeldBy(Entry) }`
- `pub fn decide_claim(current: Option<&Entry>, asker: &str, now_ms: u64) -> Grant` — grants if `current` is `None` or expired, or if `current.node == asker` (re-claim of one's own is idempotent); otherwise `HeldBy`
- `pub fn decide_renew(current: Option<&Entry>, asker: &str, now_ms: u64) -> Option<Entry>` — extends only if the asker still holds it and it has not expired; `None` means the asker lost it and must close
- `pub fn decide_release(current: Option<&Entry>, asker: &str, now_ms: u64) -> Option<Entry>` — sets `expires_ms = now_ms + DRAIN` if the asker holds it; `None` otherwise
- `pub fn is_expired(e: &Entry, now_ms: u64) -> bool`

Tests to write (all pure, pass `now_ms` explicitly — no sleeping, no wall clock):
- `leader_of` for `-0`, `-2`, `a-b-12`; errors for `nodash`, `x-notanumber`
- claim on an absent entry grants; on a live entry held by someone else returns `HeldBy` with that entry; on an expired entry grants; re-claim by the current holder grants (idempotent)
- renew by the holder extends; renew by a non-holder returns `None`; renew of an expired entry returns `None`
- release by the holder yields an entry expiring `DRAIN` from now, and **that entry is still un-expired**, so a claim during the drain returns `HeldBy` — the test that encodes why release is not a delete
- release by a non-holder returns `None`

**Commit:** `Decide ownership: claim, renew and release as pure functions`

---

### Task 2: The store — one SlateDB the leader writes and the rest read

Wraps Task 1's decisions in the database, and nothing else. No HTTP.

**Files:** Modify `src/ownership.rs`, `src/ownership/tests.rs`. Modify `src/store.rs` if a shared object-store handle is wanted.

**Consumes:** Task 1's decisions.

**Produces:**
- `pub struct OwnershipStore` holding either `Writer(Arc<Db>)` or `Reader(Arc<DbReader>)`
- `pub async fn OwnershipStore::open(os: Arc<dyn ObjectStore>, is_leader: bool) -> Result<OwnershipStore>` — path `cluster/ownership`. Leader: `Db::builder` with `Settings { flush_interval: Some(10ms), ..default }` and background compaction **off** (`compactor_options: None`) so a follower's `FollowLatest` reader cannot have objects deleted under it. Follower: `DbReader::open` with `DbReaderMode::FollowLatest` and `manifest_poll_interval: 200ms`.
- `pub async fn get(&self, repo: &str) -> Result<Option<Entry>>` — reads a key; works on both variants
- `pub async fn put(&self, repo: &str, e: &Entry) -> Result<()>` — leader only; `Err` on a follower (a follower writing is a bug, not a fallback)
- `pub async fn all(&self) -> Result<Vec<(String, Entry)>>` — scan, for pruning and for `/healthz` diagnostics
- Key format `own/{repo}`, value JSON. Use `serde_json` if already a dependency; otherwise a two-field manual encode — do not add a dependency for two fields.

Tests (integration, `tests/ownership.rs`, in-memory object store):
- a leader can `put` then `get`
- a follower opened on the same store reads what the leader wrote (allow up to 1s for the poll; assert it arrives)
- a follower's `put` returns `Err`
- `all()` returns everything written

**Commit:** `Store the ownership map in its own database`

---

### Task 3: The protocol and the routing

Claims over the peer port; routing reads the map. Deletes the rank-and-probe machinery.

**Files:** Modify `src/proxy.rs`, `src/http.rs`, `src/ssh.rs`, `src/lib.rs`. Delete `src/peers.rs`, `src/peers/tests.rs`. Modify `tests/routing.rs`.

**Produces:**
- On the **peer** router only: `POST /own/claim`, `POST /own/renew`, `POST /own/release`, each taking `{repo(s), node}` JSON and returning the resulting `Entry` or an explicit "held by" answer. **Leader-only:** a follower receiving these returns 421 (it is not the leader; the caller has a stale idea of who is).
- `pub async fn App::owner(&self, repo: &str) -> Result<Option<Entry>>` — local read from `OwnershipStore`
- `pub async fn App::claim(&self, repo: &str) -> Result<Grant>` — if this node *is* the leader, decide locally and write; otherwise POST to `{leader}.{svc}:{peer_port}/own/claim`
- `App::renew_all` / `App::release` similarly
- `route` middleware in `http.rs` becomes: read the map → `Some(e)` naming us → serve; `Some(e)` naming another → forward; `None`/expired → `claim()` → granted? serve : forward to the holder. On `claim()` failure (pod zero down) → **503**, never serve.
- SSH `run` uses the same three-way decision before `open_repo`.

**Deleted:** `peers::{rank, Membership, decide, Route, Peer}`, `proxy::{reachable, probe_via, probe_once_with_retry, probe_via_once, PROBE_*, in_flight, via_in_flight, up_cache}`, the `/probe` handler, `KLOUDLITE_REPLICAS`. Keep `PEER_HEADER`, `HOPS_HEADER`, `MAX_HOPS`, `OWNER_HEADER`, `forward`, `stream_to_peer`, `serve_peer_streams`, `stream_addr`.

Tests in `tests/routing.rs` — **rewrite the helpers**: `node()` now takes `(os, name, svc_hosts)` and opens an `OwnershipStore` per node with `is_leader = name ends with -0`. Keep every existing *behavioural* test that is still meaningful (forwarding, peer secret, hop bound, fence handling, real git push/clone, real SSH clone) and add:
- a claim on an unowned repo is granted, and only the claimant's pool goes warm
- a second node asking for the same repo is told the holder and forwards there
- a follower receiving `/own/claim` returns 421
- with pod zero stopped, a cold repo returns 503 and **no** node opens it
- a release makes the repo claimable only after the drain, not during it

**Commit:** `Route by the ownership map instead of ranking and probing`

---

### Task 4: Lifecycle, wiring, and deployment

**Files:** Modify `src/pool.rs`, `src/main.rs`, `deploy/kloudlite.yaml`, `README.md`.

- `Pool` gains a release hook: `pub fn on_release(&self, f: impl Fn(String) -> BoxFuture<'static, ()>)` or an `Arc<dyn OwnershipHooks>` — whichever is cleaner. Eviction (idle and `MAX_WARM`) becomes: call release → **spawn** a task that sleeps `DRAIN` then closes the handle. The sweeper must not block for the drain.
- A node that learns it has lost a lease (a `renew` returning `None`) closes that database immediately.
- `main.rs`: derive `leader = leader_of(self_name)?`; open `OwnershipStore` with `is_leader`; spawn a renewal task every `RENEW_EVERY` that renews everything the pool holds; spawn a prune task on the leader that drops expired entries. Single-node (`KLOUDLITE_PEER_SVC` unset) keeps working with no ownership store at all — one node owns everything by construction.
- Manifest: drop `KLOUDLITE_REPLICAS`; nothing else changes (the peer ports and secret are unchanged).
- README + spec: reflect what shipped.

**Commit:** `Bind the lease to the open database, and wire it up`

## Self-Review

Spec coverage: leadership by name (T1 `leader_of`, T4 wiring) · single writer (T2) · tuned intervals (T2) · claim/renew/release (T1 decisions, T2 storage, T3 protocol) · release ordering and drain (T1 test, T4 eviction) · lifecycle invariant (T4) · staleness never grants (T3 `421` + claim-on-miss) · no failover to ordinal 1 (T3 503 test) · what-goes list (T3).
