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

### Task 1: harness — no builtin tools, ask a workspace, own-workspace tools, identity (harness/)
- `bench/src/rpc-child.ts`: bench session args gain `--no-builtin-tools`, exts = `kloudlite.ts`
  only (drop `background.ts`/`process.ts` for bench sessions); fork = `--no-tools`; every child
  gets `KL_SESSION=<session id>` in env. Workspace sessions keep `workspace-tools.ts` + `--tools`
  and ALSO load `kloudlite.ts` in workspace mode (own `kl_pkg_*` only).
- `bench/src/server.ts` + `bench.ts`: `POST /workspaces/{id}/ask {text, from}` per spec §2 —
  open (create if absent) the workspace's own session, record the exchange, prompt the child
  (followUp when busy), transition on agent_start/agent_end, deliver the final assistant text to
  `from` as a follow-up prompt `[from workspace <name>] …`. Refuse a `from` that is not a live
  session (400).
- `pi/kloudlite.ts` + `pi/catalog.ts`: `kl_workspace_ask`; `kl_pkg_*`, `kl_env_*` on the own id
  (bench: `KL_WORKSPACE_ID`; workspace session: `KL_TOOLS_WORKSPACE`); remove
  `kl_workspace_packages`; tools per spec §3; system prompt per §5 (REPLACES pi's; "the Kloudlite
  harness"; never "pi"; default target = itself; named workspace = ask).
- Tests (`bench:test`): spawn args per kind; `/ask` creates the session, queues, transitions,
  delivers back (fake child); catalogue names every registered tool; prompt has no "pi"/"/opt/harness".
- Commit `Give bench sessions platform tools only and queue workspace work into the workspace's session`.

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

### Task 3: probe fixes from hourly-manual-0456 (bins/slo)
- `k8s::BENCH_POD` is dead: a bench pod is named by its workspace id. Replace every probe use
  (`stages/bench_tool.rs`, `stages/bench.rs` idle.wake, `stages/env_intercept/mod.rs`,
  `stages/experience_teams/paused.rs`, `env.space.bench` if it shares the path) with the id from
  `GET /v1/bench` (field `id`); delete the constant if nothing else reads it (the agent names the
  pod from the Workspace, check first).
- `bench.push.p95`: delete the push it made after measuring (`DELETE /v1/volumes/{bench volume}/snapshots/{id}`).
- `request.approve`: pinch the probe owner's diskGb below its stamped usage via the admin write,
  expect 409 on a create, then the request/approve flow as today; restore the quota in the
  compensation. Mirror `quota.refused`'s pinch.
- Teardown volume-delete 404: reproduce against a leaked `ws-*` volume owned by slo-hourly with
  one pushed snapshot and no owners (e.g. `ws-e2f90f8bf8f20d4f`), find why
  `snapshots_for_caller` answers 404 for the run's jwt, fix the right side (probe jwt/owner or api).
- Gates: clippy + `bins/slo` tests; `deploy/slo.md` unchanged unless an id changes.
- Commit `Find the bench pod by its id in every probe and stop the hourly's snapshot leak`.

### Task 4: ship (main session)
Standing flow; owner's bench pod restarted; hourly hand-started.
