# Object Serving Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task.

**Goal:** Answer clones with signed URLs so pack bytes never pass through the fleet.

**Architecture:** The owner gains a snapshot endpoint returning refs and pack list from one read. Any
node authenticates a clone, asks the owner for that snapshot, and replies with `packfile-uris`
pointing at short-lived SAS URLs on blob storage. Incremental fetches, pushes, and clients without
`packfile-uris` keep the current forwarding path unchanged.

**Spec:** `docs/superpowers/specs/2026-08-18-object-serving-design.md`

## Global Constraints

- **Refs and pack list come from ONE read.** Every advertised ref must point into a pack the client
  was given a URL for. They live in one database for this reason.
- **No follower reads of a repo's database.** A non-owner asks the owner and waits. No `DbReader`
  on repo databases, no extra databases open anywhere.
- **URIs only when the fetch has no `have` lines.** Any `have` means incremental: use the existing
  path.
- **URLs are issued only after authentication**, are read-only, scoped to one pack blob, and expire
  after `SAS_TTL` (5 minutes).
- **Repack never deletes a pack immediately.** Superseded packs carry a timestamp and are deleted
  after `SUPERSEDED_GRACE` (24 hours).
- Ownership, leases, the leader and forwarding are untouched by this work.
- Comments explain *why*. Mark deliberate shortcuts with `ponytail:`.
- `cargo test --release` and `cargo clippy --all-targets` clean before every commit.
- Commit messages: no `Co-Authored-By`, no Claude references. Author `karthik@kloudlite.io`.

---

### Task 1: The snapshot — refs and packs from one read

**Files:** Modify `src/store.rs`, `src/http.rs`. Test in `tests/routing.rs`.

**Produces:**
- `pub struct Snapshot { pub refs: Vec<(String, ObjectId)>, pub packs: Vec<(String, u64)> }`
- `Store::snapshot(owner, name) -> Result<Snapshot>` — one database read producing both halves.
  Reuse `list_refs` and `pack_index`; the point of the method is that they are read together.
- `GET /snapshot/{owner}/{name}` on the **peer router only**, secret-guarded, returning it as JSON.
  A node that does not own the repo returns 421, exactly as the ownership endpoints do.

Tests: a snapshot's refs and packs are consistent with what a push just wrote; a non-owner returns
421; the endpoint is absent from the public router (a client must not be able to enumerate packs).

**Commit:** `Read a repo's refs and pack list as one snapshot`

---

### Task 2: Signed URLs

**Files:** Create `src/sas.rs`. Modify `src/store.rs`, `src/main.rs`.

**Produces:**
- `pub fn sign(account: &str, key: &str, container: &str, blob: &str, ttl: Duration) -> Result<String>`
  — a read-only Azure Blob SAS URL. Service SAS, `sp=r`, `se` = now + ttl, HMAC-SHA256 over the
  canonical string-to-sign.
- `pub const SAS_TTL: Duration = Duration::from_secs(300)`
- Wiring: the account name and key come from the same configuration the object store already uses.
  If they are unavailable, `sign` returns `Err` and the caller falls back to serving the pack itself
  — signing must never be a hard dependency of serving a clone.

Tests: a signed URL matches a known-good signature for fixed inputs (pin the canonicalisation — it
is the part that silently produces 403s); the expiry is in the future and reflects `SAS_TTL`; a
missing key yields `Err` rather than an unsigned URL.

**Commit:** `Sign read-only URLs for pack blobs`

---

### Task 3: Advertise and answer `packfile-uris`

**Files:** Modify `src/protocol/upload.rs`. Test in `tests/protocol.rs`.

**Consumes:** Tasks 1 and 2.

**Produces:**
- Advertise `fetch=wait-for-done packfile-uris` (the capability line currently reads
  `fetch=wait-for-done`).
- Parse the client's `packfile-uris <protocols>` argument. Honour it only when the protocol list
  contains `https`.
- In `fetch`, when the client offered `packfile-uris` AND there are no `have` lines: emit a
  `packfile-uris` section of `<sha1> <uri>` lines, one per pack, then an empty `packfile` section.
  The sha1 on each line is the pack's own name hash, which is what the client uses to name the file.
- Every other case takes the existing path untouched.

Tests: a fetch offering `packfile-uris` with no haves gets URI lines and no pack bytes; the same
fetch WITH haves gets a real packfile and no URI lines; a client that does not offer the capability
gets a real packfile; a real `git clone` against a server with signing disabled still succeeds
(the fallback path).

**Commit:** `Answer a clone with pack URLs instead of pack bytes`

---

### Task 4: Serve a clone from any node

**Files:** Modify `src/http.rs`, `src/ssh.rs`, `src/lib.rs`.

**Consumes:** Tasks 1-3.

**Produces:**
- Routing gains one case: for `git-upload-pack`, a non-owner that can reach the owner fetches the
  snapshot and answers locally with URIs, instead of forwarding. Authentication happens on the node
  that received the request, as it does today.
- If the snapshot call fails, or the client did not offer `packfile-uris`, or the fetch has `have`
  lines: forward to the owner exactly as now.
- `info/refs` is unchanged — it still forwards. Serving it from a snapshot costs the same round trip
  and buys nothing; the spec says so explicitly.

Tests: a clone through a non-owner returns URIs and the non-owner's pool stays cold — nothing is
opened, nothing is cached; an incremental fetch through a non-owner still forwards and the OWNER's
pool goes warm; with the owner unreachable, a clone through a non-owner 503s rather than serving
something it cannot vouch for.

**Commit:** `Let any node answer a clone`

---

### Task 5: Repack keeps superseded packs

**Files:** Modify `src/gc.rs`. Test in `tests/store.rs`.

**Produces:**
- Repack records superseded packs with a deletion timestamp instead of deleting them.
- A sweep deletes those older than `SUPERSEDED_GRACE` (24 hours).
- `pack_index` excludes superseded packs, so new snapshots never name them; only clients holding an
  older snapshot still reach them, which is the entire point.

Tests: a repack leaves the old packs readable; a snapshot taken after the repack names only the new
ones; a sweep with the clock advanced past the grace deletes them; a sweep before it does not.

**Commit:** `Keep superseded packs until every client that was told about them is gone`

## Self-Review

Spec coverage: one-read snapshot (T1) · no follower reads (T1, T4) · URIs only without haves (T3) ·
post-auth, read-only, short-lived URLs (T2, T4) · superseded grace (T5) · fallback for old clients
and for signing failures (T2, T3, T4) · ownership untouched (all).

Not covered, deliberately, per the spec: incremental fetches and pushes still run on the owner and
still need the local pack cache. `info/refs` still forwards.
