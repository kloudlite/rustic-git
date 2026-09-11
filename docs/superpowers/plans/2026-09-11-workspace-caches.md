# Workspace caches: build output in the workspace dir — implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development or
> superpowers:executing-plans, task by task. All edits in the dev pod (`/work/src`); build, test,
> commit and push from there. Never edit while `ship` is building.

**Goal:** per-project build output lives in `{ws}/.cache/`, snapshotted and replicated with the
workspace, ignored from git globally; the NFS home keeps only small config; homecache keeps
global rebuildable caches.

**Architecture:** env and mount changes in the pod builder (`login_env`, `workspace_pod`,
`prelude`), one image file, one default number, one probe id, docs.

**Spec:** `docs/superpowers/specs/2026-09-11-workspace-caches-design.md`

**Decisions taken as defaults (override before Task 1):** default quota 50 GB; Playwright browsers
in the workspace dir; `~/.cargo/registry` and `RUSTUP_HOME` stay on homecache.

## Global constraints

- Paths: workspace dir `/home/kl/workspaces/{name}` (`k8s::workspace_dir`), homecache
  `/home/kl/.local-cache` (`k8s::HOME_CACHE_DIR`), state `/home/kl/.local/state`.
- Global ignore file `/home/kl/.config/git/ignore`, block delimited by the marker line
  `# kloudlite: derived state the platform places inside a workspace directory`, entries exactly
  `.cache/`, `graft/`, `.direnv/`. Never a per-repo `.gitignore` write.
- `cargo clippy --workspace --all-targets -- -D warnings` clean; every `k8s::tests` fixture that
  asserts env vars updated, never loosened.
- Commit subjects imperative sentence case, no tool attribution.

---

### Task 0: Commit the spec and this plan

**Files:** Create `docs/superpowers/specs/2026-09-11-workspace-caches-design.md`,
`docs/superpowers/plans/2026-09-11-workspace-caches.md`.

- [ ] Copy `/tmp/workspace-caches-spec.md` and this file into the pod at those paths (pipe through `kubectl exec -i`).
- [ ] Commit: `Spec and plan: build output in the workspace dir, global git ignore`.

### Task 1: The env table

**Files:** Modify `crates/workspaces/src/k8s/workspace.rs` (`login_env`), Test `crates/workspaces/src/k8s/tests.rs`.

- [ ] **Failing test** in `k8s/tests.rs`:
```rust
#[test]
fn build_output_lives_in_the_workspace_dir_and_global_caches_on_homecache() {
    let env = super::workspace::login_env("api", "alice", "cr.example");
    let get = |k: &str| env.iter().find(|v| v.name == k).and_then(|v| v.value.clone()).unwrap_or_default();
    assert_eq!(get("CARGO_TARGET_DIR"), "/home/kl/workspaces/api/.cache/cargo-target");
    assert_eq!(get("GOCACHE"), "/home/kl/workspaces/api/.cache/go-build");
    assert_eq!(get("PLAYWRIGHT_BROWSERS_PATH"), "/home/kl/workspaces/api/.cache/ms-playwright");
    assert_eq!(get("GRADLE_USER_HOME"), "/home/kl/.gradle", "credentials live on the home");
    assert_eq!(get("GRADLE_PROJECT_CACHE_DIR"), "", "gradle's own default, {ws}/.gradle");
    assert_eq!(get("TMPDIR"), "/home/kl/.local-cache/tmp");
    for (k, v) in [("YARN_CACHE_FOLDER", "yarn"), ("COMPOSER_CACHE_DIR", "composer"), ("NUGET_PACKAGES", "nuget")] {
        assert_eq!(get(k), format!("/home/kl/.local-cache/{v}"));
    }
    assert_eq!(get("MAVEN_OPTS"), "-Dmaven.repo.local=/home/kl/.local-cache/m2");
    assert_eq!(get("DO_NOT_TRACK"), "1");
    assert_eq!(get("GOMODCACHE"), "/home/kl/.local-cache/gomod", "unchanged: global");
}
```
- [ ] Run: `cargo test -p kloudlite-workspaces --lib k8s::tests::build_output_lives` → FAIL.
- [ ] Implement in `login_env`, replacing the three moved lines and adding the new ones:
```rust
        // Per-project build output belongs WITH the project: snapshotted by push, replicated on
        // the sync beat, so a clone, a restore or a start on another node arrives warm. Under
        // `{ws}/.cache/`, never the tool's own `./target`, so nothing the platform places can
        // collide with a directory a repository versions — and the global git ignore is one line.
        var("CARGO_TARGET_DIR", format!("{}/.cache/cargo-target", workspace_dir(name))),
        var("GOCACHE", format!("{}/.cache/go-build", workspace_dir(name))),
        var("PLAYWRIGHT_BROWSERS_PATH", format!("{}/.cache/ms-playwright", workspace_dir(name))),
        // `GRADLE_USER_HOME` holds `gradle.properties` credentials — config, the home's half —
        // and Gradle's project cache is `{ws}/.gradle` on its own.
        var("GRADLE_USER_HOME", format!("{HOME_DIR}/.gradle")),
        var("TMPDIR", format!("{HOME_CACHE_DIR}/tmp")),
        var("YARN_CACHE_FOLDER", format!("{HOME_CACHE_DIR}/yarn")),
        var("COMPOSER_CACHE_DIR", format!("{HOME_CACHE_DIR}/composer")),
        var("NUGET_PACKAGES", format!("{HOME_CACHE_DIR}/nuget")),
        var("MAVEN_OPTS", format!("-Dmaven.repo.local={HOME_CACHE_DIR}/m2")),
        var("DO_NOT_TRACK", "1".into()),
```
  Remove the old `CARGO_TARGET_DIR`, `PLAYWRIGHT_BROWSERS_PATH`, `GRADLE_USER_HOME` lines and the comment block above `CARGO_TARGET_DIR` that explains the homecache redirect (rewrite it to the three-homes rule in two sentences).
- [ ] Run the test → PASS; run `cargo test -p kloudlite-workspaces --lib k8s` → fix any fixture that asserted the old paths (update the expectation, do not delete the assertion).
- [ ] Commit: `Workspace env: build output under {ws}/.cache, gradle config back on the home`.

### Task 2: Editor server mounts and the tmp directory

**Files:** Modify `crates/workspaces/src/k8s/workspace.rs` (`workspace_pod`, `prelude`), Test `k8s/tests.rs`.

- [ ] **Failing test**:
```rust
#[test]
fn editor_servers_mount_from_homecache() {
    let pod = fixture_pod(); // the existing helper in this file that builds a default-image pod
    let mounts: Vec<(String, String)> = pod.spec.unwrap().containers[0].volume_mounts.clone().unwrap()
        .into_iter().filter(|m| m.name == "homecache").map(|m| (m.mount_path, m.sub_path.unwrap_or_default())).collect();
    for (path, sub) in [("/home/kl/.zed_server", "zed-server"), ("/home/kl/.windsurf-server", "windsurf-server"), ("/home/kl/.jetbrains", "jetbrains")] {
        assert!(mounts.contains(&(path.to_string(), sub.to_string())), "{path}");
    }
}
```
- [ ] Run → FAIL. Add the three `VolumeMount`s beside the `.vscode-server` and `.cursor-server` ones (same shape, `sub_path` as above).
- [ ] In `prelude`, after the existing homecache `mkdir`s, add `mkdir -p "$C/tmp"` where `$C` is the homecache mount (match the variable the script already uses; `chmod 1777` is not needed, single user).
- [ ] Run → PASS. Commit: `Workspace pod: Zed, Windsurf and JetBrains servers on homecache; TMPDIR on it too`.

### Task 3: Global git ignore in the image and the append-once prelude

**Files:** Create `deploy/workspace-image/gitignore-global`, Modify `Dockerfile` (workspace stage), `crates/workspaces/src/k8s/workspace.rs` (`prelude`), Test `k8s/tests.rs`.

- [ ] `deploy/workspace-image/gitignore-global`:
```
# kloudlite: derived state the platform places inside a workspace directory
.cache/
graft/
.direnv/
```
- [ ] Dockerfile workspace stage, after the `kl-build.sh` COPY:
```dockerfile
# Derived state the platform places inside a workspace directory, ignored by git GLOBALLY —
# git's default `core.excludesFile`. The prelude appends this block to the person's own file
# if the home already has one; a per-repository `.gitignore` line would be a diff they did not ask for.
COPY deploy/workspace-image/gitignore-global /etc/kloudlite/gitignore-global
```
- [ ] **Failing test**: `prelude("api")` contains the literal `gitignore-global` and the marker line inside a `grep -q`:
```rust
#[test]
fn the_prelude_appends_the_global_ignore_block_once() {
    let s = super::workspace::prelude("api");
    assert!(s.contains("grep -qF '# kloudlite: derived state' \"$H/.config/git/ignore\""));
    assert!(s.contains("cat /etc/kloudlite/gitignore-global >> \"$H/.config/git/ignore\""));
}
```
- [ ] Run → FAIL. In `prelude`, in the `su kl` section that seeds rc files, add:
```sh
mkdir -p "$H/.config/git"
grep -qF '# kloudlite: derived state' "$H/.config/git/ignore" 2>/dev/null || cat /etc/kloudlite/gitignore-global >> "$H/.config/git/ignore"
```
- [ ] Run → PASS. Commit: `Workspace image: global git ignore for .cache/, graft/ and .direnv/, appended once`.

### Task 4: Default quota 50 GB

**Files:** Modify `crates/workspaces/src/crd/snapshot.rs` (`DEFAULT_WS_QUOTA_GB`), `web/apps/web/src/app/(shell)/[owner]/(org)/workspaces/actions.ts` (the create action's default when the form sends none), `web/apps/web/src/lib/api/workspaces.ts` if it carries a default.

- [ ] `grep -rn "20" crates/workspaces/src/crd/snapshot.rs | grep QUOTA` → change `20` to `50`; update the doc comment: "50: a source tree plus its build output, now that `{ws}/.cache` is inside the quota".
- [ ] `cargo test -p kloudlite-workspaces --lib` → fix every assertion that pinned 20 for a default (search `DEFAULT_WS_QUOTA_GB` and literal `20` in `crd/snapshot.rs`, `api/workspaces/mod.rs` tests, `slo` catalogue text if it names the number).
- [ ] Console: `grep -rn "quota_gb\|quotaGb" web/apps/web/src/app/(shell)/[owner]/(org)/workspaces/actions.ts web/apps/web/src/components/app/workspace-list.tsx` — where the form default is set, make it 50; `cd web && bunx tsc --noEmit -p apps/web/tsconfig.json && bun run lint`.
- [ ] Commit: `Default workspace quota is 50 GB: the tree plus its build output`.

### Task 5: Probe `ws.cache.travels`

**Files:** Modify `crates/workspaces/src/slo/catalogue.rs`, `deploy/slo.md`, `bins/slo/src/stages/experience_ws.rs`, tests that count catalogue rows (`grep -rn "catalogue().len()\|const N_SLOS\|expected_count" crates/workspaces/src/slo bins/slo/src tests/`).

- [ ] Catalogue row, beside `ws.seeded`:
```rust
    // Build output lives in the workspace dir since 2026-09-11 and travels with a push: a
    // restore arrives warm. Read on the RESTORED copy, never the source.
    Slo { id: "ws.cache.travels", feature: "Workspaces", sli: "A file written under `{ws}/.cache` before a push is present in a workspace restored from that push", target: p95(240_000), suite: Suite::Hourly, stage: "14 · Experience" },
```
- [ ] `deploy/slo.md`: the matching table row (the catalogue test tells you the exact format on failure).
- [ ] Stage step in `experience_ws.rs`, using the existing kube exec helper `ws.exec.ok` uses (`exec_ok` in `stages/workspace.rs` — lift its exec into a shared `pub(crate) async fn exec_in(c, ws_id, cmd) -> Result<String>` in `stages/mod.rs` if it is private):
```rust
/// `ws.cache.travels`: write a marker under `{ws}/.cache`, push, restore, read it on the copy.
async fn cache_in_tree(c: &mut Ctx, ws: &str) {
    let ws = ws.to_string();
    c.step("ws.cache.travels", Duration::from_secs(240), move |c| async move {
        exec_in(c, &ws, "mkdir -p $KL_WORKSPACE/.cache/cargo-target && echo warm > $KL_WORKSPACE/.cache/cargo-target/marker").await?;
        let snap = push_and_wait(c, &ws).await?;                       // the helper `ws.push.p95` uses
        let restored = restore_and_wait(c, &snap, &format!("{}-cache", c.prefix())).await?; // the helper `ws.restore` uses
        let out = exec_in(c, &restored, "cat $KL_WORKSPACE/.cache/cargo-target/marker").await?;
        anyhow::ensure!(out.trim() == "warm", "restored copy has no marker: {out:?}");
        Ok(())
    }.boxed()).await;
}
```
  Wire it into the stage's `run` after `ws.seeded`, and into the stage's id list so a skip is recorded when the workspace is missing.
- [ ] `cargo test -p kloudlite-workspaces --lib slo` and `cargo test -p kloudlite-slo-bin` → the count tests and the `deploy/slo.md` equality test pass.
- [ ] Commit: `Probe: build output written under the workspace dir survives push and restore`.

### Task 6: Docs and the project guide

**Files:** Modify `docs/product/concepts/workspaces.md`, `docs/product/reference/limits.md`, `CLAUDE.md`.

- [ ] `concepts/workspaces.md` "What is in a workspace" table: row `/home/kl/workspaces/{name}/.cache` — "Build output: Cargo target, Go build cache, browsers" — "Across stop and start; snapshotted by push; ignored by git globally"; the caches row becomes "Package stores and editor servers | `~/.local-cache` | Per node".
- [ ] `reference/limits.md`: default disk 50 GB; a row `Global git ignore | .cache/, graft/, .direnv/ in ~/.config/git/ignore`.
- [ ] `CLAUDE.md` homes paragraph: replace the sentence listing `CARGO_TARGET_DIR` with the three-homes rule (home = small config; workspace dir = the project and what is derived from it, `{ws}/.cache`; homecache = big global rebuildable) and name the global ignore file.
- [ ] Commit: `Docs: where a workspace's caches live`.

### Task 7: Ship, roll, verify

- [ ] `deploy/dev/ship.sh` (full gate) → `deploy/pin.sh <sha> <sha>` in the pod → commit `Pin every tier to <sha>` → push `origin HEAD:master`, `platform HEAD:master`, `platform HEAD:main` → laptop `git pull` → `deploy/roll.sh`; k3s: `kubectl apply -f deploy/k3s/agent-daemonset.yaml` (the workspace image tag is a ClusterSettings/agent value; confirm with `grep -n workspace_image deploy/k3s/agent-daemonset.yaml`).
- [ ] Verify on the fleet: create a workspace, `kubectl exec` → `echo $CARGO_TARGET_DIR` is `/home/kl/workspaces/<name>/.cache/cargo-target`; `git -C $KL_WORKSPACE status --short` shows nothing after `mkdir .cache graft .direnv`; the next hourly run passes `ws.cache.travels` (read from `otel_logs`, `slo.step.done`, `ok=true`).
- [ ] Record the outcome in memory (`[[edit-remotely-in-pod]]` style): what the fleet found that the tests did not.

## Self-review

- Spec coverage: three homes (T1, T2), global ignore (T3), quota (T4), probe (T5), docs (T6), migration = no task (nothing moves; stated in spec). ✓
- Placeholders: none; every code step carries its code. Helper names in T5 (`push_and_wait`, `restore_and_wait`, `exec_in`) are the existing stage helpers by role — the implementer confirms the exact names in `stages/workspace.rs` and `stages/lifecycle.rs` before writing.
- Type consistency: `workspace_dir(name)` returns `String`; `HOME_DIR`, `HOME_CACHE_DIR` are `&str` consts. ✓
