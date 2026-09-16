# Bench tools without a filesystem — implementation plan

> Implemented task by task by Opus implementers dispatched from the main session (Fable), which
> reviews each diff itself.

**Spec:** `docs/superpowers/specs/2026-09-17-bench-tools-no-fs-design.md` — binding.

## Global constraints
- Worktree `/Volumes/kdisk/rustic-git-wt/desktop-login`, branch `desktop-login`; prefix every
  shell command with `cd` there; never `cargo` in `/Users/karthik/rustic-git`.
- Gates: harness `npm run typecheck && npm run bench:test && npm run build`; Rust `cargo clippy
  --workspace --all-targets -- -D warnings` + touched crates' tests; `deploy/slo.md` equal to the
  catalogue (test enforces).
- Wire names verbatim from the spec. Commits imperative, no attribution. Do not push.

### Task 1: harness — no builtin tools, kl_ws_*, fuller catalogue, system line (harness/)
- `bench/src/rpc-child.ts`: bench session args gain `--no-builtin-tools`, exts = `kloudlite.ts`,
  `workspaces.ts`; fork = `--no-tools` (drop `BTW_TOOLS`). Workspace sessions unchanged.
- `pi/workspaces.ts` (new): the seven `kl_ws_*` tools per spec §2, reusing `toIde`/`fromIde`
  and the address resolver from `workspace-tools.ts` (export what is needed; do not duplicate).
- `pi/catalog.ts` + `pi/kloudlite.ts`: tools per spec §3; exchange regex matches `kl_ws_`;
  system-prompt line per §5 (find pi's hook in `node_modules/@earendil-works/pi-coding-agent/docs`).
- Tests (`bench:test`): spawn args for bench/workspace/fork; `kl_ws_*` → IdeCall mapping with
  the workspace id; catalogue names every registered tool.
- Commit `Give bench sessions platform tools only and reach workspaces through their tool servers`.

### Task 2: api + agent — PATCH environment services (crates/workspaces, bins/agent)
- `api/environments.rs`: `patch_env_services` per spec §4, route `.patch(...)` on
  `/v1/environments/{id}`; `check_services`; `guard_alloc(&environment_cost(added))` where
  `added = new.len().saturating_sub(old.len())`; 409 when `spec.intercepts` names a removed
  service. Update the `check_services` doc comment (it says nothing updates in place).
- `bins/agent/src/controller/environment/run.rs`: after `apply_services`, delete StatefulSets and
  Services in the namespace labelled as the environment's whose name is not in spec. Never a
  folder. Test in `bins/agent/tests/reconcile/`.
- Probes `env.services.patched` (hourly) + `bench.tools.no_fs` (hourly) in the catalogue,
  `deploy/slo.md`, the web fixture, stage code in `bins/slo/src/stages/`.
- Commit `Change an environment's services in place and probe it`.

### Task 3: ship (main session)
Standing flow; owner's bench pod restarted; hourly hand-started.
