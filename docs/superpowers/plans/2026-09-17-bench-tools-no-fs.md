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

### Task 3: ship (main session)
Standing flow; owner's bench pod restarted; hourly hand-started.
