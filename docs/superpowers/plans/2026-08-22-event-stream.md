# Event Stream Implementation Plan (sub-project 2 of 3)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the merge worker's global Mongo polling claim (`pull_to_check`) and the activity feed's cross-repo query (`pulls_across`) with a Redis Stream, so neither depends on PRs living in one central collection — the prerequisite for moving pulls into per-repo databases.

**Architecture:** Owning nodes and the api tier `XADD` an event after each PR write. The worker consumes via a consumer group (`XREADGROUP`/`XACK`, `XAUTOCLAIM` for crashed consumers) instead of `find_one_and_update`-with-sort. The feed reads recent stream entries instead of querying pulls across repos. Pulls stay in Mongo for this whole sub-project — only *how work is discovered* changes, which keeps every step revertible.

**Tech Stack:** Rust, existing `redis` crate (already a dependency, already wired via `RUSTIC_GIT_REDIS_URL`), Azure Managed Redis (already deployed).

**Spec:** `docs/superpowers/specs/2026-08-22-repo-local-data-design.md` §4 (Events), §6 (Consistency), §9 step 2. Read it before implementing.

## Global Constraints

- `cargo test` green after every task; FOREGROUND test runs only (never background — run tests, commit, and write the report in the same session).
- No NEW clippy warnings in touched files (~9 pre-existing are ignored).
- House style: comments explain WHY; `// ponytail:` markers name ceiling + upgrade path; commit subjects imperative sentence case, no tool attribution.
- **Redis is never authoritative.** A lost event must never lose work: every consumer path keeps a reconcile fallback that finds work without the stream. Losing Redis degrades latency, never correctness.
- **A Redis outage must never fail a user operation.** Every `XADD` is fire-and-forget: log on failure, continue.
- Pulls stay in Mongo this sub-project. Do NOT move PR data or touch `repos`/`counters`.
- `noeviction` is a deployment prerequisite (spec §4) — Task 5 verifies and documents it; do not assume it.

---

### Task 1: The events module

**Files:**
- Create: `src/events.rs`
- Modify: `src/lib.rs` (add `pub mod events;`)
- Test: inline `#[cfg(test)]` in `src/events.rs` (encode/decode round-trip; no Redis needed)

**Interfaces produced:**
- `pub struct Event { pub kind: Kind, pub repo: String, pub number: i64, pub actor: String, pub at_ms: i64 }`
- `pub enum Kind { PullOpened, PullCommented, MergeRequested, PullMerged, PullClosed, HeadMoved }` with `fn as_str()` / `fn parse(&str) -> Option<Kind>`
- `pub async fn publish(cache: &crate::cache::Cache, e: &Event)` — fire-and-forget `XADD events * k v …` with `MAXLEN ~ 5000`; logs and returns on any error (never propagates)
- `pub fn fields(e: &Event) -> Vec<(String, String)>` / `pub fn from_fields(&[(String,String)]) -> Option<Event>`
- The stream name is a single `events` key (not per-repo): the worker needs ONE consumer group to see all work, and per-repo streams would need discovery of stream names — the thing this design is removing. Document that in the module doc.

Redis access goes through `crate::cache::Cache` (it owns the connection); add a `pub(crate)` accessor or a `pub async fn xadd(&self, stream, maxlen, fields)` on `Cache` rather than opening a second connection pool.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn fields_round_trip() {
    let e = Event { kind: Kind::PullOpened, repo: "alice/web".into(), number: 7,
                    actor: "alice@example.com".into(), at_ms: 1755772800000 };
    assert_eq!(from_fields(&fields(&e)).unwrap().number, 7);
    assert_eq!(from_fields(&fields(&e)).unwrap().kind.as_str(), "pull_opened");
}

#[test]
fn unknown_kind_is_ignored_not_fatal() {
    let f = vec![("kind".to_string(), "from_the_future".to_string()),
                 ("repo".to_string(), "a/b".to_string())];
    assert!(from_fields(&f).is_none()); // a consumer must skip it, never panic
}
```

- [ ] **Step 2: Run** `cargo test --lib events` — FAIL (module missing).
- [ ] **Step 3: Implement** per Interfaces. `publish` no-ops when the cache is disabled (`conn: None`) so tests and single-node runs need no Redis.
- [ ] **Step 4: Run** `cargo test --lib events` — PASS; full `cargo test`.
- [ ] **Step 5: Commit** — `Add the event stream module`

---

### Task 2: Publish events from every PR write

**Files:**
- Modify: `src/api.rs` — the PR write handlers (grep `open_pull`, `comment_on_pull`, `request_merge` call sites)
- Test: `src/api.rs` inline or the existing api test file — assert publish is called with the right kind (use `Cache::memory()`, then read the stream back; if `Cache::memory()` has no stream support, add a minimal in-memory stream to it in Task 1 and assert against that)

**Interfaces consumed:** `events::{publish, Event, Kind}` from Task 1.

- [ ] **Step 1: Failing test** — opening a PR publishes exactly one `PullOpened` carrying `repo` and `number`; commenting publishes `PullCommented`.
- [ ] **Step 2: Run** — FAIL.
- [ ] **Step 3: Implement** — after each successful Mongo write, `events::publish(...)`. Never before the write, never with `?`.
- [ ] **Step 4: Run** — PASS; full suite.
- [ ] **Step 5: Commit** — `Publish an event on every pull request write`

---

### Task 3: Worker consumes the stream

**Files:**
- Modify: `src/bin/worker.rs` (~line 258, the `pull_to_check` caller), `src/cache.rs` (consumer-group helpers)
- Test: `tests/` — a worker-loop test against `Cache::memory()`'s stream, or a unit test of the claim/ack decision function

**Interfaces produced on `Cache`:** `xgroup_create_mkstream(stream, group)`, `xreadgroup(stream, group, consumer, count, block_ms) -> Vec<(id, Vec<(String,String)>)>`, `xack(stream, group, id)`, `xautoclaim(stream, group, consumer, min_idle_ms, count)`.

**The safety rule (Global Constraints):** the stream is a *nudge*, not the record. Keep `pull_to_check` as a **periodic fallback sweep** (e.g. every 60s) so a dropped event delays a check rather than losing it. Say so in the loop's comment. This is what lets Task 5 tolerate a Redis outage and what keeps sub-project 3's cutover safe.

- [ ] **Step 1: Failing test** — an event published for `alice/web#3` causes the worker's next iteration to check that PR; a consumed event is ACKed; an event whose consumer died is re-delivered by `XAUTOCLAIM`.
- [ ] **Step 2: Run** — FAIL. **Step 3: Implement**: create the group on startup (idempotent, ignore BUSYGROUP), read with a short block, process, ACK; on empty read fall through to the periodic `pull_to_check` sweep. **Step 4: Run** — PASS; full suite.
- [ ] **Step 5: Commit** — `Feed the merge worker from the event stream`

---

### Task 4: Feed reads the stream

**Files:**
- Modify: `src/api.rs` (~line 1289, the `pulls_across` caller in the activity feed)
- Test: existing feed test if one exists (grep `fn feed` / `activity` in tests), else a new one

- [ ] **Step 1: Failing test** — after publishing three events, the feed returns them newest-first, capped at the requested `n`.
- [ ] **Step 2: Run** — FAIL.
- [ ] **Step 3: Implement** — read recent entries with `XREVRANGE events + - COUNT n`, map to the existing `Event` response shape the web app already consumes (do NOT change the wire shape — check `web/apps/web/src/lib/browse.ts` for the fields it reads). Keep `repos_for` for the `repo_created` half of the feed; only the PR half moves. **Fallback:** if the stream is empty or Redis is down, fall back to `pulls_across` so the feed degrades to today's behavior rather than going blank.
- [ ] **Step 4: Run** — PASS; full suite.
- [ ] **Step 5: Commit** — `Build the activity feed from the event stream`

---

### Task 5: Durability prerequisite and docs

**Files:**
- Modify: `deploy/rustic-git.yaml` (worker Deployment comment), `CLAUDE.md` (one load-bearing line)
- Verify: the Redis instance's eviction policy

- [ ] **Step 1: Check the live policy** — `redis-cli CONFIG GET maxmemory-policy` against the configured instance (or the Azure portal/CLI equivalent; Azure Managed Redis may not expose CONFIG — if so, record how it was verified). `noeviction` is required: an eviction policy that can drop stream entries silently drops queued merge work. If it is NOT `noeviction`, do not change it silently — record the finding in the report and state the risk; the periodic fallback sweep (Task 3) is what keeps that from losing work.
- [ ] **Step 2: Document** — worker Deployment comment: events are a nudge, the periodic sweep is the floor, and the eviction-policy requirement. `CLAUDE.md`: one line under load-bearing rules — the `events` stream is a nudge for the worker and a view for the feed, never the record; every consumer keeps a fallback.
- [ ] **Step 3:** `kubectl apply --dry-run=client -f deploy/rustic-git.yaml` OK; `cargo test` green.
- [ ] **Step 4: Commit** — `Document the event stream and its durability requirement`

---

## Final verification

- [ ] `cargo test` — full suite green
- [ ] `cargo clippy --lib` — no new warnings in touched files
- [ ] Redis-down drill: with `RUSTIC_GIT_REDIS_URL` unset, PR writes still succeed, the worker still finds work via the fallback sweep, the feed still renders via `pulls_across`
- [ ] Re-read spec §4/§6: every claim maps to a landed task
