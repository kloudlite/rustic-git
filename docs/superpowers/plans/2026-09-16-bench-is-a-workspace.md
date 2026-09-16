# A bench is a Workspace — implementation plan

> **For agentic workers:** implemented task by task by Opus implementers dispatched from the main
> session (Fable), which reviews each diff itself. Steps use `- [ ]` for tracking.

**Goal:** retire the `Bench` object model: a bench is a `Workspace` with `spec.bench` set, one per
(person, team), with a volume, packages, kl, tool server and sshd, plus `harness-bench` in a
second container. `/v1/bench*` stays compatible for the shipped desktop.

**Spec:** `docs/superpowers/specs/2026-09-16-bench-is-a-workspace-design.md` — binding. Code map:
`docs/superpowers/specs/2026-09-16-bench-is-a-workspace-map.md` (file:line for everything named
below; read the section for your task before touching code).

## Global constraints

- Worktree `/Volumes/kdisk/rustic-git-wt/desktop-login`, branch `desktop-login`. Prefix every
  shell command with `cd` there. Never `cargo` in `/Users/karthik/rustic-git`.
- Gates: `cargo clippy --workspace --all-targets -- -D warnings`; the touched crates' tests;
  `cd harness && npm run typecheck && npm run bench:test && npm run build` when harness is
  touched; `cd web && bun test` when `deploy/slo.md` or web is touched.
- `is_bench(&Workspace)` (`spec.bench.is_some()`) is the ONLY predicate for "this is a bench".
  Never infer it from the name prefix, a label, or the container list.
- The Bench CRD, its Rust type and `deploy/k3s/crds.yaml` entry STAY in this plan (deleted in a
  later release). Code stops reading/writing Bench except the backfill (Task 6) and the legacy
  folder migration (Task 4).
- `/v1/bench*` responses and status codes are byte-compatible with the shipped desktop
  (`harness/src/connect/bench.ts`) and `kl-connect`; tests pin the exact strings.
- No admin surface, history reflector or audit row ever sees a bench workspace (filter on
  `is_bench`, with a test per surface).
- Comments explain WHY; module docs carry context; files under ~800 lines (split when a file you
  edit crosses it); keep `// ponytail:` markers. Commits imperative sentence case, no attribution.
  Do not push.

---

### Task 1: the object model (crates/workspaces/src/crd)

**Files:** `crd/workspace.rs`, `crd/bench.rs` (move `Access` out; keep `Bench` type + `bench_wants_pod`
for the legacy backfill only), `crd/mod.rs`, `crd/names.rs`, `crd/tests/**`, `tests/crd_yaml.rs`,
`deploy/k3s/crds.yaml` (regenerate), `deploy/k3s/agent-admission.yaml` (verify the spec fence covers
the new fields; no change expected).

**Produces:** `WorkspaceSpec { bench: Option<BenchOptions>, access: Access }`, `BenchOptions
{ model: String, wake_at: Option<String> }` (serde `wakeAt`), `Access { Full, Paused }` (alias
`readOnly`, schema publishes the three wire values exactly as `access_schema` does today),
`WorkspaceStatus.idle_since: Option<String>` (serde `idleSince`), `pub fn is_bench(w) -> bool`,
`pub fn wants_pod(w) -> bool` (bench: Running && access != Paused && (idle_since none || wake_at
> idle_since); non-bench: desiredState == Running), consts `BENCH_IDLE`, `FOLDER_LOCKED`,
`FOLDER_MIGRATED` on the Workspace side.

- [ ] fields + enums + helpers + tests (`wants_pod` truth table copied from `bench_wants_pod`'s
      test; stored `access: readOnly` parses; a spec without `bench` deserialises as before)
- [ ] `bench_id` stays; test `no_workspace_name_generator_yields_a_bench_prefix` (grep the
      generators: `ws-` ids in `api/workspaces/mod.rs`, probe run names).
- [ ] regenerate `deploy/k3s/crds.yaml`; `tests/crd_yaml.rs` green
- [ ] commit `Give Workspace a bench option, an access field and an idle clock`

### Task 2: the pod (crates/workspaces/src/k8s)

**Files:** `k8s/workspace.rs` (`workspace_pod`), `k8s/bench.rs` (shrinks to
`bench_container(id, spec, image, idle_secs, api_url, otlp) -> Container` and
`allow_gateway_bench(ns, id) -> NetworkPolicy`; delete `bench_pod`, `prelude`, `bench_folder`,
`bench_ingress_policy`), `k8s/policies.rs`, `k8s/secrets.rs` (nothing new), `k8s/mod.rs`,
`k8s/tests/{pod,bench}.rs`, `deploy/workspace-image/gitignore-global` (`.bench/`),
`deploy/bench/Dockerfile` (drop zsh, starship, `python3 make g++`; node-pty is removed in Task 3
so the deps stage needs no toolchain), `deploy/bench/{zshrc,starship.toml}` deleted,
`k8s/shell_rc.rs` keeps the constants (workspace prelude uses them; drop the bench file test).

**Consumes:** Task 1. **Produces:** `workspace_pod(...)` gains `bench_image: Option<&str>`
(Some when `is_bench`) and emits the `bench` container per the spec (command `harness-bench --dir
{workspace_dir}/.bench --idle-secs N`, env list from the map §2 plus `KL_WORKSPACE_ID`,
`KL_WORKSPACE`; mounts: live worktree at the workspace path, home, user-key ro, bench-tool Secret
ro optional, tmp; readiness `harness-bench --ping` period 5 s; hardened without SYS_CHROOT);
pod label `KIND_LABEL` = `bench` when `is_bench`. `allow_bench_tools` unchanged.

- [ ] bench container + labels + policy; `a_workspace_pod_never_mounts_the_bench_tool_secret`
      becomes `a_non_bench_pod_never_mounts_the_bench_tool_secret` and a sibling asserts the
      bench pod mounts it optional and carries both containers
- [ ] gitignore-global `.bench/`; Dockerfile slimmed; tests
- [ ] commit `Run harness-bench as a second container of a bench workspace's pod`

### Task 3: harness-bench in the workspace pod (harness/bench)

**Files:** `harness/bench/src/{main.ts,idle.ts,server.ts,pty.ts}`, `harness/bench/test/**`,
`harness/package.json` + lock (remove `node-pty`).

- [ ] `--dir` default: `$KL_WORKSPACE/.bench` when `KL_WORKSPACE` set, else `/bench`.
- [ ] Idle: `Idle` no longer calls `shutdown`; it writes `{dir}/.idle` (RFC 3339) when idle
      begins and deletes it when a client connects or work starts. `GET /healthz` answers
      `{"ok":true,"idle":"<ts>"}` or `{"ok":true}`; `--ping` exits 2 when `idle` is present.
      Exit 0 remains only for SIGTERM/SIGINT. Exit 75 (folder locked) unchanged.
- [ ] `/pty?scope=bench` → `spliceWorkspaceShell(w, "127.0.0.1:7788", first)`; delete
      `attachBenchShell` and the node-pty dependency; `SHELL`/`ZDOTDIR` env no longer read here.
- [ ] Tests: idle file appears/disappears and `--ping` exit codes (drive `Idle` with a fake
      clock as `idle.test.ts` does); healthz shape; bench scope splices to a fake tool server on
      127.0.0.1:7788 (bind the fake to that port only if free; else skip with a message — CI has it
      free); existing pty tests adjusted; `main.test.ts` idle-exit tests become idle-file tests.
- [ ] `npm run typecheck && npm run bench:test` twice; commit `Signal idleness with a file and
      serve the bench shell from the pod's own tool server`

### Task 4: the agent (bins/agent)

**Files:** `controller/workspace/**` (pod verdicts for a bench, wake, idle_since, migration),
`controller/bench.rs` DELETED, `controller/{mod,run}.rs`, `claim.rs` (`claim_bench` gone),
`peer/sweeps.rs` (Bench arm gone), `binding.rs` (Bench list gone; namespace readiness reads
Workspaces), `janitor.rs` (attach keep-set from Workspaces), `controller/workspace/home.rs`
(`ensure_bench_folder`/`delete_bench_folder` → `migrate_bench_folder`), `tests/reconcile/**`
(`bench.rs` rewritten around bench-flagged Workspaces).

**Consumes:** Tasks 1–3. Rules:
- `wants_pod(w)` replaces `desiredState == Running` wherever the workspace reconciler decides on
  a pod. A bench with `!wants_pod` and a pod → delete the pod, phase `Idle` (reason `BENCH_IDLE`)
  when the reason is idleness, `Stopped` otherwise.
- Idle verdict: pod Running, `bench` container `ready == false` for ≥ 2 probe periods AND the
  container is not restarting (`restartCount` unchanged since last pass) → treat as idle: delete
  pod, stamp `status.idleSince = now`. (Readiness false immediately after start is `Starting`,
  not idle: require `lastTransitionTime` of the pod's Ready condition ≥ 15 s ago.)
- Locked verdict: `bench` container `lastState.terminated.exitCode == 75` → phase `Starting`,
  reason `FOLDER_LOCKED`, message from `terminated.message`.
- Migration: on the pass that first finds the worktree mounted and `{worktree}/.bench` absent
  while `{pool}/homes/.benches/{team}/{owner}` exists (needs `ctx.homes_export`): `cp -a` the
  folder minus `.lock*` into `{worktree}/.bench`, `chown -R 1000:1000`, then rename the legacy
  folder to `…/{owner}.migrated-{unix}`; condition `FOLDER_MIGRATED=True`; log
  `bench.folder.migrated {files, bytes}`. Any error → condition False with the io error, retry
  next pass, no pod until migrated (a pod would start an empty bench and race the copy).
- Remove: `reconcile_bench`, `claim_bench`, `mark_parent_of::<Bench>`, Bench in `binding.rs`
  and `janitor.rs`; the run loop no longer watches Bench. The agent must still start on a cluster
  that has Bench objects (it simply does not list them).
- [ ] verdicts + wake + idle_since + tests (`tests/reconcile/bench.rs`: create → pod with two
      containers; idle readiness → pod deleted + idleSince; wake_at newer → pod again; exit 75 →
      FolderLocked; migration copies and renames; paused → no pod)
- [ ] deletions; policy list test in `owner_bindings_and_quota.rs` updated (`allow-gateway-bench`)
- [ ] `cargo test -p kloudlite-agent-bin`, clippy; commit `Reconcile a bench as a workspace with
      a bench container, an idle clock and a one-time folder migration`

### Task 5: API — facade, lists, quota, tool admission (crates/workspaces/src/api)

**Files:** `api/bench.rs`, `api/mod.rs`, `api/workspaces/mod.rs`, `quota.rs`, `model.rs`,
`tests/api_bench*.rs`, `tests/api_workspaces*.rs`.

- [ ] `my_bench` → `Workspace::get_opt(bench_id)` filtered `is_bench && owner`; `create_bench`
      builds a `NewWorkspace`-shaped request (`name: "bench"`, `bench: Some(BenchOptions{model})`,
      region from `team_region`, default storage) and calls the SAME internal create used by
      `POST /v1/workspaces` (extract `create_workspace_inner` if needed); re-POST wakes.
      `start/stop/session/tool-token` operate on the Workspace; `bench_session` wake patches
      `spec.bench.wakeAt` + `desiredState`; `bench_doc` shape unchanged (`model` from
      `spec.bench.model`, `access` from `spec.access`).
- [ ] `GET /v1/workspaces` excludes `is_bench`; `DELETE /v1/workspaces/{id}` on a bench → 409
      "a bench is deleted with your membership, not by hand"; `POST …/stop` on a bench → same
      handler as `/v1/bench/stop`.
- [ ] Quota: `usage` counts non-bench workspaces; a bench adds disk always, cpu+mem while
      `wants_pod && phase != Idle`; `bench_cost` retired; tests.
- [ ] `bench_admits_tool(&Workspace, sub, team)`; `bench_tool_check` gets the Workspace.
- [ ] Compatibility tests: the exact strings/statuses the desktop branches on (404 → create; 409
      body `"bench is stopped; start it"`; 202 `{state}`; 201 `{id, token, gateway, expires_at}`).
- [ ] commit `Serve /v1/bench from the bench workspace and keep benches out of workspace lists`

### Task 6: membership, removals, keys, GC, history, backfill

**Files:** `api/membership/{mod,tests}.rs`, `api/removals.rs`, `api/keys/{mod,prune}.rs`,
`api/workspaces/keys.rs` (`space_of`), `bins/controller/src/gc.rs`, `history/watch.rs`,
`history/events.rs`, `api/admin/{owners,overview}.rs` (filter), `api/bench_backfill.rs` (new),
`bins/api` boot wiring (where the region backfill runs).

- [ ] Pause/unpause: patch `spec.access` (+ `desiredState: stopped` on pause) on EVERY workspace
      of the pair; `delete_tool_secret` stays. Removals mark Workspaces only. GC: bench
      workspaces first, then the rest, then space choice; no bench-folder finalizer wait.
- [ ] Keys: `project_all`, `prune_namespaces`, `space_of` drop their Bench branches (a bench IS a
      Workspace with `OWNER_LABEL`).
- [ ] History: `watch.rs` mappers and `events.rs` skip `is_bench`; test that a bench workspace
      produces no row. Admin owners/overview counts exclude benches; test.
- [ ] Backfill (`bench.backfill`, admin role boot, idempotent, one pass per boot + every 10 min
      until zero legacy objects): for each legacy `Bench`: if no Workspace named `bench_id`
      exists → create it (owner, team, region from directory or the legacy pod's node region,
      `access`, `desiredState`, `model`), remove `BENCH_FOLDER_FINALIZER` from the legacy Bench,
      patch it `desiredState: Stopped`; once the Workspace is `Ready` or `Idle` → delete the
      legacy Bench. Log `bench.backfill.{created,stopped,deleted,failed}`. Tolerates a missing
      Bench CRD (404 = done).
- [ ] commit `Pause, remove, key and collect benches as workspaces, and backfill legacy Bench objects`

### Task 7: gateway (bins/gateway)

- [ ] `resolve_bench(client, id, port)`: `Workspace::get` → 404 unless `is_bench` → 403 Paused
      → 409 not Ready → podRef → IP. `resolve(...)`: read `spec.access` from the Workspace; drop
      the pair-Bench lookup (`REMOVED_AT` fallback stays). Tests updated.
- [ ] commit `Resolve benches and pauses from the Workspace alone`

### Task 8: desktop (harness/src)

- [ ] Machine panel lists the bench first (from `GET /v1/bench`, mapped through `toWorkspace`
      with name "bench"), with packages and shell scope like any workspace; nothing else changes.
      Tests for the merge order.
- [ ] commit `Show the bench as the first workspace in the desktop`

### Task 9: SLO (bins/slo, catalogue, slo.md, fixture)

- [ ] New ids per spec: `bench.push.p95` (group 3), `bench.pkg.add` (group 3), `bench.migrated`
      (weekly: seed a legacy folder for the drill owner, create the bench, assert `.bench/` holds
      the seed and the folder was renamed). `bench.idle.wake` asserts readiness false → pod gone
      → wake. Rows in catalogue, `deploy/slo.md`, fixture.
- [ ] commit `Probe the bench's push, packages and folder migration`

### Task 10: ship and migrate (main session)

Order matters: CRDs (`deploy/k3s/crds.yaml`) → api roll (backfill creates bench workspaces, stops
legacy benches) → agents (migrate folders, start two-container pods) → gateway → probes → desktop
rebuild. Verify: owner's bench comes back with sessions intact; `kl pkg add` inside the bench
shell works; hourly green; `.migrated-*` folder present. Bench CRD deletion: separate,
owner-gated, after a week of zero legacy objects.

## Dependency order

T1 → {T2, T3, T5, T6, T7} in parallel (disjoint trees) → T4 (needs T2's pod, T3's signals) →
{T8, T9} → T10.

## Self-review

Spec sections → tasks: object model T1; pod T2; harness-bench T3; agent + migration T4; API +
quota + tool admission T5; membership/removal/keys/GC/history/backfill T6; gateway T7; desktop T8;
SLO T9; migration order T10. Names: `is_bench`, `wants_pod`, `BenchOptions{model, wake_at}`,
`Access`, `idle_since`, `allow_gateway_bench`, `bench_container`, `migrate_bench_folder`,
`bench.backfill`.
