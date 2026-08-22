# Repo metadata and pulls into SlateDB (sub-project 3 of 3)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement
> this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move repo metadata and pull requests out of Cosmos/Mongo and into each repo's own SlateDB,
so a repo carries its own truth. Retire the Mongo `repos`, `pulls`, and `counters` collections.
Teams, users, handles, credentials and passkeys STAY central — that is the owner's ruling, not an
oversight.

**Spec:** `docs/superpowers/specs/2026-08-22-repo-local-data-design.md` §1, §5, §6, §9 step 3.
Read it before implementing.

**Tech Stack:** Rust, axum, SlateDB, existing `Store`/`Pool`/`keyed_lock`, existing routing middleware.

---

## Two rulings that DEVIATE from the spec — read these first

### 1. Discrete `meta/*` keys, not one line-encoded `meta` blob

Spec §1 proposes a single `meta` key holding description/public/created_by/created_at. The repo DB
**already has a `meta/` namespace** — `meta/public` (`src/refs.rs:34`) and `meta/protect/`
(`src/refs.rs:42`) — with a doc comment explaining why repo state lives there. Extending that beats
introducing a second, differently-encoded convention beside it:

| Key | Value |
|---|---|
| `meta/public` | `1`/`0` — **already exists, already the authorizing truth.** Not part of this move. |
| `meta/description` | utf-8 bytes |
| `meta/created_by` | utf-8 bytes |
| `meta/created_at` | ms since epoch, decimal |
| `meta/next_pull` | next PR number, decimal |
| `pull/{number:08}` | the full `PullRequest` as serde_json bytes |
| `meta/pulls_migrated` | `1` once this repo's Mongo PRs have been copied in (see ruling 2) |

Discrete keys mean a description edit is one `put`, not a read-modify-write of a blob that a
concurrent visibility flip could clobber. Zero-padded `pull/{number:08}` keeps `scan_prefix("pull/")`
in numeric order, as the spec requires.

**`meta/public` already being the truth is the single biggest simplification in this sub-project:**
visibility does not move at all. `Store::is_public` (`src/refs.rs:230`) already reads the repo's own
DB and is already what authorizes; Mongo's `Repo.public` is only a listing mirror. Only description,
created_by, created_at and the PRs actually move.

### 2. Backfill is NOT deferred — it becomes lazy, per-repo, on first touch

Spec §9 defers backfill. **That is unsafe as written** and I am overriding it, because
`dev.kloudlite.io` holds live repos and pull requests:

- Cutting PR reads over to SlateDB with an empty `pull/` prefix makes **every existing PR vanish**.
- `meta/next_pull` starting at 1 while Mongo already has PRs 1..5 **collides**, and `pull/{number}`
  is the identity — a collision overwrites a real PR.

Neither is acceptable, and no flag hides it: the moment reads cut over, the damage is visible.

The fix is not a big-bang migration script (which the owner declined, reasonably). It is
**migrate-on-first-touch, per repo, on the owning node**:

```
ensure_migrated(owner, name):
    if db.get("meta/pulls_migrated") == Some("1"): return          # fast path, one get
    lock = keyed_lock("pulls/{owner}/{name}"); guard = lock.lock()
    if db.get("meta/pulls_migrated") == Some("1"): return          # re-check under lock
    rows = mongo.pulls_for("{owner}/{name}")                       # may be empty; that is fine
    for pr in rows: db.put("pull/{pr.number:08}", json(pr))
    db.put("meta/next_pull", max(numbers, default 0) + 1)
    db.put("meta/pulls_migrated", "1")                             # LAST — see below
```

Properties that make this safe:
- **The owning node is the only party that runs it**, so it is a single writer by construction —
  the same property every other write in this design leans on.
- **Idempotent.** `meta/pulls_migrated` is written LAST, so a crash mid-copy re-runs the whole copy
  next time and re-`put`s the same keys with the same content. Nothing is appended, so re-running
  cannot duplicate.
- **Bounded.** One extra `get` per repo forever; one Mongo query once per repo, ever.
- **Ordering.** The marker is written last for the same reason §6.2 orders DB-before-view: a crash
  leaves work to redo, never a repo that believes it migrated when it did not.

This keeps the owner's "no big-bang backfill" intent while refusing to lose data. Mongo `pulls`
stays readable (not dropped) until Task 10 confirms every repo is migrated.

### 3. The merge worker's safety floor has to move (spec gap)

`Directory::pull_to_check()` (`src/directory.rs:1045`) is a **global** Mongo scan and is the merge
worker's floor — the thing sub-project 2 deliberately kept so a lost Redis event delays work rather
than losing it. Once PRs live in per-repo databases **no worker can scan them**: opening a repo's DB
on a non-owning node fences the owner, which is the exact hazard this whole design exists to avoid.
The spec names the consumers (§5) but does not resolve this. Resolution:

- **Discovery moves to the owning node**, the one party allowed to read that truth. Each node sweeps
  the repos it already owns for PRs needing a mergeability check.
- **One shared function does the check**, reachable two ways:
  - the owner's own periodic lane → **the floor, and it needs no Redis at all** (strictly better than
    today's floor, which needed Mongo to be reachable);
  - a routed `POST /api/{owner}/{name}/pulls/{n}/check` the worker calls on a stream nudge → the
    low-latency path.
- The worker keeps consuming the stream. It stops discovering work and starts being told about it.

Net effect on the §6 safety property: unchanged in kind, better in practice — the floor no longer
depends on any central system being up.

---

## Global Constraints

- `cargo test` green after every task; **FOREGROUND test runs only** — never background. Run tests,
  commit, and write the report in the same session.
- No NEW clippy warnings in touched files (~10 pre-existing are ignored; `cargo clippy --lib`).
- House style: comments explain WHY; `// ponytail:` markers name ceiling + upgrade path; commit
  subjects imperative sentence case, no tool attribution.
- **Never open a repo's or image's DB on a non-owning node.** Every new read/write of `meta/*` or
  `pull/*` happens on the owning node, reached through the routing middleware. If you are writing
  code in `src/api.rs` that touches those keys directly, you have made a mistake — forward instead.
- **Every new `/api/{owner}/{name}/{tail}` route needs its tail in `BROWSE_TAILS`**
  (`src/http.rs:181`) or the middleware 404s it before axum sees it.
  `every_browse_route_is_routable` (`src/http.rs:997`) scrapes `browse_api/mod.rs` source text — use
  the literal string form it expects.
- **Truth before view, always.** DB write first, then marker/generation bump. A crash must leave a
  stale listing, never wrong truth.
- Teams/users/handles/credentials/passkeys stay in Mongo. Do not touch them.

---

### Task 1: Repo meta keys in the repo's own DB

**Files:** `src/refs.rs` (beside `PUBLIC_KEY`), test inline.

**Interfaces produced on `Store`:**
- `set_repo_meta(owner, name, description: &str, created_by: &str, created_at_ms: i64) -> Result<()>`
- `repo_meta(owner, name) -> Result<Option<RepoMeta>>` where
  `RepoMeta { description: String, created_by: String, created_at_ms: i64, public: bool }`
  (`public` read from the existing `meta/public`, so callers get one coherent answer)
- `set_repo_description(owner, name, &str) -> Result<()>`

Return `None` from `repo_meta` only when `meta/created_at` is absent — that is the "never written"
signal Task 2 keys migration off. A missing description is an empty string, not `None`.

- [ ] **Step 1: Failing test** — set then get round-trips all fields; `repo_meta` on an untouched
      repo is `None`; a repo with only `meta/public` set still reads `None` (proves the created_at
      sentinel, not the public flag, decides).
- [ ] **Step 2: Run** `cargo test --lib` — FAIL.
- [ ] **Step 3: Implement.** Follow the `PUBLIC_KEY` doc-comment style: say WHY repo state lives here.
- [ ] **Step 4: Run** — PASS; full `cargo test`.
- [ ] **Step 5: Commit** — `Store repo metadata in the repo's own database`

---

### Task 2: Write repo meta on create and edit (dual-write)

**Files:** `src/api.rs` (repo create + settings edit), `src/http/browse_api/admin.rs` (receiving handlers).

Repo create and description edit must write the repo's DB **through the owning node**, then update
Mongo as today. Both writes stay for now — Mongo is still the read path until Task 4. New/changed
routes get tails in `BROWSE_TAILS`.

- [ ] **Step 1: Failing test** — creating a repo leaves `repo_meta` readable on the owner; editing
      the description updates it.
- [ ] **Step 2: Run** — FAIL.
- [ ] **Step 3: Implement.** Owner first, then Mongo — matching the ordering comment already at
      `api.rs:2211` for visibility ("the fleet first, and only then the index").
- [ ] **Step 4: Run** — PASS; full suite.
- [ ] **Step 5: Commit** — `Write repo metadata to the owning node on create and edit`

---

### Task 3: Structural repair for REPO markers

**Files:** `src/registry/gc.rs` (generalize `reconcile_owner`), test inline.

`reconcile_owner` (`gc.rs:133`) already does structural repair for `Kind::Img`. Repos need the same
before listings depend on markers (Task 4). Repo directories are discoverable from the object store
under `repo/{owner}/` **without opening any database** — which is what makes this safe for the GC
worker to run. `repo/img/...` is the image keyspace; `img` is a reserved owner name, so excluding it
is a name check, not a guess.

Same three structural cases, same keep-biased rule: **any uncertainty aborts rather than deletes.**
(a) repo directory with no marker → create PRIVATE (fail closed); (b) marker with no directory →
remove; (c) stale body → `put_in_place`. **Visibility stays out of scope** — that is the owning
node's duty (`Store::reconcile_marker`), because only the owner may read `meta/public` without
fencing itself. Keep that split explicit in the doc comment, as the image version does.

- [ ] **Step 1: Failing test** — orphan marker removed; unmarked repo dir gains a PRIVATE marker;
      a marker whose visibility disagrees with the DB is left ALONE by this sweep.
- [ ] **Step 2: Run** — FAIL. **Step 3: Implement. Step 4: Run** — PASS; full suite.
- [ ] **Step 5: Commit** — `Repair repo listing markers structurally`

---

### Task 3b: The owner-side visibility repair lane (spec §6.4's missing half)

**Files:** the renewal/warm loop that already runs on each node (grep `warm_repos`), `src/store.rs`.

**Why this must land BEFORE Task 4.** Spec §6.4 asks for visibility repair in TWO places: on open,
and on a low-frequency lane of the renewal loop. Only the first exists — `open_repo` calls
`reconcile_marker` (`src/store.rs:239`). That is not enough at cutover:

- Pre-existing repos have NO marker (markers are new in sub-project 1).
- Task 3's structural sweep creates a missing marker **PRIVATE**, correctly failing closed.
- So the moment Task 4 switches listings to markers, **a public repo that nobody happens to touch
  is missing from every listing, indefinitely** — it only heals if someone clones or browses it.

That is a "where did all my public repos go" incident, and lazy on-open repair cannot prevent it.

Add the periodic lane: each node walks the repos **it already owns** and calls the existing
`reconcile_marker(owner, name, Kind::Repo)` on each. Constraints:
- **Only repos this node owns.** Reuse whatever ownership set the renewal loop already iterates —
  do NOT enumerate all repos and open them, which is the fencing hazard.
- Low frequency and paced (there is a `GC_OWNER_GAP`-style sleep precedent in `worker.rs`).
- Log-and-continue per repo; one bad repo must not stop the lane.
- State the resulting drift ceiling as a number in the comment ("a crashed flip is corrected within
  X"), per spec §6.

- [ ] **Step 1: Failing test** — a repo whose `meta/public` is `1` but whose marker is private has
      its marker corrected to public by the lane; a repo the node does NOT own is left untouched.
- [ ] **Step 2: Run** — FAIL. **Step 3: Implement. Step 4: Run** — PASS; full suite.
- [ ] **Step 5: Commit** — `Reconcile owned repo markers on a periodic lane`

---

### Task 3c: One-shot marker backfill from Mongo (closes the cutover gap)

**Files:** `src/main.rs` (a new `admin` subcommand), test inline.

**Why.** Task 3b landed the periodic lane, but its ownership set is `pool.warm_repos()` — repos this
node currently holds OPEN. That is what spec §6.4 asks for ("for what it holds warm"), and it is the
right scope: the alternative, walking every assigned repo, means opening thousands of databases on a
timer, which is how the WAL explosion that took `rustic-git-0` down began.

But it leaves one case genuinely uncovered, and it is the cutover case:

> a pre-existing PUBLIC repo that nobody touches has no marker, is never warm, and so is never
> reached by either repair loop. Task 3's structural sweep will mark it PRIVATE (correctly failing
> closed) and it then stays missing from listings indefinitely.

So the two repair loops are NOT total for never-touched repos, and the spec's totality claim is
overstated there. Neither loop can fix it, because both are driven by the repo being touched.

**The fix uses Mongo one last time on its way out.** Mongo's `repos` collection still holds a
`public` mirror for every existing repo, kept in sync by `update_repo` (owner first, then Mongo).
A marker is a VIEW, so a mirror is a perfectly good source for it — and any error self-heals the
next time the owner touches the repo via `reconcile_marker`.

Add `rustic-git admin backfill-repo-markers`:
- Read every row from Mongo `repos`.
- For each, `index::write(Kind::Repo, owner, Marker { name, public, created_by, created_ms,
  description, .. })` from the row's own fields.
- **Idempotent and re-runnable** — it overwrites, never appends, so running it twice is a no-op.
- **Never deletes.** A marker with no Mongo row is left alone; removing markers is the GC sweep's
  job and it is keep-biased for good reason.
- Print a count, and print each repo it could not write, so the operator sees partial failure rather
  than a silent one.
- It only writes object-store keys, so it opens NO database and can run from anywhere — which is
  why it is an admin command and not a node lane.

Run it ONCE at deploy, before Task 4's listing cutover goes live. Task 12 re-runs it as a
verification step (a second run repairing nothing proves convergence).

- [ ] **Step 1: Failing test** — a Mongo row with `public: true` produces a PUBLIC marker; `public:
      false` produces a PRIVATE one; running twice changes nothing; a marker whose repo has no Mongo
      row is NOT removed.
- [ ] **Step 2: Run** — FAIL. **Step 3: Implement. Step 4: Run** — PASS; full suite.
- [ ] **Step 5: Commit** — `Backfill repo listing markers from the directory`

---

### Task 4: Repo listings read markers; close the delete-path lock gap

**Files:** `src/api.rs` (repo listing, and the two `forget_repo` call sites at `api.rs:1345` and
`api.rs:2253`), `src/http/browse_api/admin.rs`.

Two things, together because they touch the same paths:

1. Repo listings stop calling `Directory::repos_for` and read `index::list(Kind::Repo, ...)`, the way
   image listings already do.
2. **The delete-path lock gap** (carried over from sub-project 1's final review): the visibility-flip
   handlers take the `index/repo/{owner}/{name}` keyed lock (`admin.rs:50`, `admin.rs:135`) but the
   two repo-delete sites do not. A delete racing a flip can leave an orphaned marker for a repo that
   no longer exists. Take the same lock on the delete path, in the same order.

- [ ] **Step 1: Failing test** — a listing reflects markers, not Mongo; an anonymous listing never
      contains a private repo (the leak test); a delete concurrent with a flip leaves no marker.
- [ ] **Step 2: Run** — FAIL. **Step 3: Implement. Step 4: Run** — PASS; full suite.
- [ ] **Step 5: Commit** — `List repos from markers and lock the delete path`

---

### Task 5: The pull store (pure, unit-testable)

**Files:** Create `src/pulls.rs`; `src/lib.rs` (`pub mod pulls;`).

Key encoding and the numbering sequence, with **no HTTP and no Mongo** so it is testable directly.

**Interfaces produced:**
- `pull_key(number: i64) -> String` (`pull/{number:08}`)
- `get(db, number) -> Result<Option<PullRequest>>`
- `put(db, &PullRequest) -> Result<()>`
- `list(db) -> Result<Vec<PullRequest>>` (scan_prefix, numeric order by construction)
- `open_only(db, limit) -> Result<Vec<PullRequest>>`
- `next_number(store, owner, name) -> Result<i64>` — under `keyed_lock("pulls/{owner}/{name}")`:
  read `meta/next_pull`, write back +1, return the old value.

**Ruling 4 — timestamps become `i64` ms, NOT bson `DateTime`.** `PullRequest.created_at` is today a
`mongodb::bson::DateTime`. Storing that as `serde_json` bytes is fragile (bson types round-trip
through a non-bson serializer only by accident of their Serialize impl) and would leave repo-local
truth carrying a MongoDB-shaped value long after Mongo is gone — the opposite of the point of this
move. The API **already** converts on the way out (`.timestamp_millis()` at `api.rs:1218`, `:1496`,
`:1554`), so the wire shape the web app consumes is already plain i64 milliseconds.

So `PullRequest` moves into `pulls.rs` with `created_at_ms: i64` and `merged_at_ms: Option<i64>`
(same for any nested `Comment`/`MergeJob`/`Mergeability` timestamps). Task 6's migration converts at
the Mongo boundary, which is the only place a bson value should still appear. The api tier's
`.timestamp_millis()` calls collapse to plain field reads. **The JSON the web app receives must be
byte-identical** — `web/apps/web/src/lib/api.ts` needs no edit, and if it does, something is wrong.

Drop `#[serde(rename = "_id")] id` too: `id` was `"{repo}#{number}"`, a Mongo primary key. The
SlateDB key already encodes the number and the DB already belongs to the repo, so the field is
redundant there — but check `api.rs` and the web app for readers of `id` before removing it, and
keep it in the JSON response if anything reads it.

`PullRequest` moves out of `directory.rs` into here unchanged (same serde shape) so the wire format
the web app reads does not shift. Keep `#[serde(rename = "_id")] id` for now — dropping it is a
separate, later change.

- [ ] **Step 1: Failing test** — round-trip; `list` returns numeric order across the 9→10 and 99→100
      boundaries (the whole point of zero-padding); concurrent `next_number` on one node hands out
      **distinct** numbers (mirror `concurrent_pulls_count_every_hit`).
- [ ] **Step 2: Run** — FAIL. **Step 3: Implement. Step 4: Run** — PASS; full suite.
- [ ] **Step 5: Commit** — `Add the repo-local pull request store`

---

### Task 6: Lazy per-repo migration from Mongo

**Files:** `src/pulls.rs`, test inline.

Implement `ensure_migrated` exactly as specified in **ruling 2** above. It runs on the owning node
only, under the repo's `pulls/{owner}/{name}` lock, writes `meta/pulls_migrated` LAST.

- [ ] **Step 1: Failing test** — a repo with 3 Mongo PRs migrates all 3 and sets `next_pull` to 4;
      running it twice is a no-op (no duplicates, `next_pull` unchanged); **a crash before the marker
      re-runs cleanly and converges to the same state**; a repo with zero Mongo PRs still marks
      migrated with `next_pull` = 1.
- [ ] **Step 2: Run** — FAIL. **Step 3: Implement. Step 4: Run** — PASS; full suite.
- [ ] **Step 5: Commit** — `Migrate a repo's pull requests on first touch`

---

### Task 7: Routed PR handlers on the owning node

**Files:** Create `src/http/browse_api/pulls.rs`; modify `src/http/browse_api/mod.rs`,
`src/http.rs` (`BROWSE_TAILS`).

Every PR operation becomes a handler that runs on the owning node against that repo's DB. Each
calls `ensure_migrated` first. Add the tail `"pulls"` to `BROWSE_TAILS` — all PR routes share it as
their third segment, so one entry covers them all.

Routes: list, get, open, comment, request-merge, close, and `check` (Task 8 uses that last one).
Every write publishes its `events::Event` after the DB write, never before — sub-project 2's
contract.

- [ ] **Step 1: Failing test** — extend `every_browse_route_is_routable` coverage; a PR opened
      through the routed handler is readable through it; `open` allocates sequential numbers.
- [ ] **Step 2: Run** — FAIL. **Step 3: Implement. Step 4: Run** — PASS; full suite.
- [ ] **Step 5: Commit** — `Serve pull requests from the owning node`

---

### Task 8: Mergeability discovery moves to the owner

**Files:** `src/http/browse_api/pulls.rs` (the `check` handler), `src/bin/worker.rs`, the owning
node's periodic lane (beside the existing renewal/reconcile loop).

Implement **ruling 3**. One shared check function; the owner's periodic lane calls it in-process
(the Redis-free floor), the worker calls the routed endpoint on a stream nudge (low latency).
`pull_to_check`, `claim_merge`, `record_mergeability`, `finish_merge`, `clear_merge` become
repo-local operations. Delete the worker's global-sweep path.

State the drift ceiling as a number in the loop's comment, per spec §6 ("a pending check is picked
up within X").

- [ ] **Step 1: Failing test** — the owner's sweep finds a PR needing a check **with Redis entirely
      down** (this is the floor — it is the most important test in this plan); a stream event
      triggers the routed check; a claim is not handed to two workers at once.
- [ ] **Step 2: Run** — FAIL. **Step 3: Implement. Step 4: Run** — PASS; full suite.
- [ ] **Step 5: Commit** — `Discover mergeability work on the owning node`

---

### Task 9: api.rs forwards PR requests instead of querying Mongo

**Files:** `src/api.rs` (the PR section, comment header at `api.rs:2342`).

The api tier's PR endpoints stop calling `directory(api)?` and forward through
`ask_owner`/`read_from_owner` (`api.rs:2166`, `api.rs:2359`). **The wire shape the web app consumes
does not change** — every endpoint is already a per-repo path, so this is purely a change of where
the data comes from. `web/apps/web/src/lib/api.ts` must need no edit; if it does, something is wrong.

- [ ] **Step 1: Failing test** — the full PR lifecycle end-to-end through the api tier: open →
      comment → request merge → close, with no Mongo `pulls` involvement.
- [ ] **Step 2: Run** — FAIL. **Step 3: Implement. Step 4: Run** — PASS; full suite.
- [ ] **Step 5: Commit** — `Forward pull request traffic to the owning node`

---

### Task 10: Retire the Mongo repo/pull surface

**Files:** `src/directory.rs`.

Delete, now that nothing calls them: `open_pull`, `pulls_for`, `open_pulls_for`, `pulls_across`,
`pull`, `comment_on_pull`, `request_merge`, `pull_to_check`, `record_mergeability`, `claim_merge`,
`finish_merge`, `clear_merge`, `set_pull_state`, `claim_repo`, `update_repo`, `forget_repo`,
`repos_for`, the `Repo` struct, and the `repos`/`pulls`/`counters` collection handles.

**Keep `pulls_for` until this task and delete it LAST** — Task 6's `ensure_migrated` is its only
remaining caller. Before deleting it, confirm on the cluster that every repo carries
`meta/pulls_migrated`; if any does not, migration has not finished and this task is not ready.
Do NOT drop the Cosmos collections themselves in this task — leave the data in place as a rollback
path. Dropping them is a separate, later, explicitly-requested operation.

- [ ] **Step 1:** `cargo build` — every deletion must be dead code already. Anything still
      referenced means an earlier task missed a call site; fix that, do not re-add the method.
- [ ] **Step 2:** full `cargo test` green.
- [ ] **Step 3: Commit** — `Retire the Mongo repo and pull collections`

---

### Task 11: Split the long files

**Files:** `src/api.rs` (2974 lines), `src/directory.rs` (1249 lines).

Only now, with the Mongo halves deleted and both files already smaller. Split by seam, following the
`http/browse_api/` precedent (max 355 lines/file) — mechanical moves only, **no behavior change** in
this task, so the diff stays reviewable.

- [ ] **Step 1:** Split `api.rs` into modules by concern (auth/session, repos, pulls, settings,
      images). Target: no file over ~600 lines.
- [ ] **Step 2:** Split `directory.rs` by remaining collection (users/handles, teams, credentials,
      passkeys).
- [ ] **Step 3:** full `cargo test` green; `cargo clippy --lib` no new warnings.
- [ ] **Step 4: Commit** — `Split the api and directory modules`

---

## Final verification

- [ ] `cargo test` — full suite green
- [ ] `cargo clippy --lib` — no new warnings in touched files
- [ ] `./tests/registry_e2e.sh` — exit 0, or 77 with the docker half genuinely skipped (77 is NOT a pass)
- [ ] **Redis-down drill:** with `RUSTIC_GIT_REDIS_URL` unset — PR writes succeed, the owner's sweep
      still finds mergeability work, listings still render from markers
- [ ] **Mongo-down drill:** with the `pulls`/`repos` collections unreachable, a migrated repo's PRs
      still open, list, and merge. This is the proof the truth actually moved.
- [ ] **Migration check on the cluster:** every existing repo carries `meta/pulls_migrated` and its
      PR numbering continues from the old maximum — no restart at 1, no collision
- [ ] `every_browse_route_is_routable` passes with the new `pulls` tail
- [ ] Re-read spec §1/§5/§6: every claim maps to a landed task, and both deviations above are
      recorded in the spec
