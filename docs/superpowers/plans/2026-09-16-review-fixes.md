# Review Fixes (16 Sep) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close every finding from the three whole-range reviews of the night's 118 commits (8b7b0ab1..a93d9b9d), then ship once.

**Architecture:** Three independent tracks by area, one worktree each, merged into `desktop-login` one at a time, one pod build, one rollout (RBAC/CRD/VAP first, then k3s tiers, then AKS), then a hand-started hourly probe.

**Tech Stack:** Rust (workspaces/api/core/storage/registry/agent/controller/server/slo), Next.js web, k3s YAML (VAP, CRD, RBAC), Helm values (ClickStack), Sampler YAML.

**Spec:** the three review files — `scratchpad/review-a.md`, `review-b.md`, `review-c.md` (session scratchpad; copies below in Findings). The reviews are the authority; this plan is their argument.

## Global Constraints

- Keep-bias: uncertainty never deletes or pauses; a 404 is success only where the same computed name was the create's.
- Ownership invariant (CLAUDE.md): exactly one node may open a repo/image DB; a guard must fail OPEN only in the pre-change direction, never closed on an owner.
- No behaviour change inside a pure file split; every path outside the split module stays unchanged (`mod.rs` re-exports).
- Commits: imperative sentence case, no tool attribution (hook rejects it).
- Gates per crate touched: `cargo test -p <crate>`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test -p kloudlite-tests --no-run`; web: `bun run lint && bun run typecheck && bun run test --force`; YAML: `yaml.safe_load_all` over `deploy/**`, `helm template` for clickstack values, `kubectl apply --dry-run=server` for the VAP.
- Fixture row-for-row with `deploy/slo.md` (test enforces).

---

### Task A — workspaces / api / core (branch `rv-a`, worktree `/Volumes/kdisk/rustic-git-wt/rv-a`)

**Files:** `crates/workspaces/src/api/{membership.rs,removals.rs,scope.rs,keys.rs}`, `crates/api/src/teams.rs`, `crates/core/src/metrics.rs`.

- [ ] A1 IMPORTANT `membership.rs:78` — `Member(Paused)` with a system `removed-at` on the pair: clear the stamps AND pause in the same pass. Test.
- [ ] A2 IMPORTANT `removals.rs:50` — delete-now takes a handle; an email → 400 naming it; unreadable directory stays 503. Tests for both.
- [ ] A3 `scope.rs:90` — evict expired `member_verdicts` on insert.
- [ ] A4 `scope.rs:66` — `denial()` uses the cache, no extra directory call.
- [ ] A5 `teams.rs:81` — no `unwrap_or_default` fabricating `{"state":"active"}`; propagate the serialization error.
- [ ] A6 `membership.rs:359` — grace-keep arm deletes the tool Secret like `pause()` (or a why-comment).
- [ ] A7 `metrics.rs` — `http.unready` demotion only for planned readiness 503s (no live leader / draining); object-store failure stays warn.
- [ ] A8 `removals.rs:81` — test: delete-now on an unstamped non-member stamps then marks due now.
- [ ] A9 Split `membership.rs` (~908) and `keys.rs` (~944) under ~800 lines: pure moves, `//!` headers, re-exports. Separate commit(s).

### Task B — agent / controller / server / storage / registry / slo (branch `rv-b`, worktree `rv-b`)

**Files:** `crates/storage/src/pool/mod.rs`, `crates/app/src/routing.rs`, `bins/server/src/router/{route.rs,limits.rs}`, `bins/agent/src/{controller/watch.rs,snapshot.rs,binding.rs,controller/bench.rs,controller/workspace/home.rs}`, `bins/controller/src/gc.rs`, `bins/slo/src/stages/monthly/removed.rs`, `crates/workspaces/src/slo/catalogue.rs`, `deploy/slo.md`, one fixture row, `.config/nextest.toml`.

- [ ] B1 IMPORTANT unowned guard is task-local: carry it on the request (extension) so spawned work inherits it, or minimum: comments say "in-task only" + a test/assertion that no open happens off-task under Missing.
- [ ] B2 IMPORTANT `is_fenced` "version already exists" anchored to the manifest-write path only; `// ponytail:` ceiling; test that a corruption-shaped Data error is not a fence.
- [ ] B3 `watch.rs` — back off relists for a permanently erroring watch (double up to 1 h). Test.
- [ ] B4 `snapshot.rs` — "not here" stamps `Degraded/NotHere` after N passes.
- [ ] B5 `gc.rs` — log a mid-loop lost lease.
- [ ] B6 `limits.rs` — debug log on unowned→404.
- [ ] B7 `route.rs` — `leaderless_too_long` warns once on the transition (latch).
- [ ] B8 `bench.rs` + `home.rs` docs — folder dies with the Bench for every delete reason.
- [ ] B9 `binding.rs:43` ↔ `api::keys::prune_bindings` — cross-cite.
- [ ] B10 `team.member.removed.dir_down` marked not-yet-measurable (`manual`) in catalogue + slo.md + fixture row.
- [ ] B11 `nextest.toml` — comment naming the two suites the retry masks.

### Task C — deploy / web (branch `rv-c`, worktree `rv-c`)

**Files:** `deploy/k3s/{agent-admission.yaml,crds.yaml}`, `deploy/sampler/{slo.yml,fleet.yml}`, `deploy/clickstack/clickstack-values.yaml`, `web/apps/web/src/app/(shell)/superadmin/{configuration/[scope]/editor.tsx,actions.ts}`, fixtures test.

- [ ] C1 VAP also matches `benches/status`, `workspaces/status`, `spaceenvironments/status` (hardening; comment that /status discards metadata). Server dry-run.
- [ ] C2 CRD Bench access enum keeps `readOnly` beside `full|paused` until no stored object carries it (comment).
- [ ] C3 `slo.yml` — fix `FINAL r INNER JOIN` alias order; gauge title; every query parses.
- [ ] C4 `fleet.yml` — `lower(SeverityText)` filters.
- [ ] C5 clickstack values — explicit `max_server_memory_usage` under the 12 GiB limit.
- [ ] C6 editor — aria-label on revert note; plural wording.
- [ ] C7 `deleteRemovalNowAction` validates slug/owner/confirm server-side.
- [ ] C8 fixtures test for `PENDING_REMOVALS`, `CLUSTER_HISTORY` shapes.

### Task D — merge, review, ship

- [ ] Opus re-review of each track's diff against its review file (Ready / Not ready), fix rounds as needed.
- [ ] Merge rv-a, rv-b, rv-c into `desktop-login` one at a time; resolve conflicts (keys.rs likely).
- [ ] Pod build (`deploy/dev/pod/ship.sh`); pin; roll: `crds.yaml` + `agent-admission.yaml` + RBAC first → k3s tiers → AKS; push origin master when green.
- [ ] Hand-started hourly probe; confirm 0 failed; confirm `dir_down` reads as manual, not passed.

## Findings (verbatim summaries)

See the three review files; the checklists above are their fix clauses in order of severity.
