# Phase 6: Correctness, Quality and Infrastructure — Implementation Plan

> **For agentic workers:** execute this with `superpowers:subagent-driven-development` — one
> subagent per `### Task`, in the order written. Tasks 1–7 are correctness and security and must
> land before the mechanical sweeps (8–11), because the sweeps touch the same files and a
> conflict on a `tracing` rewrite is far more expensive than one on a three-line guard.

## Goal

Close the medium-severity correctness findings in the merge worker, registry and ownership map;
promote the vol-agent token from a fleet-wide shared secret to a region-scoped one; freeze the
secret-redaction invariant with the test CLAUDE.md says is load-bearing; and retire the two
biggest aggregate quality debts (unstructured `eprintln` logging, error-message-substring
branching) plus the CI supply-chain gap and the stale docs. Ends with the web app's minor
findings batched into one pass.

Nothing here changes a protocol, a storage layout, or a public route shape. Every fix is either
a guard, a rename of an existing behaviour, or a mechanical substitution.

## Architecture

Four independent seams, touched in this order:

1. **`crates/pulls` merge worker + jobs** — the outcome recorded for a merge must match what
   actually landed in the base. The existing "already merged" ancestry guard already knows how to
   answer that question; the push-failure path simply never asks it. Same for `commit_tree`'s
   determinism rule, which the `rebase` arm does not apply to itself.
2. **`crates/registry` + `crates/storage`** — three "a bug elsewhere becomes an outage here"
   spots: a poisoning `Mutex` on the manifest hot path, an over-strict digest walk on manifest
   PUT, and a panicking clock read on the ownership claim/renew path.
3. **`bins/server/src/vol_agent.rs`** — the record routes (`commits`, `ref`, `history`) accept any
   region's agent token for any volume. The volume's `{name}` segment IS the workspace/environment
   id, and both docs carry `region`, so scoping is a `get_ws`-or-`get_env` lookup plus one
   comparison. The job routes (`register`, `work`, `jobs/*`) are already region-derived and
   unchanged.
4. **Cross-cutting sweeps** — `tracing` replaces `eprintln`, typed errors replace three
   substring matches, `cargo-deny` and `serde_yml` land in CI/Cargo.toml, CLAUDE.md's layout
   line is corrected, and the web app's four minor findings go in one commit-per-step task.

## Tech Stack

Rust 2021 workspace (`crates/{core,storage,gitbase,pulls,app,git,registry,api,workspaces}`,
`bins/{server,api,worker,agent}`), axum 0.8, tokio, slatedb; `tracing` + `tracing-subscriber`
(new workspace dependencies); `serde_yml` (replacing archived `serde_yaml`);
`EmbarkStudios/cargo-deny-action` in GitHub Actions; Next.js app router + Auth.js under
`web/apps/web`, tested with `bun test`.

## Audit findings covered

From `docs/superpowers/audit-2026-08-25.md` §4 and §5:

| # | Finding | Task |
|---|---------|------|
| 1 | Lease lapse records a landed merge as Refused (`merge_worker.rs:499`) | 1 |
| 2 | Rebase arm does not pin `GIT_COMMITTER_DATE` (`merge_worker.rs:595`) | 2 |
| 3 | `claim_merge` scan variant missing the `state == Open` guard, no callers (`jobs.rs:51`) | 3 |
| 4 | `manifest_cache.lock().unwrap()` on every manifest request (`manifests.rs`, `store.rs:428`) | 4 |
| 5 | Non-digest `digest`-keyed annotation rejects a legitimate manifest (`manifests.rs:133`) | 5 |
| 6 | `now_ms()` panics on a bad clock, on the claim/renew path (`ownership/mod.rs:29`) | 6 |
| 7 | vol-agent tokens are fleet-wide, not region-scoped (`vol_agent.rs:10` ponytail) | 7 |
| 8 | No test for the `local()`/`networked()` secret-redaction invariant | 8 |
| 9 | ~70 `// ponytail: eprintln` markers; no structured logging | 9 |
| 10 | Three seams branch on error message substrings | 10 |
| 11 | Redundant clone (`merge_worker.rs:452`) | 11 |
| 12 | No `cargo deny` in CI; archived `serde_yaml` (RUSTSEC-2024-0320) | 12 |
| 13 | CLAUDE.md workspace-layout line stale ("three binaries", omits workspaces/agent) | 13 |
| 14 | Web: `secureCookies` downgrade, unvalidated slugs into `revalidatePath`, `destroyRepo` confirm, missing `loading.tsx`/error boundaries | 14 |

## Global Constraints

- `cargo clippy --workspace -- -D warnings` must be clean after every task. Test targets are
  excluded from that gate, but the bar in files you touch is **no new warnings**.
- `cargo test` must pass after every task. Do not leave a red test between commits.
- Web changes are gated by `cd web && bun run typecheck && bun run lint && bun run test` — all
  three, every web commit.
- Comments explain **WHY**, never what. Match the density of `bins/server/src/router/route.rs`.
- Deliberate shortcuts keep their `// ponytail: <ceiling and upgrade path>` marker. When a task
  RESOLVES a ponytail marker (task 7 resolves `vol_agent.rs:10`), delete the marker in the same
  commit — a resolved ceiling left documented is worse than none.
- Commit subjects are imperative sentence case, no tool attribution, no `Co-Authored-By`.
- One commit per numbered step unless a step says otherwise. Never squash a failing-test step
  into its implementation step — the plan's TDD order is the review trail.

## File Structure

**Rust — correctness**

| File | Responsibility in this plan |
|------|------------------------------|
| `crates/pulls/src/merge_worker.rs` | Push-failure re-resolution (T1), rebase committer-date pin (T2), secret-redaction test (T8), redundant clone (T11) |
| `crates/pulls/src/pulls/jobs.rs` | Delete the callerless `claim_merge` scan variant (T3) |
| `crates/registry/src/manifests.rs` | Poison-tolerant manifest cache access (T4); filter unparseable digests instead of erroring (T5) |
| `crates/registry/src/store.rs` | `manifest_cache()` accessor; `delete_image`'s prefix retain uses it (T4) |
| `crates/registry/src/gc.rs` | `collect()` stays unpruned — GC over-collecting only keeps blobs alive; referenced by T5's comment |
| `crates/storage/src/auth.rs` | The existing poison-tolerant accessor `auth_cache()` — the shape T4 copies |
| `crates/storage/src/ownership/mod.rs` | `now_ms()` stops panicking (T6) |

**Rust — security**

| File | Responsibility |
|------|----------------|
| `bins/server/src/vol_agent.rs` | Region-scoped record-route auth: `region_of_volume` + `authorized_for` (T7), plus its tests |
| `crates/workspaces/src/store.rs` | `MetaStore::get_ws`/`get_env` — the lookups T7 uses; unchanged |
| `crates/workspaces/src/model.rs` | `Workspace.region` / `Environment.region` — the scope key; unchanged |

**Rust — sweeps**

| File | Responsibility |
|------|----------------|
| `Cargo.toml` (root) | `tracing`/`tracing-subscriber` workspace deps (T9); `serde_yaml` → `serde_yml` (T12) |
| `bins/{server,api,worker,agent}/src/main.rs` | One `init_tracing()` call each (T9) |
| `crates/core/src/log.rs` (new) | The single subscriber initialiser + the levels/targets convention (T9) |
| `crates/core/src/lib.rs` | `pub mod log;` (T9) |
| `crates/storage/src/cache/disk.rs` | Typed `XGroupErr::BusyGroup` instead of the `"BUSYGROUP"` substring (T10) |
| `crates/workspaces/src/engine/mod.rs` (or wherever `EngineErr` lives) | Typed variants for the three cases the tests assert (T10) |
| `crates/workspaces/tests/engine_ops.rs` | Assert on the typed variants, not `contains()` (T10) |
| `bins/server/src/boot.rs` | Admin-command error typed at the one seam its tests match on (T10) |
| `crates/workspaces/src/engine/compose.rs` | `serde_yaml` → `serde_yml` call sites (T12) |
| `.github/workflows/image.yml` | `cargo-deny-action` step (T12) |
| `deny.toml` (new) | advisories/bans/licences/sources config (T12) |
| `CLAUDE.md` | Corrected workspace-layout paragraph (T13) |

**Web**

| File | Responsibility |
|------|----------------|
| `web/apps/web/src/auth.ts` | `secureCookies` refuses to silently downgrade in production (T14) |
| `web/apps/web/src/lib/slug.ts` (new) | `safeSegment()` — the one validator every server action feeds `revalidatePath` through (T14) |
| `web/apps/web/src/lib/slug.test.ts` (new) | Its `bun test` (T14) |
| `web/apps/web/src/app/(shell)/**/actions.ts` | Validate `owner`/`repo` before `revalidatePath` (T14) |
| `web/apps/web/src/app/(shell)/[owner]/[repo]/settings/actions.ts` | `destroyRepo` confirms on `owner/repo` (T14) |
| `web/apps/web/src/app/(shell)/[owner]/[repo]/settings/*.tsx` | The confirm form's label and placeholder follow (T14) |
| `web/apps/web/src/app/(shell)/loading.tsx` (new) | Shell-level navigation skeleton (T14) |
| `web/apps/web/src/app/(shell)/[owner]/loading.tsx` (new) | Owner-level navigation skeleton (T14) |
| `web/apps/web/src/app/(auth)/error.tsx` (new) | Auth segment error boundary (T14) |
| `web/apps/web/src/app/(onboarding)/error.tsx` (new) | Onboarding segment error boundary (T14) |

---

### Task 1: A push that loses its lease must not record a landed merge as refused

Both workers computing the same merge is the normal outcome of a lease lapse: A pushes and wins,
B's `--force-with-lease` fails, and B currently returns `Outcome::refused(stderr_tail(&o))`. The
change is then displayed as failed even though it merged, and because `HeadMoved`/`PullMerged`
fire off `OutcomeState::Merged`, neither event ever happens.

The answer already exists twenty lines up: `merge-base --is-ancestor head base` (for
fast-forward/merge/rebase), and merged-tree == base-tree (for squash). Both were computed against
the base as it was BEFORE the push. So on push failure, re-fetch, re-resolve the base, and re-run
exactly those two checks against the NEW base.

**Files:** `crates/pulls/src/merge_worker.rs`

**Interfaces:**

```rust
/// Did this merge already land, despite our push being refused?
///
/// Only ever called after a `--force-with-lease` failure, which is the shape a lost race takes:
/// another worker computed the same merge from the same base and won. Re-resolves the base from
/// the fleet (ours is stale by definition — the lease failed because the ref moved) and asks the
/// two questions `run` already asks before merging: does the base now contain the head
/// (fast-forward, merge, rebase), or does the merge of the two now produce the base's own tree
/// (squash, which rewrites and so leaves no ancestry behind).
///
/// Answering "no" is the safe default: a `false` here records the refusal git actually gave,
/// which is what a protection rule or a genuinely-moved base deserves.
fn landed_anyway(dir: &Path, url: &str, secret: &str, job: &Job, head_oid: &str) -> Option<String>;
```

Returns the new base oid when the merge is in the base, `None` otherwise. `Option`, not `Result`:
every failure inside it (a fetch that fails, a rev-parse that fails) means "cannot prove it
landed", which is the same answer as "it did not".

- [ ] **Step 1:** In `mod tests`, add `landed_anyway_needs_the_head_in_the_new_base` — a
      `#[test]` that builds a real bare repo in a `tempfile::tempdir()` (git binary, skip via an
      early `if !available() { return; }` so the suite still runs where git is absent), commits a
      base and a head, merges the head into base with `commit_tree`, and asserts
      `landed_anyway(&dir, "", "", &job, &head)` is `Some(new_base)`; then asserts it is `None`
      for a head that was never merged.
- [ ] **Step 2:** Run `cargo test -p rustic-git-pulls landed_anyway` and confirm it fails to
      compile (no such function) — that is the failing state.
- [ ] **Step 3:** Implement `landed_anyway` above `run`. Body: `fetch(dir, url, secret,
      &job.owner, &job.base, &job.head).ok()?` (skipped when `url` is empty so the test can drive
      it purely locally), then `must(dir, &["rev-parse", &format!("refs/heads/{}^{{commit}}",
      job.base)]).ok()?` for the new base oid, then `local(dir, &["merge-base", "--is-ancestor",
      head_oid, &base])` success ⇒ `Some(base)`. For `job.strategy == "squash"`, additionally
      resolve `refs/heads/{base}^{tree}` and compare it against `tree_merge(dir, &base,
      head_oid)`'s `Ok(Ok(t))` — equal trees ⇒ `Some(base)`. Comment WHY the squash arm is
      separate: a squash rewrites, so the head is never an ancestor of what landed.
- [ ] **Step 4:** Run `cargo test -p rustic-git-pulls landed_anyway` — passes.
- [ ] **Step 5:** In `run`, replace the push-failure arm with:

      ```rust
      if !o.status.success() {
          // A lost lease and a refused push look identical from here — git says "stale info"
          // either way. If the merge is already IN the base, someone else computed the same
          // result and won the race, and recording Refused would show a merged change as
          // failed AND swallow HeadMoved/PullMerged. Ask the fleet before believing the error.
          if let Some(base) = landed_anyway(&dir, &url, secret, job, &head_oid) {
              return Ok(Outcome {
                  state: OutcomeState::Merged,
                  detail: Some("already merged".to_string()),
                  new_tip: Some(base),
              });
          }
          // A protection rule, or a base that moved without our work landing. Both are the
          // fleet saying no to a merge that was otherwise fine, and both are the person's to
          // read, so git's own last word is kept.
          return Ok(Outcome::refused(stderr_tail(&o)));
      }
      ```
- [ ] **Step 6:** Add `a_lost_race_records_merged_not_refused` to `mod tests`: same fixture, but
      drive `run` against a bare "upstream" whose base has already been advanced by an
      equivalent merge, and assert `state == OutcomeState::Merged`.
- [ ] **Step 7:** Run `cargo test -p rustic-git-pulls` and `cargo clippy --workspace -- -D warnings`.
- [ ] **Step 8:** Commit: `git commit -am "Record a merge that landed despite a lost lease as merged"`

---

### Task 2: Pin the rebase arm's committer date

`commit_tree` takes author AND committer from the head commit precisely so a retried merge mints
the same commit id. The `rebase` arm pins `GIT_COMMITTER_NAME`/`_EMAIL` from the head but leaves
the DATE to the clock, so every replay produces different ids — which contradicts the module's own
determinism claim and makes a retried rebase push real new commits instead of a no-op.

`--committer-date-is-author-date` is git's own answer: the replayed commits keep their original
author dates and the committer date follows them, so the ids are a pure function of the inputs.

**Files:** `crates/pulls/src/merge_worker.rs`

- [ ] **Step 1:** Add `#[test] a_rebase_is_byte_identical_when_replayed` to `mod tests`: build a
      bare repo, run `rebase(&dir, base, head)` twice (resetting nothing — the function is
      already idempotent on inputs), assert both calls return the same oid.
- [ ] **Step 2:** Run `cargo test -p rustic-git-pulls a_rebase_is_byte_identical` — fails (the
      two runs differ by committer timestamp, or flakily pass within one clock second; if it
      passes, insert a one-second sleep between the runs so the failure is deterministic).
- [ ] **Step 3:** In `rebase`, add `"--committer-date-is-author-date"` to the `rebase` argv,
      immediately after `"rebase"`, with a WHY comment: the replayed commits already keep their
      authors, and pinning the committer date to match is what makes a retried rebase re-mint
      identical ids instead of new ones — the same determinism rule `commit_tree` states.
- [ ] **Step 4:** Run `cargo test -p rustic-git-pulls a_rebase_is_byte_identical` — passes.
- [ ] **Step 5:** Commit: `git commit -am "Pin the rebase committer date so a replay re-mints identical commits"`

---

### Task 3: Delete the callerless `claim_merge` scan variant

`claim_merge` (`crates/pulls/src/pulls/jobs.rs:51`) has no callers — `claim_merge_number` is what
the worker uses, and it is the correct one: it carries the `pr.state != PullState::Open` guard
that `claim_merge` omits, so `claim_merge` would happily claim a merge on a CLOSED change.

YAGNI: adding the missing guard maintains a function nothing calls, and its doc comment already
explains why the by-number twin superseded it ("scanning the repo for 'any queued merge' would
have a worker claim a job some other worker was already nudged about"). Delete it. If a scan is
ever wanted again, it is ten lines around the `takeable` helper, which stays.

**Files:** `crates/pulls/src/pulls/jobs.rs`

- [ ] **Step 1:** Confirm it is dead: `grep -rn "claim_merge\b" --include='*.rs' .` — expect hits
      only in `jobs.rs` itself (the definition and `claim_merge_number`'s doc reference). If any
      other hit exists, STOP and add the `pr.state != PullState::Open` guard instead of deleting.
- [ ] **Step 2:** Delete `pub async fn claim_merge` in full. Keep `takeable` and
      `claim_merge_number`. Amend `claim_merge_number`'s doc: it no longer has a "by-number twin"
      to be the twin of — reword it to state the rule directly ("one named change's merge job,
      claimed; a nudge is about ONE change, and a repo-wide scan would have a worker claim a job
      some other worker was already nudged about").
- [ ] **Step 3:** Run `cargo test` and `cargo clippy --workspace -- -D warnings` — clean, no dead
      `with_merge_jobs` import left behind (if `with_merge_jobs` is now unused, delete it too and
      note that in the commit body).
- [ ] **Step 4:** Commit: `git commit -am "Delete the callerless claim_merge scan variant"`

---

### Task 4: Make the manifest cache poison-tolerant

`app.store.manifest_cache.lock().unwrap()` sits on every manifest GET, PUT and DELETE
(`manifests.rs:179,280,309,384`) and on `store.rs:428`'s `delete_image`. A panic anywhere while
that lock is held poisons it, and every subsequent manifest request then panics into a 500 —
turning one bug into a registry-wide outage. `crates/storage/src/auth.rs:45` already has the
answer and states the reasoning: `.unwrap_or_else(|p| p.into_inner())`, because the map holds
nothing a half-finished insert can leave inconsistent.

**Files:** `crates/registry/src/store.rs`, `crates/registry/src/manifests.rs`

**Interfaces:**

```rust
impl Store {
    /// The manifest cache, poisoning ignored — the same rule and the same reason as
    /// `auth_cache`: a panic while the lock was held (a bug somewhere else) must not turn every
    /// later manifest request into a 500, and the map holds nothing a half-finished insert can
    /// leave inconsistent (bytes and media type are inserted together, under one key).
    pub(crate) fn manifest_cache(&self) -> std::sync::MutexGuard<'_, ManifestCache>;
}
```

Where `ManifestCache` is whatever the field's map type already is
(`HashMap<String, (Bytes, String)>`) — introduce the alias only if the return type is unwieldy at
the call sites; otherwise spell it out and skip the alias.

- [ ] **Step 1:** Add `#[test] a_poisoned_manifest_cache_still_serves` in `crates/registry/src/store.rs`'s
      test module: build a `Store`, poison the lock by panicking inside a
      `std::thread::spawn(|| { let _g = store.manifest_cache(); panic!() }).join()`, then assert
      `store.manifest_cache().len() == 0` does not panic.
- [ ] **Step 2:** Run `cargo test -p rustic-git-registry a_poisoned_manifest_cache` — fails to
      compile (no accessor).
- [ ] **Step 3:** Add the `manifest_cache()` accessor next to the field, with the doc above.
- [ ] **Step 4:** Run `cargo test -p rustic-git-registry a_poisoned_manifest_cache` — passes.
- [ ] **Step 5:** Replace every `manifest_cache.lock().unwrap()` with `manifest_cache()`:
      `manifests.rs` PUT invalidation, GET read, GET insert (the `c.len() >= 256` block — keep
      its existing ponytail marker verbatim), DELETE invalidation, and `store.rs`'s
      `delete_image` prefix `retain`. Verify none remain:
      `grep -rn "manifest_cache.lock()" crates/` returns only the accessor itself.
- [ ] **Step 6:** Run `cargo test` and `cargo clippy --workspace -- -D warnings`.
- [ ] **Step 7:** Commit: `git commit -am "Ignore manifest cache poisoning so one panic is not a registry outage"`

---

### Task 5: Filter unparseable digest strings on manifest PUT instead of refusing the push

`gc::collect` gathers the value of EVERY key named `digest` anywhere in the manifest JSON — which
is correct for GC (over-collecting only keeps blobs alive) but wrong for the PUT-time presence
check, because `annotations` is free-form: a legitimate manifest carrying
`"annotations": {"digest": "sha1-of-our-build-input"}` is refused with `MANIFEST_INVALID`.

The right rule: a string that is not a digest is not a blob reference, so it cannot be a blob this
registry must hold. Filter it out of the presence check. Malformed digests in the STRUCTURAL
positions (`layers[].digest`, `config.digest`, `manifests[].digest`) are a different matter — but
they are caught by the presence check itself: an unparseable structural digest filters out, the
blob it named is never probed, and the push succeeds with an unfetchable layer. That is the one
regression risk, so keep a strict pass over the structural positions and make only the free-form
walk lenient.

**Files:** `crates/registry/src/manifests.rs`

**Interfaces:**

```rust
/// The digests a manifest names in the positions the spec DEFINES as blob references:
/// `config.digest`, `layers[].digest`, `manifests[].digest`, `subject.digest`. Unlike
/// `gc::collect`, which walks every `digest`-keyed value anywhere in the document, this is the
/// set a push is allowed to be strict about — an `annotations` map may legally hold a key called
/// `digest` whose value is a build-input hash, not an OCI descriptor.
fn structural_digests(v: &serde_json::Value) -> Vec<&str>;
```

- [ ] **Step 1:** Add `#[test] a_digest_annotation_is_not_a_blob_reference` to `manifests.rs`'s
      test module: a manifest JSON with a valid `config`/`layers` plus
      `"annotations": {"digest": "not-a-digest", "org.opencontainers.image.revision": "abc"}`,
      asserting `structural_digests` returns exactly the config and layer digests.
- [ ] **Step 2:** Add `a_malformed_layer_digest_is_still_structural`: a manifest whose
      `layers[0].digest` is `"sha256:zzz"`, asserting it IS returned (so the existing
      `Digest::parse` refusal still fires on it).
- [ ] **Step 3:** Run `cargo test -p rustic-git-registry structural_digests` — fails to compile.
- [ ] **Step 4:** Implement `structural_digests`: read `config.digest`, each `layers[].digest`,
      each `manifests[].digest`, and `subject.digest` as `&str`, in that order, skipping absent
      or non-string values. Comment WHY it is not `gc::collect`: GC stays unpruned because
      over-collecting there only keeps blobs alive, whereas over-collecting HERE refuses a
      legitimate push.
- [ ] **Step 5:** In the PUT handler, replace `super::gc::collect(&v, &mut named)` with
      `structural_digests(&v)` feeding the same `digests` vector. Keep the existing `subject`
      removal and the foreign/`urls` layer pruning exactly as they are — they operate on `v`
      before the walk and their reasoning is unchanged. Keep the `MANIFEST_INVALID` refusal for a
      structural digest that does not parse.
- [ ] **Step 6:** Run `cargo test -p rustic-git-registry` and `cargo test --test registry_blobs`
      and `cargo test --test registry_http` — all pass.
- [ ] **Step 7:** Commit: `git commit -am "Check only structural digests when a manifest is pushed"`

---

### Task 6: Stop `now_ms()` panicking on a bad clock

`ownership::now_ms()` is on the ownership claim/renew request path. `.expect("system clock before
1970")` turns a misconfigured or NTP-stepped clock into a panic on the one code path whose whole
job is to keep exactly one node owning a database. `unwrap_or_default()` yields 0, which every
consumer already handles correctly: a zero timestamp reads as "infinitely stale", so a lease
looks lapsed and the claim is re-taken — the safe direction.

**Files:** `crates/storage/src/ownership/mod.rs`

- [ ] **Step 1:** Add `#[test] now_ms_never_panics` asserting `now_ms() > 0` under a normal clock
      (a smoke test; the panic path cannot be forced without moving the system clock — say so in
      a comment).
- [ ] **Step 2:** Run `cargo test -p rustic-git-storage now_ms_never_panics` — passes trivially;
      the real change is the `.expect` removal, which the test guards against regressing.
- [ ] **Step 3:** Replace `.expect("system clock before 1970")` with `.unwrap_or_default()` and
      extend the existing doc comment: a clock before the epoch yields 0, which every consumer
      reads as "infinitely stale" — a lease that looks lapsed is re-claimed, which is the safe
      direction, and far safer than a panic on the claim/renew path.
- [ ] **Step 4:** Run `cargo test -p rustic-git-storage` and `cargo clippy --workspace -- -D warnings`.
- [ ] **Step 5:** Commit: `git commit -am "Answer zero rather than panic when the clock predates the epoch"`

---

### Task 7: Region-scope the vol-agent record routes

`bins/server/src/vol_agent.rs:10` carries a `ponytail:` marker acknowledging that any registered
region's `agent_token` authorizes writes to ANY volume's records. That is a security boundary, not
a convenience shortcut: a leaked agent token from region X can append commits to and move the
`main` ref of any volume in the fleet.

The scoping key is already in the URL. `/vol-agent/{owner}/{name}/…`'s `{name}` IS the
workspace/environment id (volumes are `vol/{owner}/{id}`), and both `Workspace` and `Environment`
carry `region`. The job routes (`register`, `work`, `jobs/*`) already derive their region from the
token via `region_by_token`/`region_by_id` and are unchanged.

Three behaviours must be preserved exactly: `PeerVouched` still short-circuits (a forwarded
request cannot re-validate a region token without Cosmos, and vouches harder anyway); the
`RUSTIC_GIT_VOL_AGENT_TOKENS` break-glass list still works fleet-wide (that is what break-glass
IS — document it, do not scope it); and a node with no `MetaStore` configured keeps refusing.

**Files:** `bins/server/src/vol_agent.rs`

**Interfaces:**

```rust
/// The region a volume belongs to, or `None` when nothing here knows.
///
/// `{name}` in a `/vol-agent/{owner}/{name}/…` path is the workspace or environment ID — volumes
/// are `vol/{owner}/{id}` — so the owning region is the one on that doc. Workspaces first, then
/// environments: the two id spaces are disjoint and workspaces are the common case.
async fn region_of_volume(store: &dyn MetaStore, owner: &str, name: &str) -> Option<String>;

/// Record-route auth, scoped to the volume being written.
///
/// A region's minted `agent_token` now authorizes only volumes in THAT region — a leaked token
/// from one region can no longer rewrite another region's commit history or move its `main` ref.
/// `RUSTIC_GIT_VOL_AGENT_TOKENS` stays fleet-wide by design: it is the break-glass path for
/// standing an agent up when Cosmos is unreachable, and scoping it would need the very lookup
/// that is unavailable in that situation.
///
/// A volume whose doc cannot be found is REFUSED, not allowed: an unknown volume is either a
/// typo or a probe, and the pre-Cosmos era where records existed without docs is over.
async fn authorized_for(jobs: &JobsState, headers: &HeaderMap, owner: &str, name: &str) -> bool;
```

- [ ] **Step 1:** In `mod tests`, add `#[tokio::test] a_region_token_is_refused_for_another_regions_volume`:
      build a `MemStore` with two regions (`r-x` token `tok-x`, `r-y` token `tok-y`) and a
      workspace `w1` owned by `alice` in region `r-y`; assert
      `authorized_for(&jobs, &h_with("tok-y"), "alice", "w1")` is `true` and
      `authorized_for(&jobs, &h_with("tok-x"), "alice", "w1")` is `false`.
- [ ] **Step 2:** Add `an_environment_volume_scopes_the_same_way` (same shape, `create_env` in
      `r-x`, `tok-x` accepted, `tok-y` refused) and
      `break_glass_still_reaches_any_volume` (`RUSTIC_GIT_VOL_AGENT_TOKENS=bg`, accepted for
      both volumes) and `an_unknown_volume_is_refused`.
- [ ] **Step 3:** Run `cargo test -p rustic-git-server region_token` — fails to compile.
- [ ] **Step 4:** Implement `region_of_volume` (try `store.get_ws(owner, name)`, then
      `store.get_env(owner, name)`, mapping each to its `.region`) and `authorized_for`: presented
      token from `bearer_token(headers).or(WS_AGENT_HEADER)`; break-glass first (unchanged);
      otherwise `region_of_volume(...)` → `store.regions()` → find that one region → non-empty
      `agent_token` and `secret_eq`. No fallback to "any region".
- [ ] **Step 5:** Run `cargo test -p rustic-git-server region_token` — passes.
- [ ] **Step 6:** Point `commits`, `move_ref` and `history` at `authorized_for(&jobs, &headers,
      &owner, &name)` instead of `authorized(&jobs, &headers)`. Delete `authorized` if it now has
      no callers; keep `break_glass_matches`.
- [ ] **Step 7:** Rewrite the module doc's v1-contract paragraph: the fleet-wide sentence is no
      longer true. DELETE the `// ponytail: no region scoping yet` marker — the ceiling it named
      is gone. State the new contract and the break-glass exception explicitly.
- [ ] **Step 8:** Update the existing `token_check_rejects_empty_and_mismatched` test to the new
      signature (it uses `JobsState::new(None)`, i.e. no store — assert those calls now refuse
      everything except break-glass, and comment WHY: no store means no region to scope to).
- [ ] **Step 9:** Run `cargo test` and `cargo clippy --workspace -- -D warnings`.
- [ ] **Step 10:** Commit: `git commit -am "Scope vol-agent record routes to the volume's own region"`

---

### Task 8: Freeze the secret-redaction invariant with a test

CLAUDE.md and `merge_worker.rs`'s module doc both say the `local()`/`networked()` split is what
keeps the peer secret out of error messages — and nothing asserts it. One test against a dead port
makes any future refactor that formats a networked argv into an error fail loudly.

**Files:** `crates/pulls/src/merge_worker.rs`

- [ ] **Step 1:** Add to `mod tests`:

      ```rust
      /// The invariant CLAUDE.md names: a networked git call's argv carries the peer secret in
      /// `-c http.extraHeader`, so NOTHING derived from it — an error, a log line, a panic —
      /// may contain that secret. Driven against a closed port so git fails fast and every
      /// failure path this function has is exercised at once.
      #[test]
      fn a_networked_failure_never_names_the_secret() {
          if !available() {
              return;
          }
          const SECRET: &str = "peer-secret-do-not-leak-4f3a91";
          let dir = tempfile::tempdir().expect("tempdir");
          // A bare repo so git gets far enough to attempt the transport.
          assert!(Command::new("git")
              .args(["init", "--bare", "-q"])
              .arg(dir.path())
              .status()
              .expect("git init")
              .success());
          let o = networked(
              dir.path(),
              SECRET,
              "alice",
              &["fetch", "--quiet", "http://127.0.0.1:1/alice/web.git", "+refs/heads/main:refs/heads/main"],
          )
          .expect("the subprocess itself must run");
          assert!(!o.status.success(), "port 1 must not be serving git");
          let said = format!("{}{}", stderr_tail(&o), String::from_utf8_lossy(&o.stdout));
          assert!(!said.contains(SECRET), "the secret reached an error string: {said}");
          // And the shape a caller would actually record.
          let outcome = Outcome::refused(stderr_tail(&o));
          assert!(!format!("{outcome:?}").contains(SECRET), "{outcome:?}");
      }
      ```
- [ ] **Step 2:** Run `cargo test -p rustic-git-pulls a_networked_failure_never_names_the_secret`
      and confirm it passes today (this test freezes existing behaviour rather than driving a
      change — say so in the commit body).
- [ ] **Step 3:** Temporarily break it to prove it bites: add
      `.map_err(|e| err(format!("{args:?}: {e}")))`-shaped leakage into `networked` (or simply
      assert against `format!("{:?}", cmd)` in a scratch edit), confirm the test FAILS, then
      revert the scratch edit. Do not commit the scratch edit.
- [ ] **Step 4:** If `tempfile` is not already a dev-dependency of `crates/pulls`, add it
      (`tempfile = { workspace = true }`, adding it to `[workspace.dependencies]` if absent).
- [ ] **Step 5:** Run `cargo test -p rustic-git-pulls`.
- [ ] **Step 6:** Commit: `git commit -am "Assert a networked git failure never names the peer secret"`

---

### Task 9: Adopt tracing and retire the eprintln markers

~173 `eprintln!` calls across the workspace, ~70 of them carrying `// ponytail: eprintln`
markers, are the single biggest quality debt in aggregate. One `tracing` adoption retires all of
them and gives ops levels and targets instead of grep-able prose.

This is a mechanical sweep and lands AFTER every correctness task, so it never conflicts with a
substantive change.

**Files:** `Cargo.toml`, `crates/core/src/log.rs` (new), `crates/core/src/lib.rs`,
`bins/{server,api,worker,agent}/src/main.rs`, plus every file with an `eprintln!`.

**Interfaces:**

```rust
// crates/core/src/log.rs

/// Install the process's log subscriber. Called exactly once, first thing in each binary's
/// `main`, before anything that can log.
///
/// `RUST_LOG` is the only knob (standard `EnvFilter` syntax: `info`, `rustic_git_git=debug`,
/// `warn,rustic_git_registry::gc=trace`). Default `info`: the fleet's normal volume, with the
/// noisy dependency targets (`hyper`, `h2`, `slatedb`, `aws_*`, `reqwest`) pinned to `warn` so
/// raising our own level to `debug` does not drown it.
///
/// Plain text to stderr, not JSON: the cluster's log pipeline reads lines, and a JSON layer is a
/// one-line change here if that ever stops being true.
pub fn init(service: &str);
```

**Levels and targets convention** (write this into `log.rs`'s module doc, and reference it from
CLAUDE.md's House style section):

- `error!` — a request or job failed in a way a person must act on; a startup precondition unmet.
- `warn!` — a fallback fired: Redis down, a marker reconciled, a lease re-taken, a cache miss
  that costs real work. The thing still worked.
- `info!` — lifecycle only: bind, shutdown, ownership acquired/released, a merge job's outcome.
  One line per event, never per request.
- `debug!` — per-request or per-item detail. Off by default.
- `trace!` — protocol bytes and loop internals.
- Target = the module path (tracing's default). Never invent a target string.
- Structured fields over interpolation: `warn!(owner, name, %e, "xgroup create failed")`, not
  `warn!("xgroup create {owner}/{name} failed: {e}")`.

- [ ] **Step 1:** Add to `[workspace.dependencies]`: `tracing = "0.1"` and
      `tracing-subscriber = { version = "0.3", features = ["env-filter"] }`. Add both to
      `crates/core/Cargo.toml`; add `tracing` alone to every other member that logs.
- [ ] **Step 2:** Write `crates/core/src/log.rs` with `init` as specified plus
      `#[test] init_is_idempotent` (two calls must not panic — use `try_init` internally and
      ignore the second's error, because tests call it too). Add `pub mod log;` to
      `crates/core/src/lib.rs`.
- [ ] **Step 3:** Run `cargo test -p rustic-git-core log::` — passes.
- [ ] **Step 4:** Call `rustic_git_core::log::init("rustic-git")` (and `"rustic-git-api"`,
      `"rustic-git-worker"`, `"rustic-git-agent"`) as the first statement of each binary's `main`.
      Commit this scaffolding alone:
      `git commit -am "Add a tracing subscriber and initialise it in every binary"`
- [ ] **Step 5:** Sweep crate by crate, ONE COMMIT PER CRATE so a bad conversion is bisectable.
      Order: `storage`, `registry`, `pulls`, `api`, `app`, `git`, `gitbase`, `workspaces`, then
      the four bins. For each: replace `eprintln!` with the level the convention above assigns,
      convert interpolation to structured fields, delete the accompanying `// ponytail: eprintln`
      marker (the ceiling it named is gone), and leave every other comment intact.
      Per-crate command: `cargo test -p <crate> && cargo clippy -p <crate> -- -D warnings`.
      Per-crate commit: `git commit -am "Log through tracing in <crate>"`
- [ ] **Step 6:** Leave `eprintln!` alone inside `#[cfg(test)]` blocks and in `tests/*.rs` — test
      output is not ops output. Note that exemption in `log.rs`'s module doc.
- [ ] **Step 7:** Verify the sweep is complete:
      `grep -rn "ponytail: eprintln" crates bins` returns nothing, and
      `grep -rn "eprintln!" crates bins --include='*.rs' | grep -v tests` returns nothing.
- [ ] **Step 8:** Add a "Logging" paragraph to CLAUDE.md's House style section pointing at
      `crates/core/src/log.rs` for the convention. Commit:
      `git commit -am "Document the logging convention"`

---

### Task 10: Type the three errors that are matched by substring

`Box<dyn Error>` workspace-wide is right — it keeps every call site that only propagates from
paying for a taxonomy. But three seams BRANCH on the message text, which means a reworded error is
a silent behaviour change. Type only those three. Everything else stays boxed.

**Files:** `crates/storage/src/cache/disk.rs`, `crates/workspaces/src/engine/` (wherever its error
type lives), `crates/workspaces/tests/engine_ops.rs`, `bins/server/src/boot.rs`

**Seam A — `cache/disk.rs:64`, `"BUSYGROUP"`.** `XGROUP CREATE` on an existing group is normal and
expected; every other failure deserves a log line. Redis errors carry a code, so read the code
instead of the rendered string.

```rust
/// Redis answers `BUSYGROUP` when the consumer group already exists — which is the steady state,
/// not a failure, so it is the one error this swallows. Matched on redis's own error CODE, not
/// the rendered message: a message is prose and prose gets reworded.
fn is_busygroup(e: &redis::RedisError) -> bool {
    e.code() == Some("BUSYGROUP")
}
```

**Seam B — the three `contains()` assertions in `crates/workspaces/tests/engine_ops.rs`**
(`"registry"` at :276, `"history"` at :316, `"sha mismatch"` at :570; also `"chain"`/`"MB"` at
:504 and `"already running"` at :518, which are *reason strings*, not errors — leave those, they
are user-facing prose the test is right to check). Add a small enum to the engine's error type
covering exactly those three cases; keep the boxed variant for everything else.

```rust
/// The engine failures a caller actually branches on. Everything else stays boxed — a taxonomy
/// nothing reads is a maintenance tax with no reader.
#[derive(Debug, PartialEq, Eq)]
pub enum EngineFailure {
    /// The registry tier refused or was unreachable.
    Registry,
    /// The volume has no pushed history to graft onto.
    NoHistory,
    /// A downloaded snapshot's content hash did not match what the record named.
    ShaMismatch,
}
```

**Seam C — `bins/server/src/boot.rs:511`.** Its admin tests match on `"public or private"` and
`"set-image-visibility"`. Give the admin command path a small typed error for the two cases the
tests assert (`BadVisibility`, `FleetUnreachable`) and assert on the variant.

- [ ] **Step 1 (A):** In `disk.rs`'s tests, add `busygroup_is_recognised_by_code` asserting
      `is_busygroup` on a `RedisError` constructed from
      `(redis::ErrorKind::ExtensionError, "BUSYGROUP", "…".to_string())` and NOT on an unrelated
      error whose message happens to contain the word.
- [ ] **Step 2 (A):** `cargo test -p rustic-git-storage busygroup` — fails to compile.
- [ ] **Step 3 (A):** Implement `is_busygroup` and use it in place of
      `e.to_string().contains("BUSYGROUP")`. Run the test — passes.
- [ ] **Step 4 (A):** Commit: `git commit -am "Recognise BUSYGROUP by its redis error code"`
- [ ] **Step 5 (B):** Change the three `assert!(err.0.contains(...))` lines in `engine_ops.rs` to
      assert the typed variant. Run `cargo test -p rustic-git-workspaces` — fails.
- [ ] **Step 6 (B):** Add `EngineFailure` and thread it through the three producing sites only.
      Every other engine error keeps its current boxed form and its current prose.
      Run `cargo test -p rustic-git-workspaces` — passes.
- [ ] **Step 7 (B):** Commit: `git commit -am "Type the three engine failures the tests branch on"`
- [ ] **Step 8 (C):** Same shape for `boot.rs`: change the two assertions first, watch them fail,
      add the typed error, watch them pass.
- [ ] **Step 9 (C):** Commit: `git commit -am "Type the admin visibility command's two failure modes"`
- [ ] **Step 10:** Confirm nothing else branches on prose:
      `grep -rn 'to_string().contains(' crates bins --include='*.rs' | grep -v tests` — empty, or
      every remaining hit is a test asserting user-facing prose (which is legitimate; note them in
      the final commit body).

---

### Task 11: Fix the redundant clone and re-run the lint

`merge_worker.rs:452`'s `head_oid.clone()` in the fast-forward arm clones a `String` that is not
used again on that path. One line, plus the workspace-wide check that nothing else grew one.

Note: Task 1 edits the same function, so this MUST land after it.

**Files:** `crates/pulls/src/merge_worker.rs`, plus whatever the lint finds

- [ ] **Step 1:** Run `cargo clippy --workspace -- -W clippy::redundant_clone 2>&1 | grep -A3 redundant_clone`
      and record every hit.
- [ ] **Step 2:** Fix `merge_worker.rs`'s fast-forward arm (the arm's value can be `head_oid`
      directly once nothing below it reads the binding; if Task 1's `landed_anyway` call now DOES
      read `head_oid` below, keep the clone and instead pass `&head_oid` — verify which by
      reading the post-Task-1 code, do not assume).
- [ ] **Step 3:** Fix every other hit the lint reported, or, where a clone is genuinely required
      (a borrow that outlives the value), leave it with a one-line WHY comment so the next sweep
      does not re-litigate it.
- [ ] **Step 4:** Run `cargo clippy --workspace -- -W clippy::redundant_clone` — clean — then
      `cargo clippy --workspace -- -D warnings` and `cargo test`.
- [ ] **Step 5:** Commit: `git commit -am "Drop the redundant clones the lint finds"`

---

### Task 12: Add cargo-deny to CI and replace the archived serde_yaml

`serde_yaml` 0.9 is archived and carries RUSTSEC-2024-0320. `serde_yml` is the maintained fork
with a compatible API. The only consumer is `crates/workspaces/src/engine/compose.rs`.

**Coordination note:** a separate runtime plan also touches `serde_yaml` in `crates/workspaces`.
The workspace-wide `Cargo.toml` swap and the `compose.rs` call-site updates happen HERE, in one
commit, so that plan can rebase onto a tree where `serde_yml` is already the dependency. If that
plan has already landed its own swap, verify with
`grep -rn serde_yaml Cargo.toml crates bins` and skip Steps 4–6.

`rustsec/audit-check` is already in `image.yml` and covers advisories. `cargo-deny` adds the three
things it does not: banned/duplicate crates, licence policy, and source allowlisting — the reason
the audit called it out separately given the crypto surface (JWT, ssh, pgp, registry auth).

**Files:** `Cargo.toml`, `crates/workspaces/Cargo.toml`,
`crates/workspaces/src/engine/compose.rs`, `deny.toml` (new), `.github/workflows/image.yml`

- [ ] **Step 1:** Write `deny.toml`. Start from `cargo deny init` and then narrow it:
      `[advisories] yanked = "deny"`; `[bans] multiple-versions = "warn"` with an explicit `skip`
      list carrying the three `rand` lines and two `rsa` versions the root `Cargo.toml` already
      documents (reference that comment rather than restating it); `[licenses]` allowing
      MIT/Apache-2.0/BSD-2/BSD-3/ISC/Unicode-3.0/Zlib, everything else denied;
      `[sources] unknown-registry = "deny"`, `unknown-git = "deny"`.
- [ ] **Step 2:** Run `cargo deny check` locally (`cargo install cargo-deny` if absent). Expect a
      RUSTSEC-2024-0320 hit on `serde_yaml` — that is the failing state this task fixes, and it
      proves the config bites.
- [ ] **Step 3:** Add the CI step to `image.yml`'s `test` job, immediately after the existing
      `rustsec/audit-check` step, pinned by SHA like every other action there:

      ```yaml
      # `audit-check` above covers advisories only. This adds the three checks the audit called
      # out for a repo with this much crypto surface: banned/duplicate crates, licence policy,
      # and source allowlisting. Config lives in deny.toml, next to Cargo.toml.
      - uses: EmbarkStudios/cargo-deny-action@<pinned-sha> # v2
        with:
          command: check
      ```
      Resolve the pinned SHA with
      `gh api repos/EmbarkStudios/cargo-deny-action/git/refs/tags/v2 --jq .object.sha`.
- [ ] **Step 4:** Swap the dependency: root `Cargo.toml` `serde_yaml = "0.9"` →
      `serde_yml = "0.0"` (pin the exact current minor), and
      `crates/workspaces/Cargo.toml`'s `serde_yaml = { workspace = true }` →
      `serde_yml = { workspace = true }`.
- [ ] **Step 5:** Update `crates/workspaces/src/engine/compose.rs`'s `serde_yaml::` paths to
      `serde_yml::`. The API is a drop-in; if any call does not compile, fix it at that call and
      note the difference in the commit body.
- [ ] **Step 6:** Run `cargo test -p rustic-git-workspaces` — the compose parsing tests must pass
      unchanged — then `cargo test` and `cargo clippy --workspace -- -D warnings`.
- [ ] **Step 7:** Run `cargo deny check` — now clean.
- [ ] **Step 8:** Commit: `git commit -am "Gate CI on cargo-deny and move off the archived serde_yaml"`

---

### Task 13: Correct CLAUDE.md's workspace layout line

The "Workspace layout" paragraph in `## Commands` omits `crates/workspaces` and `bins/agent`, and
says "three deployed binaries" where four are built (`default-members` lists all four, and
`bins/agent` produces `rustic-git-agent`).

**Files:** `CLAUDE.md`

- [ ] **Step 1:** Verify the ground truth before writing:
      `ls crates bins` and `grep -n "default-members" -A 4 Cargo.toml`.
- [ ] **Step 2:** Replace the paragraph with:

      > Workspace layout: `crates/{core,storage,gitbase,pulls,app,git,registry,api,workspaces}`
      > are the library crates; `bins/{server,api,worker,agent}` build the four deployed binaries
      > (`rustic-git`, `rustic-git-api`, `rustic-git-worker`, `rustic-git-agent` — the agent is
      > root-only and runs one per btrfs-capable box, see "Workspaces and environments"); the root
      > package is `tests/`'s host only, not a facade.
- [ ] **Step 3:** Check the rest of the file for the same staleness:
      `grep -n "three binaries\|three deployed" CLAUDE.md` — the `ws_e2e.sh` comment in
      `## Commands` also says "three binaries"; it is describing which binaries THAT script runs,
      so read it and correct it only if it is actually wrong.
- [ ] **Step 4:** Commit: `git commit -am "Correct the workspace layout line in CLAUDE.md"`

---

### Task 14: Web app minor findings, in one pass

Four small findings, batched because they are all one file each and all gated by the same three
commands. Each gets its own step and its own commit.

**Files:** `web/apps/web/src/auth.ts`, `web/apps/web/src/lib/slug.ts` (new) and its test,
`web/apps/web/src/app/(shell)/**/actions.ts`,
`web/apps/web/src/app/(shell)/[owner]/[repo]/settings/` (actions + form),
`web/apps/web/src/app/(shell)/loading.tsx` (new),
`web/apps/web/src/app/(shell)/[owner]/loading.tsx` (new),
`web/apps/web/src/app/(auth)/error.tsx` (new),
`web/apps/web/src/app/(onboarding)/error.tsx` (new)

**Interfaces:**

```ts
// web/apps/web/src/lib/slug.ts

/** An owner or repo segment safe to build a revalidatePath() from.
 *
 *  revalidatePath takes a PATTERN, so a segment carrying `/`, `[`, `]` or `..` does not just
 *  fail — it silently revalidates something else, or nothing. Server actions read these from
 *  FormData, which is client-controlled, so every one is checked here before it becomes a path.
 *  The rule matches the server's own `valid_segment`/`valid_owner`: ASCII letters, digits,
 *  `-`, `_`, `.`, 1–100 chars, never `.` or `..` alone. */
export function safeSegment(s: string): string | null;

/** Both segments, or null if either fails. The shape every repo-scoped action needs. */
export function safeRepoPath(owner: string, repo: string): { owner: string; repo: string } | null;
```

- [ ] **Step 1:** Write `web/apps/web/src/lib/slug.test.ts` first: `safeSegment` accepts
      `"alice"`, `"my-repo"`, `"a.b_c"`; rejects `""`, `"."`, `".."`, `"a/b"`, `"a[b]"`,
      `"../etc"`, a 101-char string, and a string with a control character or a non-ASCII letter.
      `safeRepoPath` returns null when either half fails.
- [ ] **Step 2:** Run `cd web && bun run test` — fails (module missing).
- [ ] **Step 3:** Write `slug.ts` implementing both, with the doc comments above. Run
      `cd web && bun run test` — passes. Commit:
      `git commit -am "Add a slug validator for paths built from form data"`
- [ ] **Step 4:** Thread it through every server action that calls `revalidatePath` with a value
      read from `FormData`. Find them all with
      `grep -rn "revalidatePath" web/apps/web/src/app`. Pattern, applied uniformly:

      ```ts
      const path = safeRepoPath(String(formData.get("owner") ?? ""), String(formData.get("repo") ?? ""));
      // A bad segment is never a real form submission — the pages that render these forms fill
      // them from the route params. Refuse rather than revalidate a pattern we did not mean.
      if (!path) return { error: "That repository name is not valid." };
      ```
      Then use `path.owner` / `path.repo` for both the API call and the `revalidatePath`.
      Run `cd web && bun run typecheck && bun run lint && bun run test`. Commit:
      `git commit -am "Validate owner and repo before building a revalidate path"`
- [ ] **Step 5:** `destroyRepo` in `(shell)/[owner]/[repo]/settings/actions.ts`: change
      `if (confirm !== repo)` to `if (confirm !== \`${owner}/${repo}\`)` and the error to
      `Type ${owner}/${repo} exactly to confirm.`; update the form's label, placeholder and any
      client-side disabled-button check in the sibling `.tsx` so the three agree — a form asking
      for one string while the action wants another is worse than the original bug. Amend the
      existing doc comment: the fully-qualified name is what makes a muscle-memory `web` typed
      into the wrong tab not delete a different `web`.
      Run the three commands. Commit:
      `git commit -am "Confirm repository deletion on the full owner/repo name"`
- [ ] **Step 6:** `auth.ts`: `secureCookies` must not silently downgrade.

      ```ts
      // An unset AUTH_URL behind a TLS proxy reads as http, which drops `Secure` from the
      // session cookie — the one failure mode here that is invisible in every environment where
      // it does not matter and catastrophic in the one where it does. In production it is a
      // refusal, not a default: the deployment sets it (deploy/rustic-git-web.yaml), so an unset
      // value means a misconfigured rollout, and failing to boot is how that gets noticed.
      const authUrl = process.env.AUTH_URL ?? "";
      if (process.env.NODE_ENV === "production" && !authUrl) {
        throw new Error("AUTH_URL is required in production (without it the session cookie loses `Secure`)");
      }
      export const secureCookies = authUrl.startsWith("https");
      ```
      Run the three commands, plus `cd web && bun run build` to confirm the throw does not fire
      during a build (if it does, gate it on a runtime check inside the NextAuth callback
      instead, and comment WHY). Commit:
      `git commit -am "Refuse to start without AUTH_URL in production"`
- [ ] **Step 7:** Add `(shell)/loading.tsx` and `(shell)/[owner]/loading.tsx`. Copy the shape of
      the existing `(shell)/[owner]/[repo]/loading.tsx` — do not invent a new skeleton style; that
      file is the pattern. Add `(auth)/error.tsx` and `(onboarding)/error.tsx` modelled on the
      existing `(shell)/error.tsx`, with `"use client"` and a `reset` button, worded for a
      person who is mid-sign-in.
      Run the three commands, plus `cd web && bun run build`. Commit:
      `git commit -am "Add loading skeletons and error boundaries for the remaining segments"`
- [ ] **Step 8:** Final gate for the whole task:
      `cd web && bun run typecheck && bun run lint && bun run test && bun run build`.

---

## Done when

- `cargo test` and `cargo clippy --workspace -- -D warnings` pass on a clean tree.
- `cargo deny check` passes; `image.yml` runs it.
- `grep -rn "ponytail: eprintln" crates bins` returns nothing.
- `grep -rn 'to_string().contains(' crates bins --include='*.rs' | grep -v tests` returns nothing.
- `grep -rn serde_yaml .` returns nothing outside `Cargo.lock` history.
- `cd web && bun run typecheck && bun run lint && bun run test && bun run build` all pass.
- CLAUDE.md names four binaries and nine library crates.
- A region-X agent token is refused for a region-Y volume, asserted by a test.
