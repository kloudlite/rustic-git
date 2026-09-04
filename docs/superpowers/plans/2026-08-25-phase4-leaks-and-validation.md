# Phase 4: Resource Leaks, Validation and API Polish — Implementation Plan

> **For agentic workers:** execute this plan with `superpowers:subagent-driven-development`
> — one subagent per `### Task`, in order, each task's steps run TDD-strictly
> (failing test → run → implement → run → commit) before the next task starts.

**Goal.** Close the workspaces subsystem's resource leaks and its missing trust-boundary
validation, so a long-lived agent pool stops growing without bound and a typo'd request
fails loudly at the API instead of silently stranding a doc in `creating` forever.

**Architecture.** Nothing structural moves. Every fix lands in the layer that already owns
the concern: input validation at the API trust boundary (`crates/workspaces/src/api.rs`),
local disk reclaim in the agent's janitor (`bins/agent/src/lib.rs`), image/mount lifetime in
the engine (`crates/workspaces/src/engine/ops.rs`), doc lifetime in the metadata store
(`crates/workspaces/src/{store,cosmos}.rs`), and lease/queue bounds on the server work
surface (`bins/server/src/vol_agent.rs`). Two rules from CLAUDE.md constrain every fix here:
**unpushed local data is the only copy and is never swept**, and **pushed history is pure
cache, re-derivable from the registry** — the img/ sweep and the job TTL both key off exactly
that distinction.

**Tech Stack.** Rust (axum 0.8 handler style `Result<Response, Response>`), `azure_data_cosmos`
0.30 (verified against the vendored source at
`~/.cargo/registry/src/index.crates.io-*/azure_data_cosmos-0.30.0`: `ContainerProperties::default_ttl`
is `Option<Duration>`, and `ContainerClient::replace` is how an existing container gets one),
`btrfs`/`losetup` shell-outs through `engine::ops::run`, `tokio`, `serde`.

**Audit findings covered** (`docs/superpowers/audit-2026-08-25.md` §4):

| Finding | Task |
|---|---|
| M2 — `{pool}/img/{blob}.img` never deleted; janitor never sweeps `img/`; leaked throwaway mount on umount failure | 1 |
| M3 — stale `{id}.squashing` latch permanently disables auto-squash | 2 |
| M4 — restarted agent with a dead persisted id loops on 404 forever | 3 |
| M5 — unvalidated `region`/`name`; `quota_gb` enforced nowhere | 4, 5 |
| M8 — push/start/stop/clone/delete operate on `Deleted` docs | 6 |
| H4 (workspaces) — team env history/refs always empty (`get_history(caller, …)`) | 7 |
| M6 + M12 (perf) — unbounded job docs/history/lineage, no pagination, `queued_jobs` drains all pages | 8 |
| Low batch — `LineageEntry::parse` panic, `uuid()` unwrap, `is_mountpoint` octal escapes, `pull_core` `d[0]` | 9 |

---

## Global Constraints

- `cargo clippy --workspace -- -D warnings` must be clean. `--all-targets` has pre-existing
  lints in test targets; the bar there is **no new warnings in files you touch**.
- `cargo test` (workspace) passes. Cosmos-gated tests self-skip without `COSMOS_*`; btrfs-gated
  tests self-skip via `have_btrfs()` — a skip is not a pass, note it in the task's commit body
  if the only coverage you added skips locally.
- **No API break without a documented migration.** Every new request field is optional with a
  serde default; every new response field is additive. `quota_gb` stays on the wire (Task 5
  makes it real rather than removing it). New query params (`?limit=`, `?after=`) are
  `#[serde(default)]` so existing callers are unchanged.
- Comments explain WHY, never what. Match the density of `bins/server/src/router/route.rs`.
- Deliberate shortcuts get a `// ponytail: <ceiling and upgrade path>` marker; keep existing
  markers when editing near one.
- Commit subjects: imperative sentence case, no tool attribution, no `Co-Authored-By`.

---

## File Structure

| File | Responsibility in this plan |
|---|---|
| `crates/workspaces/src/engine/ops.rs` | Delete the squash image after upload; never leak the throwaway `/tmp/wssquash-*` mount; guard `uuid()` and `pull_core`'s first-chunk index |
| `crates/workspaces/src/engine/pool.rs` | `Pool::img_dir()` accessor; `is_mountpoint` decodes `/proc` octal escapes |
| `crates/workspaces/src/model.rs` | `LineageEntry::parse` returns `Option`; `Job.ttl` field |
| `crates/workspaces/src/api.rs` | Region/name validation, terminal-state guards, `owns_volume` returns the owning namespace, `?limit=`/`?after=` on history and list routes |
| `crates/workspaces/src/store.rs` | `MetaStore::queued_jobs(region, limit)`; `MemStore` mirror of the TOP-N bound |
| `crates/workspaces/src/cosmos.rs` | `SELECT TOP n` for queued jobs; `jobs` container `default_ttl`; `replace` an existing container to apply it |
| `crates/workspaces/src/lease.rs` | Pass the queue bound through the sweep's re-placement pass |
| `bins/agent/src/lib.rs` | `img/` sweep in the janitor; re-register on a 404 from `work`; qgroup quota application |
| `bins/agent/src/main.rs` | Clear the squash latch when the child errors out |
| `bins/server/src/vol_agent.rs` | Bounded `queued_jobs` call; stamp `ttl` on terminal job docs |
| `crates/workspaces/tests/api_user.rs` | Validation + terminal-state route tests |
| `crates/workspaces/tests/api_volumes.rs` | Team-namespace history/refs test, pagination test |
| `crates/workspaces/tests/engine_ops.rs` | Squash image cleanup test (btrfs-gated) |
| `bins/agent/tests/loop.rs` | Agent re-register-on-404 test |

---

### Task 1: Stop leaking block images and throwaway mounts (M2)

**Files:** `crates/workspaces/src/engine/ops.rs`, `crates/workspaces/src/engine/pool.rs`,
`bins/agent/src/lib.rs`, `crates/workspaces/tests/engine_ops.rs`

**Interfaces:**

```rust
// pool.rs
impl Pool {
    /// `{pool}/img` — block-layer images: squash's throwaway build image (deleted as soon as
    /// its bytes are uploaded) and a block-restore's live loop-mount backing file.
    pub fn img_dir(&self) -> PathBuf { self.root.join("img") }
}

// bins/agent/src/lib.rs
fn janitor_sweep_images(engine: &Engine, min_age: std::time::Duration) -> usize;
fn loop_attached(img: &std::path::Path) -> bool;
```

The keep rule is deliberately **not** "referenced by a lineage": a squash's block image is
referenced by the lineage it creates and is still disposable the moment its bytes are in the
object store, so a lineage-keyed sweep would never reclaim the exact files that leak most.
The rule that is both safe and actually reclaims is the janitor's existing one — pushed bytes
are pure cache (`pull_core` re-fetches a block layer on demand) — so an image survives only
while it is **currently attached to a loop device** (i.e. backing a live block-restored
voldir) or is younger than the age floor (a restore or squash still in flight, mirroring the
stage sweep's crash-window tolerance).

- [ ] **Step 1:** Add a failing test `squash_deletes_its_build_image` to
      `crates/workspaces/tests/engine_ops.rs`, modelled on the existing btrfs-gated tests
      there: build a lineage, run `engine.squash(&ws)`, then assert
      `std::fs::read_dir(engine.pool.img_dir()).map(|d| d.count()).unwrap_or(0) == 0` and that
      no `/tmp/wssquash-*` directory the squash created survives.
- [ ] **Step 2:** Run it and watch it fail (on a non-btrfs box it self-skips — say so in the
      commit body and rely on the unit tests in Step 5):
      `cargo test -p kloudlite-git-workspaces --test engine_ops squash_deletes_its_build_image`
- [ ] **Step 3:** In `ops.rs`, make the throwaway mount unconditional-cleanup and delete the
      image after upload:

```rust
        // umount can fail (a lingering child holding the mount, EBUSY under load). Retry once
        // lazily rather than returning early: an un-umounted /tmp/wssquash-* pins a loop device
        // and the image file forever, and a squash failure is not worth leaking a mount over.
        let umounted = run(&["umount", &mnt]).or_else(|e| {
            run(&["umount", "-l", &mnt]).map_err(|_| e)
        });
        let _ = std::fs::remove_dir(&mnt);
        populate?;
        umounted?;

        let f = std::fs::File::open(&img).map_err(EngErr::io)?;
        let (raw, clen, sha) =
            blob::upload_stream(self.store.as_ref(), &format!("layers/{blob_id}.zst"), f).await.map_err(EngErr::other)?;
        // The build image has served its only purpose: its bytes are durable in the object
        // store, and a restore re-fetches them into a fresh `{pool}/img/{blob}.img`. Keeping it
        // grew the pool by one full workspace image per squash, forever.
        let _ = std::fs::remove_file(&img);
```

      and replace every `self.pool.root.join("img")` with `self.pool.img_dir()`.
- [ ] **Step 4:** Run it green:
      `cargo test -p kloudlite-git-workspaces --test engine_ops squash_deletes_its_build_image`
- [ ] **Step 5:** Add a failing unit test in `bins/agent/src/lib.rs`'s `janitor_tests` —
      `sweeps_old_unattached_images_keeps_young_ones`: write two files into `pool.img_dir()`,
      backdate one past the age floor with `filetime`-free means (create it, then call the
      sweep with `min_age = Duration::ZERO` for the "old" case and a large `min_age` for the
      "young" case), and assert the sweep's return count and which files survive.
      Run: `cargo test -p kloudlite-git-agent-bin janitor_tests::sweeps_old_unattached_images` (fails).
- [ ] **Step 6:** Implement the sweep in `bins/agent/src/lib.rs` and call it from
      `spawn_janitor`'s tick beside `janitor_sweep_stage`:

```rust
/// Whether `img` is currently backing a loop device — the only state that makes a block image
/// irreplaceable locally (it is the live filesystem under a block-restored voldir). Everything
/// else in `{pool}/img` is re-fetchable from the object store, same "pushed bytes are pure
/// cache" rule the snapshot sweep already applies.
fn loop_attached(img: &std::path::Path) -> bool {
    match std::process::Command::new("losetup").arg("-j").arg(img).output() {
        Ok(out) => !out.stdout.is_empty(),
        // No losetup (or it failed): assume attached and keep the file. The sweep is never
        // allowed to guess in the delete direction.
        Err(_) => true,
    }
}

/// Reclaims `{pool}/img/*.img` left behind by a squash that died before its own delete, or by
/// a block-restore whose voldir has since been unmounted. Age floor: a restore streams its
/// image to disk BEFORE mounting it, so a young unattached image is a materialization in
/// flight, not garbage — same crash-window tolerance the stage sweep needs.
fn janitor_sweep_images(engine: &Engine, min_age: std::time::Duration) -> usize {
    let mut swept = 0;
    let Ok(entries) = std::fs::read_dir(engine.pool.img_dir()) else { return 0 };
    for entry in entries.flatten() {
        let p = entry.path();
        let young = entry
            .metadata()
            .and_then(|m| m.modified())
            .map(|t| t.elapsed().map(|e| e < min_age).unwrap_or(true))
            .unwrap_or(true);
        if young || loop_attached(&p) {
            continue;
        }
        if std::fs::remove_file(&p).is_ok() {
            swept += 1;
        }
    }
    swept
}
```

      In `spawn_janitor`: `let images = janitor_sweep_images(&engine, std::time::Duration::from_secs(3600));`
      and fold `images` into the existing summary eprintln.
- [ ] **Step 7:** Run green: `cargo test -p kloudlite-git-agent-bin janitor_tests` and
      `cargo clippy --workspace -- -D warnings`
- [ ] **Step 8:** Commit:
      `git add -A && git commit -m "Delete the squash build image and sweep stray block images"`

---

### Task 2: Make the squash latch self-healing (M3)

**Files:** `crates/workspaces/src/engine/ops.rs`, `bins/agent/src/main.rs`

**Decision — age, not pid.** Two candidates: write the child's pid and check liveness, or
treat an old latch as stale. Pid liveness needs pid-reuse handling, a `kill(pid, 0)` that
means nothing across a container restart (the agent and the detached child share a pid
namespace only until the pod restarts, after which any pid in the file is meaningless), and
it still cannot tell "child alive but wedged" from "child alive and working". An mtime check
is one `metadata()` call, correct across restarts, and self-heals with no new state. Take the
age check. The **root cause** fix ships with it: `bins/agent/src/main.rs`'s `squash` arm
errors out before `Engine::squash` on a missing owner file or a missing workspace doc, so the
latch it inherits is never cleared — clear it there too, on every exit path.

**Interfaces:**

```rust
// ops.rs
impl Engine {
    /// Path of the "a squash is building for this volume" latch.
    pub fn squash_latch(&self, id: &str) -> std::path::PathBuf;
}
/// Env `WSSNAP_SQUASH_LATCH_SECS`, default 4h.
fn squash_latch_stale_after() -> std::time::Duration;
```

- [ ] **Step 1:** Add a failing unit test in `ops.rs` (`#[cfg(test)] mod latch_tests`):
      `stale_latch_does_not_block_forever` — build an `Engine` over a `tempfile::tempdir()`
      pool, write the latch file, assert `engine.latch_is_stale(id)` is false immediately and
      true when `WSSNAP_SQUASH_LATCH_SECS=0`.
- [ ] **Step 2:** Run it fail: `cargo test -p kloudlite-git-workspaces latch_tests`
- [ ] **Step 3:** Implement in `ops.rs`:

```rust
    /// `{pool}/vol/{id}.squashing` — set before spawning the detached squash child, cleared by
    /// `Engine::squash` (and by the `squash` subcommand's own error path in
    /// `bins/agent/src/main.rs`, which can fail BEFORE ever reaching `Engine::squash`).
    pub fn squash_latch(&self, id: &str) -> std::path::PathBuf {
        self.pool.root.join("vol").join(format!("{id}.squashing"))
    }

    /// A latch older than `WSSNAP_SQUASH_LATCH_SECS` (default 4h) is treated as abandoned: the
    /// child that set it died without clearing it. Chosen over writing the child's pid and
    /// probing liveness — a pid is meaningless after the agent pod restarts, and a stuck latch
    /// silently disables auto-squash for that volume forever, so the failure mode of guessing
    /// "stale" too eagerly (one extra squash, which the ws_lock serializes anyway) is far
    /// cheaper than the failure mode of never guessing it.
    fn latch_is_stale(&self, id: &str) -> bool {
        let ttl: u64 = std::env::var("WSSNAP_SQUASH_LATCH_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(4 * 3600);
        match std::fs::metadata(self.squash_latch(id)).and_then(|m| m.modified()) {
            Ok(t) => t.elapsed().map(|e| e.as_secs() >= ttl).unwrap_or(true),
            // No latch, or an unreadable one: nothing is blocking.
            Err(_) => true,
        }
    }
```

      and in `upload_core` replace the `if latch.exists()` branch with
      `if latch.exists() && !self.latch_is_stale(id) { … } else { … }`, using
      `let latch = self.squash_latch(id);`. Change `Engine::squash` to use `self.squash_latch(&ws.id)`.
- [ ] **Step 4:** Run green: `cargo test -p kloudlite-git-workspaces latch_tests`
- [ ] **Step 5:** Fix the root cause in `bins/agent/src/main.rs`'s `squash`: clear the latch on
      every exit path, including the ones that never reach `Engine::squash`:

```rust
async fn squash(ws_id: Option<&String>) -> Result<(), String> {
    let ws_id = ws_id.ok_or("usage: kloudlite-git-agent squash <ws-id>")?;
    let cfg = Config::from_env();
    // Every early return below happens BEFORE `Engine::squash` (which owns clearing the latch
    // itself), so a missing owner breadcrumb or a deleted workspace doc used to leave the latch
    // set forever and auto-squash permanently off for this volume.
    let latch = std::path::Path::new(&cfg.pool).join("vol").join(format!("{ws_id}.squashing"));
    let r = squash_inner(&cfg, ws_id).await;
    if r.is_err() {
        let _ = std::fs::remove_file(&latch);
    }
    r
}
```

      with the existing body moved verbatim into `squash_inner(cfg: &Config, ws_id: &str)`.
- [ ] **Step 6:** Run: `cargo test -p kloudlite-git-agent-bin && cargo clippy --workspace -- -D warnings`
- [ ] **Step 7:** Commit:
      `git add -A && git commit -m "Clear the squash latch on every failure path and expire a stale one"`

---

### Task 3: Re-register an agent whose server-side doc is gone (M4)

**Files:** `bins/agent/src/lib.rs`, `bins/agent/tests/loop.rs`

`bins/server/src/vol_agent.rs`'s `work` answers `job_not_found()` (404) when no `AgentDoc`
with the polled id exists in the region — which is exactly what a restarted agent reusing
`{pool}/agent-id` hits after the Cosmos doc is gone (database recreated, region re-provisioned,
doc TTL'd). Today that 404 falls into the generic `!is_success()` arm and the agent sleeps and
retries the same dead id forever.

**Interfaces:**

```rust
/// Drops the persisted id so the next `register` mints a fresh one.
fn forget_agent_id(pool: &str);
```

- [ ] **Step 1:** Add a failing test to `bins/agent/tests/loop.rs`
      (`re_registers_after_work_404`): seed `{pool}/agent-id` with `agent-gone`, run the loop
      against the in-process work surface, and assert that within a few seconds a *new*
      `AgentDoc` exists in the `MemStore` and `{pool}/agent-id` holds that new id.
- [ ] **Step 2:** Run it fail: `cargo test -p kloudlite-git-agent-bin --test loop re_registers_after_work_404`
- [ ] **Step 3:** Implement in `bins/agent/src/lib.rs`:

```rust
fn forget_agent_id(pool: &str) {
    let _ = std::fs::remove_file(agent_id_path(pool));
}
```

      make the loop's id rebindable (`let mut agent_id = register(&client, &cfg).await?;`) and
      add the branch above the generic failure arm:

```rust
        // 404 from `work` means the server has no AgentDoc for this id — the persisted
        // `{pool}/agent-id` outlived its doc (Cosmos recreated, region re-provisioned). Retrying
        // the same id can never start succeeding, so drop it and register fresh.
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            eprintln!("agent: work poll: unknown agent {agent_id}, re-registering"); // ponytail: eprintln
            forget_agent_id(&cfg.pool);
            match register(&client, &cfg).await {
                Ok(id) => agent_id = id,
                Err(e) => {
                    eprintln!("agent: re-register: {e}"); // ponytail: eprintln
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
            }
            continue;
        }
```
- [ ] **Step 4:** Run green: `cargo test -p kloudlite-git-agent-bin --test loop`
- [ ] **Step 5:** Commit:
      `git add -A && git commit -m "Re-register the agent when the work poll reports an unknown id"`

---

### Task 4: Validate region and name at the API trust boundary (M5, part 1)

**Files:** `crates/workspaces/src/api.rs`, `crates/workspaces/tests/api_user.rs`

**Interfaces:**

```rust
fn valid_display_name(n: &str) -> bool;
async fn known_region(s: &ApiState, id: &str) -> Result<(), Response>;
```

- [ ] **Step 1:** Add failing tests to `crates/workspaces/tests/api_user.rs`:
      `create_ws_rejects_unknown_region` (400, and no `Workspace` doc and no job created),
      `create_ws_rejects_bad_name` (empty, 200 chars, `"a\u{7}b"` → 400 each), and the same two
      for `POST /v1/environments`.
- [ ] **Step 2:** Run them fail:
      `cargo test -p kloudlite-git-workspaces --test api_user rejects_`
- [ ] **Step 3:** Implement in `api.rs`:

```rust
/// A user-supplied display name (`Workspace.name`, `Environment.name`). Not a path segment and
/// not an id — it never reaches the object store or the pool, so this is a sanity bound, not
/// `valid_segment`: reject empty/whitespace-only, anything over 64 chars, and control
/// characters (which corrupt every log line and terminal listing that echoes the name).
fn valid_display_name(n: &str) -> bool {
    let t = n.trim();
    !t.is_empty() && n.chars().count() <= 64 && !n.chars().any(char::is_control)
}

fn bad_request(msg: &'static str) -> Response {
    (StatusCode::BAD_REQUEST, msg).into_response()
}

/// A region that no `Region` doc names can never be scheduled — `scheduler::schedule` finds no
/// agents in it, the job sits `Queued` forever and the workspace sits `Creating` forever with
/// no error anywhere. Refusing at the boundary is the only place this is still visible to the
/// caller.
async fn known_region(s: &ApiState, id: &str) -> Result<(), Response> {
    let known = s.store.regions().await.map_err(store_err)?.iter().any(|r| r.id == id);
    known.then_some(()).ok_or_else(|| bad_request("unknown region"))
}
```

      Call both at the top of `create_ws` and `create_env`, before any doc is written:

```rust
    if !valid_display_name(&body.name) {
        return Err(bad_request("name must be 1-64 characters with no control characters"));
    }
    known_region(&s, &body.region).await?;
```

      and validate `body.name` the same way in `clone_ws`, `clone_env` and `restore_ws` (they
      all mint a doc from a user-supplied name too).
- [ ] **Step 4:** Run green: `cargo test -p kloudlite-git-workspaces --test api_user`
      (existing tests seed a region doc via `put_region`; any that don't must be updated to —
      that update is the documented migration for this behavior change.)
- [ ] **Step 5:** Commit:
      `git add -A && git commit -m "Reject unknown regions and malformed names at the workspaces API"`

---

### Task 5: Enforce quota_gb with a btrfs qgroup (M5, part 2)

**Files:** `bins/agent/src/lib.rs`, `crates/workspaces/src/engine/ops.rs`

**Decision — enforce, don't remove.** The alternative was dropping `quota_gb` from the request
and response. Removal is the bigger change here, not the smaller one: it is a breaking API
change needing a migration note, the web app and `ws_e2e.sh` both send the field, and it would
leave the actual hazard (one workspace filling a shared pool and taking every other workspace
on that node down with it) wide open. Enforcement is two idempotent shell-outs on a path that
already shells out to `btrfs` constantly, reusing `ops::run`. (If a future pool ever runs on
something other than btrfs, the removal path is: drop the field from `NewWorkspace`, keep
`#[serde(default)]` on `Workspace.quota_gb` so old Cosmos docs still deserialize, and say so in
the API docs — what would bring it back is a per-workspace filesystem that can express a limit.)

**Interfaces:**

```rust
// bins/agent/src/lib.rs
/// Applies `w.quota_gb` as a btrfs qgroup limit on `{pool}/vol/{id}/live`. Idempotent.
fn enforce_quota(engine: &Engine, w: &kloudlite_git_workspaces::model::Workspace);
```

- [ ] **Step 1:** Add a failing btrfs-gated test to `crates/workspaces/tests/engine_ops.rs`
      (`quota_limit_is_visible_on_the_subvolume`): create a subvolume on the `LoopbackPool`,
      apply a 1 GiB limit through the same code path, and assert `btrfs qgroup show -re {live}`
      output contains `1.00GiB`.
- [ ] **Step 2:** Run it fail:
      `cargo test -p kloudlite-git-workspaces --test engine_ops quota_limit_is_visible`
- [ ] **Step 3:** Add the applier to `ops.rs` next to `create_subvol` (the engine already owns
      every other `btrfs` call, and both the agent and the tests need it):

```rust
    /// Caps `id`'s live subvolume at `gb` gigabytes with a btrfs qgroup. Idempotent: `quota
    /// enable` on an already-enabled pool and a repeated `qgroup limit` are both no-ops, so
    /// this is safe to call on every create/clone/restore/start. `gb == 0` means "no quota"
    /// (older docs, and the `MemStore` fixtures) and is skipped rather than treated as zero.
    /// Best-effort by design: a pool without quota support must not fail a workspace create,
    /// so this reports rather than returns — the ceiling is a workspace that can still fill
    /// the pool on such a host.
    /// ponytail: no qgroup on environments (their subvolume has no quota field yet); add the
    /// same call in `EnvUp` if envs ever grow one.
    pub fn set_quota(&self, id: &str, gb: u64) -> Result<(), EngErr> {
        if gb == 0 {
            return Ok(());
        }
        run(&["btrfs", "quota", "enable", self.pool.root.to_str().unwrap()])?;
        run(&["btrfs", "qgroup", "limit", &format!("{gb}G"), self.pool.live(id).to_str().unwrap()])
    }
```
- [ ] **Step 4:** Run green:
      `cargo test -p kloudlite-git-workspaces --test engine_ops quota_limit_is_visible`
- [ ] **Step 5:** Call it from `bins/agent/src/lib.rs` in the `WsCreate`, `WsClone` (workspace
      arm) and `WsRestore` arms, right after the live subvolume exists and before
      `container::start`:

```rust
            // A quota only binds once the subvolume exists, and every one of these arms has just
            // materialized it. Best-effort: an unquotable pool logs and carries on rather than
            // failing an otherwise-healthy create.
            if let Err(e) = engine.set_quota(&w.id, w.quota_gb) {
                eprintln!("agent: quota {}: {e}", w.id); // ponytail: eprintln
            }
```
- [ ] **Step 6:** Run: `cargo test -p kloudlite-git-agent-bin && cargo clippy --workspace -- -D warnings`
- [ ] **Step 7:** Commit:
      `git add -A && git commit -m "Enforce quota_gb as a btrfs qgroup limit on the live subvolume"`

---

### Task 6: Refuse mutations on terminal-state docs (M8)

**Files:** `crates/workspaces/src/api.rs`, `crates/workspaces/tests/api_user.rs`

**Interfaces:**

```rust
fn live_ws(w: &Workspace) -> Result<(), Response>;   // 409 when Deleted
fn live_env(e: &Environment) -> Result<(), Response>;
```

- [ ] **Step 1:** Add failing tests to `crates/workspaces/tests/api_user.rs`:
      `delete_ws_twice_is_a_conflict` (second DELETE → 409, exactly one `WsDelete` job queued)
      and `push_start_stop_clone_on_a_deleted_ws_conflict` (each route → 409, no new job), plus
      the environment mirrors (`delete_env`, `start_env`, `stop_env`, `push_env`, `clone_env`).
- [ ] **Step 2:** Run them fail:
      `cargo test -p kloudlite-git-workspaces --test api_user deleted`
- [ ] **Step 3:** Implement in `api.rs`:

```rust
/// `Deleted` is terminal: the doc is a tombstone (blobs and history are immutable, so the doc
/// is all that is left), and every mutating verb against it either does nothing or — for
/// `delete` — enqueues a second `WsDelete` whose agent-side `cleanup_local` runs against a
/// pool that no longer has the volume. 409 rather than 404: the caller named a real doc, it is
/// just no longer in a state that accepts writes.
fn live_ws(w: &Workspace) -> Result<(), Response> {
    (w.state != WsState::Deleted)
        .then_some(())
        .ok_or_else(|| (StatusCode::CONFLICT, "workspace is deleted").into_response())
}

fn live_env(e: &Environment) -> Result<(), Response> {
    (e.state != EnvState::Deleted)
        .then_some(())
        .ok_or_else(|| (StatusCode::CONFLICT, "environment is deleted").into_response())
}
```

      and add `live_ws(&w)?;` / `live_env(&e)?;` immediately after the `get_ws`/`find_env` in
      `delete_ws`, `start_ws`, `stop_ws`, `push_ws`, `clone_ws` (guard the SOURCE), `restore_ws`
      (guard the source), `delete_env`, `start_env`, `stop_env`, `push_env`, `clone_env`.
- [ ] **Step 4:** Run green: `cargo test -p kloudlite-git-workspaces --test api_user`
- [ ] **Step 5:** Commit:
      `git add -A && git commit -m "Refuse workspace and environment mutations on deleted docs"`

---

### Task 7: Read team-env history from the owning namespace (H4)

**Files:** `crates/workspaces/src/api.rs`, `crates/workspaces/tests/api_volumes.rs`

`owns_volume` already walks the caller's own namespace and then each of their teams — it just
throws away which one matched, and `volume_history`/`volume_refs` then ask the registry for
`get_history(caller, name)`. A team environment's commits were pushed under the TEAM's
namespace (`push_env` passes `e.owner`), so team env history and refs are always empty today.

**Interfaces:**

```rust
/// Returns the namespace the volume's commits live under (the caller, or the owning team).
async fn owns_volume(s: &ApiState, caller: &str, name: &str) -> Result<String, Response>;
```

- [ ] **Step 1:** Add a failing test to `crates/workspaces/tests/api_volumes.rs`
      (`team_env_history_reads_the_team_namespace`): stub the registry with history under
      `("team-a", "env-1")`, create the env doc owned by `team-a`, wire a `MembershipCheck` stub
      putting the caller in `team-a`, then `GET /v1/volumes/env-1/history` and assert the
      records come back (and `/refs` reports the tip) rather than `[]`.
- [ ] **Step 2:** Run it fail:
      `cargo test -p kloudlite-git-workspaces --test api_volumes team_env_history`
- [ ] **Step 3:** Implement — change the three `return Ok(())` sites to return the namespace and
      thread it through both handlers:

```rust
/// A volume `name` is only readable by the caller who owns the workspace or environment it
/// belongs to. Returns the namespace the volume's COMMITS live under, which is not always the
/// caller: a team-owned environment pushed its records under the team slug (`push_env` uses
/// `e.owner`), so reading them back with the caller's own name found an empty history every
/// time — the whole point of returning it rather than `()`.
async fn owns_volume(s: &ApiState, caller: &str, name: &str) -> Result<String, Response> {
    if s.store.get_ws(caller, name).await.map_err(store_err)?.is_some() {
        return Ok(caller.to_string());
    }
    if s.store.get_env(caller, name).await.map_err(store_err)?.is_some() {
        return Ok(caller.to_string());
    }
    for team in teams_for(s, caller).await {
        if s.store.get_env(&team, name).await.map_err(store_err)?.is_some() {
            return Ok(team);
        }
    }
    Err(not_found())
}
```

      In `volume_history` and `volume_refs`:
      `let ns = owns_volume(&s, &owner, &name).await?;` then `reg.get_history(&ns, &name)`.
- [ ] **Step 4:** Run green: `cargo test -p kloudlite-git-workspaces --test api_volumes`
- [ ] **Step 5:** Commit:
      `git add -A && git commit -m "Read volume history from the volume's owning namespace"`

---

### Task 8: Bound job docs, history and list routes (M6 / perf M12)

**Files:** `crates/workspaces/src/model.rs`, `crates/workspaces/src/store.rs`,
`crates/workspaces/src/cosmos.rs`, `crates/workspaces/src/lease.rs`,
`bins/server/src/vol_agent.rs`, `crates/workspaces/src/api.rs`,
`crates/workspaces/tests/api_volumes.rs`

**Interfaces:**

```rust
// model.rs
pub struct Job { /* … */ pub ttl: Option<i64> }   // serde(default, skip_serializing_if = "Option::is_none")

// store.rs
async fn queued_jobs(&self, region: &str, limit: usize) -> Result<Vec<(Job, Etag)>, StoreErr>;

// api.rs
#[derive(serde::Deserialize, Default)]
struct PageQuery { #[serde(default)] limit: Option<usize>, #[serde(default)] after: Option<String> }
```

**8a — TTL on terminal job docs.**

- [ ] **Step 1:** Add a failing unit test in `model.rs`:
      `terminal_job_serializes_a_ttl_and_a_live_one_does_not` — assert `serde_json::to_value` of
      a `Job` with `ttl: None` has no `"ttl"` key, and one with `ttl: Some(604800)` has it.
      Run: `cargo test -p kloudlite-git-workspaces terminal_job_serializes` (fails to compile — the
      field does not exist).
- [ ] **Step 2:** Add the field to `Job` with the WHY comment:

```rust
    /// Cosmos per-item time-to-live, in seconds. Set only when the job reaches a terminal state
    /// (`Done`/`Failed`) — a finished job doc is a receipt nothing reads after the fact, and an
    /// unbounded `jobs` container is pure cost and a slower `queued_jobs` query for every agent
    /// poll. `None` (the live case) is skipped on the wire so the container default applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl: Option<i64>,
```

      Fix every `Job { … }` literal in the crate, its tests and `bins/*` (add `ttl: None`).
- [ ] **Step 3:** Run green: `cargo test -p kloudlite-git-workspaces terminal_job_serializes`
- [ ] **Step 4:** Give the `jobs` container a `default_ttl` in `cosmos.rs` — per-item `ttl` is
      ignored unless the container has TTL enabled, and `ContainerProperties::default_ttl` is
      `Option<Duration>` (no `-1` representation in this SDK), so a long container default plus
      a short per-item override is the shape that works:

```rust
/// Containers that carry a per-item `ttl` need TTL enabled at the container level. The SDK's
/// `default_ttl` is an `Option<Duration>`, so the Cosmos "-1 = enabled, no default" sentinel is
/// not expressible — a long container default is used instead. 90 days is deliberately far
/// past any live job's lifetime: `job_done`/`job_failed` stamp a 7-day per-item ttl that wins,
/// and a job still Queued after 90 days is unschedulable garbage anyway.
const JOBS_DEFAULT_TTL: Duration = Duration::from_secs(90 * 24 * 3600);
```

      Extend `create_container_if_not_exists` with a `default_ttl: Option<Duration>` parameter,
      pass `Some(JOBS_DEFAULT_TTL)` for `jobs` and `None` for the rest, and on the `Conflict`
      arm apply it to the already-existing container (this is the migration for deployed
      databases — additive, no doc rewrite):

```rust
        Err(e) if e.http_status() == Some(StatusCode::Conflict) => {
            // Already exists from an earlier deploy, possibly without TTL enabled. `replace`
            // is the only way to turn it on for an existing container; a failure here is not
            // fatal (TTL is a cost optimization, not a correctness property).
            if default_ttl.is_some() {
                let _ = db.container_client(id).replace(properties, None).await;
            }
            Ok(())
        }
```
- [ ] **Step 5:** Stamp the ttl in `bins/server/src/vol_agent.rs`:

```rust
/// A finished job doc is a receipt: the workspace/environment doc carries the outcome the UI
/// reads, so let Cosmos reclaim these rather than growing the container forever.
const TERMINAL_JOB_TTL_SECS: i64 = 7 * 24 * 3600;
```

      `job_done`: `job.ttl = Some(TERMINAL_JOB_TTL_SECS);` beside `job.state = JobState::Done;`.
      `job_failed`: set it only inside the `if exhausted` branch (a requeued job must keep
      living). Do the same in `crates/workspaces/src/lease.rs` where the sweep sets
      `JobState::Failed`.
- [ ] **Step 6:** Run: `cargo test` (Cosmos tests self-skip without `COSMOS_*`; note that in the
      commit body) and `cargo clippy --workspace -- -D warnings`
- [ ] **Step 7:** Commit:
      `git add -A && git commit -m "Expire finished job docs with a Cosmos TTL"`

**8b — bound `queued_jobs`.**

- [ ] **Step 8:** Add a failing test in `store.rs`'s `mod tests`:
      `queued_jobs_respects_the_limit` — create 5 queued jobs, assert
      `store.queued_jobs("r", 2).await.unwrap().len() == 2`.
      Run: `cargo test -p kloudlite-git-workspaces queued_jobs_respects_the_limit`
- [ ] **Step 9:** Add the `limit` parameter to the trait and both impls:

```rust
    /// `limit` bounds the read, not the queue: `work` only ever leases the FIRST matching job,
    /// so draining every page once per second per agent was pure waste (audit M12). The sweep
    /// passes a larger bound because it re-places every unplaced job.
    async fn queued_jobs(&self, region: &str, limit: usize) -> Result<Vec<(Job, Etag)>, StoreErr>;
```

      `MemStore`: append `.take(limit)` before `.collect()`.
      `CosmosStore`: `format!("SELECT TOP {limit} * FROM c WHERE c.state = 'queued'")` — `limit`
      is a `usize`, so there is nothing injectable in the interpolation.
      `// ponytail: TOP without ORDER BY, so the bound picks an arbitrary page of the queue —
      // fine while `work` leases one job per poll and the sweep re-places on a beat; add an
      // ORDER BY c._ts (and the matching composite index) if queue fairness ever matters.`
- [ ] **Step 10:** Update the two callers: `vol_agent.rs`'s `work` → `queued_jobs(&region.id, 32)`,
      `lease.rs`'s re-placement pass → `queued_jobs(region, 256)`, plus the test call sites.
- [ ] **Step 11:** Run green: `cargo test -p kloudlite-git-workspaces && cargo test -p kloudlite-git-server`
- [ ] **Step 12:** Commit:
      `git add -A && git commit -m "Bound the queued-jobs read with a TOP N query"`

**8c — pagination on history and the list routes.**

- [ ] **Step 13:** Add failing tests to `crates/workspaces/tests/api_volumes.rs`:
      `history_limit_and_continuation` — stub 5 records, `GET …/history?limit=2` returns 2 plus
      a `next` cursor, and `?limit=2&after={next}` returns the following 2 with the newest-first
      order preserved; `history_without_limit_is_unchanged` freezes the existing plain-array
      response for current callers.
      Run: `cargo test -p kloudlite-git-workspaces --test api_volumes history_limit`
- [ ] **Step 14:** Implement in `api.rs`. Keeping the un-paginated response a bare array is what
      makes this additive — the paginated shape only appears when `?limit=` is passed:

```rust
/// Opt-in pagination: `?limit=N` (capped at 200) plus `?after={id}`, the cursor being the last
/// id of the previous page. Absent `limit` keeps the historical un-paginated array response, so
/// no existing caller breaks — the documented migration is "pass ?limit= to get the object
/// shape".
#[derive(serde::Deserialize, Default)]
struct PageQuery {
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    after: Option<String>,
}

/// Slices `items` after the `after` cursor, capped at `limit`, returning the page plus the
/// cursor for the next one (`None` on the last page). `key` names the field the cursor is
/// taken from — the only stable identity these lists have.
fn page<T, K: Fn(&T) -> String>(items: Vec<T>, q: &PageQuery, key: K) -> (Vec<T>, Option<String>) {
    let Some(limit) = q.limit.map(|n| n.clamp(1, 200)) else { return (items, None) };
    let mut it = items.into_iter().peekable();
    if let Some(after) = &q.after {
        // The cursor names the last item of the previous page: skip up to and including it.
        // An unknown cursor yields an empty page rather than silently restarting from the top.
        while let Some(x) = it.next() {
            if key(&x) == *after {
                break;
            }
        }
    }
    let page: Vec<T> = it.take(limit).collect();
    let next = (page.len() == limit).then(|| key(page.last().unwrap()));
    (page, next)
}
```

      `volume_history` becomes:

```rust
    let (records, next) = page(history, &q, |r| r.id.clone());
    if q.limit.is_none() {
        return Ok(Json(records).into_response());
    }
    Ok(Json(serde_json::json!({"records": records, "next": next})).into_response())
```

      Apply the same `Query(q): Query<PageQuery>` + `page(...)` treatment to `list_ws`,
      `list_env` and `list_volumes` (cursor: `w.id` / `e.id` / `v.name`), after the existing
      `Deleted` filtering so a page is never short by a tombstone.
      `// ponytail: the page is sliced after the full store read — it bounds the RESPONSE, not
      // the Cosmos scan. Push `after`/`TOP` into the query when a single owner's list is big
      // enough to matter.`
- [ ] **Step 15:** Run green: `cargo test -p kloudlite-git-workspaces --test api_volumes && cargo test -p kloudlite-git-workspaces --test api_user`
- [ ] **Step 16:** Commit:
      `git add -A && git commit -m "Add opt-in pagination to volume history and the list routes"`

---

### Task 9: The defensive batch — parse, uuid, mountpoints, first chunk (Low)

**Files:** `crates/workspaces/src/model.rs`, `crates/workspaces/src/engine/pool.rs`,
`crates/workspaces/src/engine/ops.rs`

All four are the same shape: an index or an unwrap on data the process does not control (a
lineage file that a crash truncated, `/proc` output, an object-store stream). One task, one
commit.

- [ ] **Step 1:** Add the four failing unit tests:
      `model.rs` → `parse_survives_a_truncated_line`: `LineageEntry::parse("")`,
      `parse("b:only-blob")`, `parse("s:")` all return `None`, and a good line round-trips
      through `encode`/`parse`.
      `pool.rs` → `mountpoint_matches_a_path_with_a_space`: feed the decoder
      `/dev/loop0 /mnt/pool/vol/ws\040one btrfs rw 0 0` and assert it matches
      `/mnt/pool/vol/ws one`.
- [ ] **Step 2:** Run them fail:
      `cargo test -p kloudlite-git-workspaces parse_survives_a_truncated_line mountpoint_matches`
- [ ] **Step 3:** Implement. `model.rs`:

```rust
    /// `None` on a malformed line rather than a panic: the lineage file is plain text on a pool
    /// that can lose power mid-write, and a torn last line used to take the whole agent down
    /// through `Pool::lineage`'s `map`.
    pub fn parse(s: &str) -> Option<LineageEntry> {
        let (s, unpushed) = match s.strip_suffix("|u") {
            Some(rest) => (rest, true),
            None => (s, false),
        };
        let p: Vec<&str> = s.split(':').collect();
        match p.first()? {
            &"b" if p.len() >= 4 => Some(LineageEntry {
                kind: LayerKind::Block,
                blob: p[1].into(),
                snap: Some(p[2].into()),
                sha256: p[3].into(),
                unpushed,
            }),
            &"b" => None,
            _ if p.len() >= 3 => Some(LineageEntry {
                kind: LayerKind::Stream,
                blob: p[1].into(),
                snap: None,
                sha256: p[2].into(),
                unpushed,
            }),
            _ => None,
        }
    }
```

      `pool.rs` — drop the malformed lines instead of the whole file, and decode `/proc`'s octal
      escapes:

```rust
    pub fn lineage(&self, name: &str) -> Vec<LineageEntry> {
        std::fs::read_to_string(self.root.join("vol").join(format!("{name}.lineage")))
            // A torn line is dropped, not fatal: the surviving prefix is still a valid lineage
            // to send/receive against, and refusing to read the file at all would strand the
            // volume completely.
            .map(|s| s.lines().filter_map(LineageEntry::parse).collect())
            .unwrap_or_default()
    }
```

```rust
/// `/proc/self/mounts` escapes space, tab, newline and backslash in octal — a pool path
/// containing a space (a workspace id never has one, but a pool root can) otherwise never
/// matches and `snap_root` silently picks the wrong root.
fn unescape_mount(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() {
            if let Some(c) = std::str::from_utf8(&b[i + 1..i + 4]).ok().and_then(|o| u8::from_str_radix(o, 8).ok()) {
                out.push(c as char);
                i += 4;
                continue;
            }
        }
        out.push(b[i] as char);
        i += 1;
    }
    out
}

pub fn is_mountpoint(p: &std::path::Path) -> bool {
    let mounts = std::fs::read_to_string("/proc/self/mounts").unwrap_or_default();
    mountpoint_in(&mounts, p)
}

/// Split out so the escape handling is testable without a real mount.
fn mountpoint_in(mounts: &str, p: &std::path::Path) -> bool {
    let Some(want) = p.to_str() else { return false };
    mounts.lines().any(|l| l.split_whitespace().nth(1).map(unescape_mount).as_deref() == Some(want))
}
```

      `ops.rs` — `uuid()` and the first-chunk index:

```rust
/// The kernel's uuid source, not a crate: this runs only on the btrfs host, where `/proc` is
/// always there. `Result` rather than `unwrap` anyway — a read failure here used to panic the
/// agent's job thread mid-push.
fn uuid() -> Result<String, EngErr> {
    std::fs::read_to_string("/proc/sys/kernel/random/uuid")
        .map(|s| s.trim().to_string())
        .map_err(EngErr::io)
}
```

      (both call sites become `let layer_id = uuid()?;` / `let blob_id = uuid()?;`)

```rust
                            if is_first_chunk {
                                // The mode byte is the stream's first byte, and a chunked
                                // object-store read can hand back an empty first chunk — indexing
                                // it panicked the whole restore.
                                let Some((&mode, rest)) = d.split_first() else { continue };
                                is_first_chunk = false;
                                d = rest;
                                if mode != b'r' {
                                    dec = Some(zstd::stream::write::Decoder::new(w).map_err(EngErr::io)?);
                                    w = std::io::BufWriter::new(
                                        std::fs::File::open("/dev/null").map_err(EngErr::io)?,
                                    );
                                }
                            }
```
- [ ] **Step 4:** Run green and check every `LineageEntry::parse` caller compiles against the
      new `Option` (the `pool.rs` reader is the only production one; `migrate_tests` and the
      agent's `janitor_tests` construct entries directly):
      `cargo test -p kloudlite-git-workspaces && cargo test -p kloudlite-git-agent-bin && cargo clippy --workspace -- -D warnings`
- [ ] **Step 5:** Commit:
      `git add -A && git commit -m "Harden lineage parsing, uuid reads, mount matching and block-stream decoding"`

---

## Done criteria

- `cargo test` green across the workspace; `cargo clippy --workspace -- -D warnings` clean.
- `./tests/ws_e2e.sh` on a btrfs-capable Linux VM: exit 0, or 77 with the missing prerequisite
  named (77 is not a pass — say which prerequisite was absent).
- Manual check on the production VM after deploy: `{pool}/img` holds only images backing a
  currently-mounted voldir, and `ls {pool}/vol/*.squashing` is empty.
