# Profile Reuse Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A workspace or clone whose package inputs already have a profile on the node reaches
`PackagesReady` without invoking nix at all.

**Architecture:** The agent already computes `packages::hash(pin, packages)` on every reconcile and
records it per workspace. This adds a node-level index under the profiles directory keyed by that
same hash, consults it before building, drops the workspace id from the derivation name so
identical inputs share one store path, and moves nix's eval cache off the container's overlay.

**Tech Stack:** Rust, Nix (`nix build --expr`), kube-rs, Kubernetes DaemonSet.

**Spec:** `docs/superpowers/specs/2026-08-30-profile-reuse-design.md` — read it first. It records
the measurements this exists to fix (28 s cold / 1.4 s warm / 0.3 s hot for the same expression)
and why the reuse is safe.

## Global Constraints

- The reconcile guarantee is unchanged: a profile matches `spec.packages` because the cache key IS
  `(nixpkgs pin, base packages, spec.packages)` — the three inputs `packages::hash` already covers.
  Nothing may be reused across a change to any of them.
- Never follow a cached link blindly: check that its TARGET exists, not just that the link does.
- `{id}/current` and the pod's `subPath` mount of `{profiles_dir}/{id}` are untouched.
- The janitor sweep is keep-biased like its siblings: an unreadable directory sweeps nothing.
- Comments explain WHY at the density of `bins/server/src/router/route.rs`.
- Commit subjects imperative sentence case, no tool attribution.
- Prefix cargo runs with `CARGO_INCREMENTAL=0` (disk pressure on this host), run them in the
  FOREGROUND with a long timeout, never wait on background monitors.
- `CARGO_INCREMENTAL=0 cargo test --workspace --locked` and
  `CARGO_INCREMENTAL=0 cargo clippy --workspace --all-targets --locked -- -D warnings` green at the
  end of every task. `tests/routing.rs` has a known pre-existing flake under parallel load
  (`a_real_git_push_and_clone_work_through_a_forwarding_node` and its ssh twin) — re-run it alone to
  confirm rather than chasing it.

---

### Task 1: Make the derivation name depend on the inputs, not the workspace

**Files:**
- Modify: `crates/workspaces/src/packages.rs:83-89` (`expression`) and its tests at `:98-140`

**Interfaces:**
- Consumes: `packages::hash(pin, packages) -> String` (exists).
- Produces: `packages::expression(pin, packages) -> String` — the `id` parameter is REMOVED.

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `crates/workspaces/src/packages.rs`:

```rust
    /// Two workspaces with the same inputs must produce the SAME derivation, or the store cannot
    /// share it and a clone rebuilds what its source already has.
    #[test]
    fn the_expression_does_not_depend_on_which_workspace_asked() {
        let a = expression("github:NixOS/nixpkgs/aaaa", &["go".into()]);
        let b = expression("github:NixOS/nixpkgs/aaaa", &["go".into()]);
        assert_eq!(a, b);
        assert!(!a.contains("ws-"), "the workspace id must not reach the derivation name: {a}");
    }
```

- [ ] **Step 2: Run it and watch it fail**

Run: `CARGO_INCREMENTAL=0 cargo test -p kloudlite-git-workspaces the_expression_does_not_depend`
Expected: FAIL — `expression` still takes three arguments, so this does not compile.

- [ ] **Step 3: Drop the id**

In `crates/workspaces/src/packages.rs`, change the signature and the name:

```rust
/// The whole expression `nix build --expr` evaluates. Names arrive validated (`validate_attr`)
/// and are emitted as `pkgs.<name>` inside a list literal — there is no string context in the
/// expression a name could escape into.
///
/// The name carries NO workspace id. It used to (`ws-{id}-env`), which put the id in the
/// derivation and therefore in the store path, so two workspaces with identical inputs built two
/// identical-but-separate profiles and a clone could never reuse its source's. Keyed only on what
/// it contains, one store path serves every workspace that asks for the same set.
pub fn expression(pin: &str, packages: &[String]) -> String {
    let paths: Vec<String> = packages.iter().map(|p| format!("pkgs.{p}")).collect();
    format!(
        "let pkgs = import (builtins.getFlake \"{pin}\") {{ }}; in pkgs.buildEnv {{ name = \"kloudlite-ws-env\"; paths = [ {} ]; }}",
        paths.join(" ")
    )
}
```

- [ ] **Step 4: Fix the existing expression test and the call site**

The existing `the_expression_is_a_list_literal_never_interpolated_text` asserts the old string with
`ws-ws-1-env` — update its expected value to the new name and its call to two arguments. Do NOT
weaken what it checks (that names appear as a list literal, never interpolated text).

Then `cargo check --workspace` and update the one caller in `bins/agent/src/controller.rs`
(`ensure_profile`, the `packages::expression(...)` call) to drop the id argument.

- [ ] **Step 5: Run the tests**

Run: `CARGO_INCREMENTAL=0 cargo test -p kloudlite-git-workspaces -p kloudlite-git-agent-bin`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/workspaces/src/packages.rs bins/agent/src/controller.rs
git commit -m "Key the profile derivation on its contents, not the workspace"
```

---

### Task 2: The node-level index

**Files:**
- Modify: `bins/agent/src/nix.rs` (helpers next to `profile_path`/`publish`, plus tests)

**Interfaces:**
- Consumes: `PROFILES_DIR`, `profile_dir`, `profile_path` (exist).
- Produces:
  - `nix::index_path(root: &Path, hash: &str) -> PathBuf` → `{root}/by-inputs/{hash}`
  - `nix::indexed(root: &Path, hash: &str) -> Option<PathBuf>` — the store path IF the link
    resolves AND its target exists, else `None`
  - `nix::record_index(root: &Path, hash: &str, store_path: &Path) -> std::io::Result<()>`
  - `nix::link_profile(root: &Path, id: &str, store_path: &Path) -> std::io::Result<()>` — point
    `{id}/current` at a store path directly, for the cache-hit path

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `bins/agent/src/nix.rs`:

```rust
    /// A hit is only a hit when the TARGET is still there. A GC that ran while the root was
    /// missing leaves a dangling link, and mounting it would give the pod an empty `bin`.
    #[test]
    fn an_index_entry_whose_target_is_gone_is_a_miss() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let store = root.join("fake-store-path");
        std::fs::create_dir_all(&store).unwrap();
        record_index(root, "abc123", &store).unwrap();
        assert_eq!(indexed(root, "abc123").as_deref(), Some(store.as_path()));

        std::fs::remove_dir_all(&store).unwrap();
        assert!(indexed(root, "abc123").is_none(), "a dangling entry must not be reused");
    }

    /// Writing the same entry twice is what two reconciles racing on one package set do.
    #[test]
    fn recording_an_index_entry_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("store-a");
        std::fs::create_dir_all(&store).unwrap();
        record_index(tmp.path(), "k", &store).unwrap();
        record_index(tmp.path(), "k", &store).unwrap();
        assert_eq!(indexed(tmp.path(), "k").as_deref(), Some(store.as_path()));
    }

    /// The cache-hit path publishes without a build, so it must produce exactly what a build
    /// would have: `{id}/current` pointing at the store path.
    #[test]
    fn linking_a_profile_points_current_at_the_store_path() {
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("store-b");
        std::fs::create_dir_all(&store).unwrap();
        link_profile(tmp.path(), "ws-1", &store).unwrap();
        assert!(profile_exists(tmp.path(), "ws-1"));
        assert_eq!(std::fs::read_link(profile_path(tmp.path(), "ws-1")).unwrap(), store);
    }
```

- [ ] **Step 2: Run them and watch them fail**

Run: `CARGO_INCREMENTAL=0 cargo test -p kloudlite-git-agent-bin index_entry`
Expected: FAIL — `cannot find function 'record_index'`.

- [ ] **Step 3: Implement**

Add to `bins/agent/src/nix.rs`, beside `publish`:

```rust
/// The node's index of built profiles, keyed by `packages::hash` — the same hash the workspace
/// records in its status. Under `PROFILES_DIR`, so it inherits the one GC root that already keeps
/// live profiles from being collected, and it survives an agent restart because that directory is
/// on the host.
pub fn index_path(root: &Path, hash: &str) -> PathBuf {
    root.join("by-inputs").join(hash)
}

/// The store path a previous build produced for these inputs, or `None`.
///
/// The TARGET's existence is what is checked, not the link's: a dangling entry is a miss, never a
/// profile with an empty `bin`.
pub fn indexed(root: &Path, hash: &str) -> Option<PathBuf> {
    let link = index_path(root, hash);
    let target = std::fs::read_link(&link).ok()?;
    std::fs::metadata(&target).ok()?;
    Some(target)
}

/// Record a built profile under its inputs. Idempotent: two reconciles that build the same set
/// write the same link to the same path.
pub fn record_index(root: &Path, hash: &str, store_path: &Path) -> std::io::Result<()> {
    let link = index_path(root, hash);
    if let Some(dir) = link.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = link.with_extension("writing");
    let _ = std::fs::remove_file(&tmp);
    std::os::unix::fs::symlink(store_path, &tmp)?;
    // Rename over the old entry: an index read must never see a half-written link.
    std::fs::rename(&tmp, &link)
}

/// Point `{id}/current` straight at a store path — the cache-hit path, which has no `.building`
/// link to rename. Writes through the same temp-then-rename as `publish` so a pod reading the
/// directory never sees a partial state.
pub fn link_profile(root: &Path, id: &str, store_path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(profile_dir(root, id))?;
    let tmp = building_path(root, id);
    let _ = std::fs::remove_file(&tmp);
    std::os::unix::fs::symlink(store_path, &tmp)?;
    publish(root, id)
}
```

- [ ] **Step 4: Run them and watch them pass**

Run: `CARGO_INCREMENTAL=0 cargo test -p kloudlite-git-agent-bin`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add bins/agent/src/nix.rs
git commit -m "Index built profiles by their inputs"
```

---

### Task 3: Consult the index before building

**Files:**
- Modify: `bins/agent/src/controller.rs` (`ensure_profile`, around the `let current = …` check at
  `:1553` and the build dispatch below it)
- Test: `bins/agent/tests/reconcile.rs`

**Interfaces:**
- Consumes: `nix::{indexed, record_index, link_profile}` (Task 2), `packages::hash` (exists).
- Produces: no new public names.

- [ ] **Step 1: Write the failing tests**

In `bins/agent/tests/reconcile.rs`, in the style of the existing profile tests:

```rust
/// The whole point: a workspace whose inputs already have a profile on this node must reach
/// PackagesReady without nix being asked to build anything.
#[tokio::test]
async fn a_workspace_whose_inputs_are_already_built_does_not_invoke_nix() {
    let (ctx, rec) = ctx_full_default();
    // Seed the index as a previous build would have.
    let store = ctx.profiles_dir.join("seeded-store-path");
    std::fs::create_dir_all(&store).unwrap();
    let hash = kloudlite_git_workspaces::packages::hash(&nixpkgs_pin_for_test(), &base_plus(&["hello"]));
    kloudlite_git_agent::nix::record_index(&ctx.profiles_dir, &hash, &store).unwrap();

    let w: crd::Workspace = serde_json::from_value(ws_json_with_packages(&["hello"])).unwrap();
    kloudlite_git_agent::controller::apply_workspace(&w, &ctx).await.unwrap();

    assert_eq!(fake_nix_builds(&ctx), 0, "an indexed profile must not be rebuilt");
    let st = rec.sent("PATCH", WS_STATUS);
    let last = st.last().expect("a status write");
    assert!(
        last["status"]["conditions"].as_array().unwrap().iter().any(|c| c["type"] == "PackagesReady" && c["status"] == "True"),
        "the workspace is ready on the cached profile"
    );
}

/// A dangling entry must not short-circuit the build, or the pod gets a profile with no bin.
#[tokio::test]
async fn an_index_entry_pointing_at_nothing_still_builds() {
    let (ctx, _rec) = ctx_full_default();
    let hash = kloudlite_git_workspaces::packages::hash(&nixpkgs_pin_for_test(), &base_plus(&["hello"]));
    kloudlite_git_agent::nix::record_index(&ctx.profiles_dir, &hash, &ctx.profiles_dir.join("gone")).unwrap();

    let w: crd::Workspace = serde_json::from_value(ws_json_with_packages(&["hello"])).unwrap();
    kloudlite_git_agent::controller::apply_workspace(&w, &ctx).await.unwrap();

    assert_eq!(fake_nix_builds(&ctx), 1, "a miss builds");
}
```

The helpers `ctx_full_default`, `ws_json_with_packages`, `fake_nix_builds`, `nixpkgs_pin_for_test`
and `base_plus` are named as this plan needs them; use whatever the file already provides for
building a context, a workspace with packages, and counting the fake nix's builds. Match the tree,
do not reshape it. If the fake nix does not already count builds, add the counter to the existing
fake rather than introducing a new one.

- [ ] **Step 2: Run them and watch them fail**

Run: `CARGO_INCREMENTAL=0 cargo test -p kloudlite-git-agent-bin already_built`
Expected: FAIL — the build runs anyway.

- [ ] **Step 3: Consult the index**

In `ensure_profile`, immediately after the existing per-workspace skip:

```rust
    let current = prev.packages.as_ref().and_then(|p| p.observed_hash.as_deref()) == Some(hash.as_str())
        && crate::nix::profile_exists(&ctx.profiles_dir, id);
    if current {
        return Ok(None);
    }
```

add:

```rust
    // Another workspace on this node already built exactly these inputs. The hash covers the pin,
    // the base set and the spec's packages, so an entry under it IS the answer nix would compute —
    // reusing it skips an evaluation of nixpkgs (measured at 28 s cold), not a check.
    if let Some(store_path) = crate::nix::indexed(&ctx.profiles_dir, &hash) {
        let (profiles, wsid) = (ctx.profiles_dir.clone(), id.to_string());
        let sp = store_path.clone();
        tokio::task::spawn_blocking(move || crate::nix::link_profile(&profiles, &wsid, &sp))
            .await
            .map_err(|e| ReconcileErr(format!("link panicked: {e}")))?
            .map_err(|e| ReconcileErr(format!("link profile: {e}")))?;
        let st = packages_status(prev, Some(observed), "Built", "reused a profile already on this node", true, gen);
        write_ws_status_tracking(w, st, prev, ctx).await?;
        return Ok(None);
    }
```

- [ ] **Step 4: Record the index entry after a real build**

Where a finished build is published (the `Ok(_) if !stale` arm that calls `crate::nix::publish`),
record the entry too, after the publish succeeds. The store path is what `{id}/current` now points
at — read it back with `std::fs::read_link(crate::nix::profile_path(&ctx.profiles_dir, id))`. A
failure to record is logged and nothing more: the profile is correct, only the sharing is lost.

- [ ] **Step 5: Run them and watch them pass**

Run: `CARGO_INCREMENTAL=0 cargo test -p kloudlite-git-agent-bin`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add bins/agent/src/controller.rs bins/agent/tests/reconcile.rs
git commit -m "Reuse a profile another workspace already built"
```

---

### Task 4: Sweep stale index entries

**Files:**
- Modify: `bins/agent/src/janitor.rs` (a sweep beside `janitor_sweep_attach`, and its beat)
- Test: the `mod janitor_tests` in the same file

**Interfaces:**
- Consumes: `nix::PROFILES_DIR`, `nix::index_path` (Task 2).
- Produces: `janitor_sweep_profiles(profiles: &Path, min_age: Duration) -> usize`.

Index entries are GC roots, so an entry nothing points at keeps its store path alive forever. This
bounds the set; it is not meant to reclaim quickly.

- [ ] **Step 1: Write the failing tests**

```rust
    /// An entry no workspace's `current` resolves to, older than the bound, is reclaimable.
    #[test]
    fn the_profile_sweep_removes_old_unreferenced_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let store = root.join("store-x");
        std::fs::create_dir_all(&store).unwrap();
        kloudlite_git_agent::nix::record_index(root, "orphan", &store).unwrap();
        assert_eq!(janitor_sweep_profiles(root, std::time::Duration::ZERO), 1);
        assert!(kloudlite_git_agent::nix::indexed(root, "orphan").is_none());
    }

    /// An entry a live workspace points at is never swept, however old.
    #[test]
    fn the_profile_sweep_keeps_entries_a_workspace_uses() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let store = root.join("store-y");
        std::fs::create_dir_all(&store).unwrap();
        kloudlite_git_agent::nix::record_index(root, "used", &store).unwrap();
        kloudlite_git_agent::nix::link_profile(root, "ws-1", &store).unwrap();
        assert_eq!(janitor_sweep_profiles(root, std::time::Duration::ZERO), 0);
        assert!(kloudlite_git_agent::nix::indexed(root, "used").is_some());
    }

    /// Keep-biased, like every other sweep: an unreadable directory reclaims nothing.
    #[test]
    fn the_profile_sweep_sweeps_nothing_when_the_directory_is_unreadable() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(janitor_sweep_profiles(&tmp.path().join("missing"), std::time::Duration::ZERO), 0);
    }
```

- [ ] **Step 2: Run them and watch them fail**

Run: `CARGO_INCREMENTAL=0 cargo test -p kloudlite-git-agent-bin profile_sweep`
Expected: FAIL — `cannot find function 'janitor_sweep_profiles'`.

- [ ] **Step 3: Implement, mirroring `janitor_sweep_attach`**

Read the store paths every `{id}/current` resolves to into a set — bailing keep-biased if the
profiles directory cannot be read — then remove `by-inputs/*` links whose target is not in that set
and whose own mtime is older than `min_age`. An entry that cannot be stat-ed or read is skipped,
never removed.

- [ ] **Step 4: Call it from the beat**

Add it to `janitor_beat` beside the other sweeps, counted into the same tuple and log line, with
`SWEEP_MIN_AGE` as its bound.

- [ ] **Step 5: Run them and watch them pass**

Run: `CARGO_INCREMENTAL=0 cargo test -p kloudlite-git-agent-bin`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add bins/agent/src/janitor.rs
git commit -m "Bound the profile index with a keep-biased sweep"
```

---

### Task 5: Move the eval cache off the overlay, and write it down

**Files:**
- Modify: `deploy/k3s/agent-daemonset.yaml` (agent container env)
- Modify: `CLAUDE.md` ("Workspaces and environments")

**Interfaces:** none.

- [ ] **Step 1: Point the cache at the host**

In the agent container's `env`, beside the existing `NIX_REMOTE`/`WS_NIXPKGS` entries:

```yaml
            # Nix's evaluation cache. Without this it lands in the container's overlay and dies
            # with the pod, so the first profile build after every agent roll re-fetches and
            # re-evaluates nixpkgs — measured at 28 s, against 0.3 s warm. `/nix` is a hostPath
            # this container already mounts, so the cache outlives the pod. ~400 MB.
            - name: XDG_CACHE_HOME
              value: /nix/var/kloudlite/cache
```

- [ ] **Step 2: Check the manifest parses**

Run: `python3 -c "import yaml,sys; list(yaml.safe_load_all(open('deploy/k3s/agent-daemonset.yaml')))"`
Expected: no output, exit 0.

- [ ] **Step 3: Document the mechanism**

In `CLAUDE.md`, in "Workspaces and environments", after the packages/profile sentences:

```markdown
A profile is keyed by `packages::hash(pin, base + spec.packages)` and indexed per node at
`{PROFILES_DIR}/by-inputs/{hash}` → the store path, so a second workspace or a clone with the same
inputs is published straight from the index and never invokes nix (an evaluation of nixpkgs costs
~28 s cold, ~0.3 s warm). A dangling entry is a miss, never a profile with an empty `bin`; the
janitor sweeps entries no `{id}/current` resolves to. The derivation name carries no workspace id —
it used to, which is what stopped two identical package sets sharing one store path.
```

- [ ] **Step 4: Commit**

```bash
git add deploy/k3s/agent-daemonset.yaml CLAUDE.md
git commit -m "Keep nix's eval cache across agent restarts"
```

---

## Self-review

**Spec coverage.** Derivation name → Task 1. The index and its safety rules → Tasks 2 and 3.
Pruning → Task 4. Eval cache → Task 5. The spec's failure table: dangling target (Task 2's test and
Task 3's), idempotent double-write (Task 2), spec edit mid-build (unchanged code, the existing
`started_from != hash` check), unwritable profiles dir (Task 3 step 4 logs and continues), nix
daemon down (untouched).

**Not covered on purpose.** The spec's final test — measuring a real clone on the cluster after
deploy — is a verification step for the deploy, not a task here; it needs a running agent.

**Type consistency.** `packages::expression(pin, packages)` (2 args) in Task 1 matches its Task 3
call site. `indexed`/`record_index`/`link_profile`/`index_path` in Task 2 are used with those exact
names and argument orders in Tasks 3 and 4. `packages::hash(pin, packages)` is unchanged throughout.

**Known soft spot.** Task 3's test helper names are written as this plan needs them; the file's real
fixtures may differ, and the implementer is told to match the tree. Task 4's sweep body is described
rather than written out because it must mirror `janitor_sweep_attach`'s current shape, which is the
authority on the keep-biased idiom.
