# Review follow-ups: the five items the 2026-09-12 plan left open

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development or superpowers:executing-plans. Steps use checkbox (`- [ ]`) syntax for tracking. Every edit happens in the dev pod; nothing is fixed until the step passed on the fleet on the carrying build.

**Goal:** close the five items left after the review-fix plan shipped (cf5e4ab8): dead settings knobs, the `/dev` mount, the agent's per-reconcile GETs, the slow audit listing, and the pre-rename cluster objects.

**Architecture:** four code/manifest batches serialized on the pod (each ships once, rolls, and is verified by the hourly or a direct check), plus one hand-run cluster cleanup. Batches 1 and 4 are independent of each other and of 2/3; 2 (agent reads) and 3 (pool device) both touch the agent and go one at a time.

**Spec:** `docs/superpowers/specs/2026-09-12-review-findings.html` (#64, #52–#56, and the 4.1/3.2 deferrals recorded in `docs/superpowers/plans/2026-09-12-review-fixes.md`).

## Global Constraints

- Edit only in `/work/src` (or a `fix/*` worktree) in the dev pod; never `cargo` on the laptop; never a plain `cargo update`; never `cargo fmt`.
- Every deleted knob, field or grant is deleted from the CRD, the validator, the schema, the web and the docs in the SAME commit; a half-removed knob is worse than a dead one.
- The agent never authorizes or decides on a label; stores are views of the API server and every decision that deletes or releases re-reads the object it acts on (CLAUDE.md "Load-bearing rules").
- Fixed = verified on the fleet: `deploy/dev/run-job.sh hourly` passed on the carrying build, plus the direct check each batch names.

---

### Batch 1: drop the three dead `ClusterSettings` knobs (4.1 deferral)

Decision recommended to the owner: DROP. `max_per_owner` is superseded by `Quota.workspaces`; `home_cache_gb` has nothing to size since the home moved to NFS and the homecache holds only TMPDIR/state; `stop_flush_timeout_secs` has no reader because a stop tears down the moment its cut is Ready. Nothing on the fleet sets any of them (`kubectl get clustersettings default -o yaml` on the region shows none).

**Files:**
- Modify: `crates/workspaces/src/crd/settings.rs` (fields + `defaults::` fns), `crates/workspaces/src/settings.rs:57-63`, `crates/workspaces/src/api/admin/settings.rs:310-352` (`range!`/`over!`), `crates/workspaces/src/api/admin/schema.rs:191-196`, `deploy/k3s/README.md` where the knobs are listed, `deploy/alerts.md`/`deploy/slo.md` only if they name them (grep first).
- Test: `crates/workspaces/tests/api_admin_settings.rs` (or the settings tests in `admin/settings.rs`): a PUT carrying `maxPerOwner` is refused 422 naming the field as unknown; the schema route no longer lists the three; `WS_MAX_PER_OWNER` in env is ignored (test that `Settings::from_env` has no such field — a compile-time absence, so the test is the grep in CI: `! grep -rn WS_MAX_PER_OWNER --include=*.rs crates bins`).

- [ ] Remove the three fields, their `defaults::` fns, the `range!`/`over!` lines and the schema entries; `serde(deny_unknown_fields)` is NOT on `ClusterSettingsSpec` (old stored objects must still parse), so an old CR that carries them parses and the value is dropped — say so in the field-removal commit.
- [ ] `cargo test -p kloudlite-workspaces`, `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] Web: `grep -rn "maxPerOwner\|homeCacheGb\|stopFlushTimeoutSecs" web/apps/web/src` is already empty (verified 2026-09-12); nothing to do.
- [ ] Ship, pin, roll (admin + agent read the CR). Verify: `GET /admin/settings/schema` on the fleet lists 3 fewer keys; hourly passes (`settings.*` ids).

### Batch 2: the agent decides from stores, not GETs (3.2 deferral)

Today `controller/environment/intercept.rs:157,179,297` GET the intercepting Workspace and its Pod on every environment pass, and `listing.rs:97-105` lists every Workspace and Environment cluster-wide on every volume decision. The only Workspace store is `Controller::store()` filtered to this node, which is wrong for an intercepting workspace on another node — that is why the phase-3 implementer left it.

**Files:**
- Modify: `bins/agent/src/controller/run.rs` (add ONE unfiltered `reflector` over `Workspace` and one over `Environment`, cluster-wide, `watcher::Config::default()`, held on `Ctx` as `Store<Workspace>`/`Store<Environment>`), `bins/agent/src/controller/environment/intercept.rs` (read the Workspace from the store; the Pod stays a GET — pods are per-namespace and short-lived, and a store over every tenant pod is the wrong trade), `bins/agent/src/listing.rs` (`parents_matching` reads both stores and filters in memory; keep the `ListParams` shape as the filter predicate).
- Test: `bins/agent/tests/reconcile/`: (a) an intercept whose workspace is on ANOTHER node is still Kept/Forced from the store (the fake kube serves the store's initial list, not a GET route — assert no `GET /apis/kloudlite.io/v1alpha1/workspaces/ws-1` call is made); (b) `parents_on_volume` names a parent that only the store knows; (c) a store not yet synced (`ready` false) makes the decision `Unknown`, never `WorkspaceGone` — the release path must not fire on an empty store.

- [ ] Add the two reflectors; expose `ctx.workspaces()`/`ctx.environments()`; block reconcile decisions that release or delete until both stores report ready (log `store.not_ready` once).
- [ ] Replace the three GETs and the two LISTs; keep `tracing` at each decision (`intercept.decided` with `source=store`).
- [ ] `cargo test -p kloudlite-agent-bin`, clippy. Ship, pin, roll k3s. Verify: hourly passes `env.intercept*` and `vol.*` ids; ClickStack `otel_metrics` (or `kube.slow`/apiserver request rate in the region) drops for the agent's `GET workspaces` calls — record before/after counts on the board.

### Batch 3: mount the pool device, not `/dev` (#64) — CLOSED 2026-09-12, not narrowable

The pool device is `/dev/sda` on session-0 and `/dev/sdb` on env-0/session-1 (Azure data disks reorder across reboots); a DaemonSet hostPath is one static path for every node and `by-label`/`by-id` links resolve back into `/dev`. Recorded on the mount in `agent-daemonset.yaml`; the steps below are kept for the record and are not to be run.

The DaemonSet mounts all of `/dev` into a privileged pod (`deploy/k3s/agent-daemonset.yaml:296-306`, the `ponytail:` marker). The narrowing is `/dev/btrfs-control` plus the pool device; `format-pool.sh` labels the pool `wspool`, so `/dev/disk/by-label/wspool` is the one path identical on every node — but the device the container sees must match what `/proc/mounts` names for `/wspool-prod`, or btrfs tooling that resolves the mount's device fails.

**Files:**
- Modify: `deploy/k3s/agent-daemonset.yaml` (two `hostPath` `type: CharDevice`/`BlockDevice` volumes replacing `dev`), `bins/agent/src/engine/ops.rs` only if a call resolves the device by name (grep `btrfs device`/`/dev/`), `deploy/k3s/README.md` (the format-pool step must state the label is load-bearing).
- Test: none unit-level — this is a fleet test by design. The check list for ONE node (pick the region's least-loaded, `kubectl -n kube-system get pods -o wide`): after applying a one-node variant (a second DaemonSet with a `nodeSelector` on that node and the original excluded from it via an anti-affinity label), run on that node: `kl-connect`/`/v1` create → push → restore → clone → stop → start → delete, plus `btrfs subvolume list /wspool-prod` and `btrfs send` of one snapshot to a peer (the `ws.replicated` id). Only when every step passes does the fleet DaemonSet change.

- [ ] Write the one-node variant (`deploy/k3s/agent-daemonset.canary.yaml`, never applied by roll scripts; a README note says it is a test harness).
- [ ] Owner step: label one node `kloudlite.io/agent-canary=true`; apply the canary; run the check list; report each command's output.
- [ ] On a full pass: fold the narrowed mounts into the real DaemonSet, delete the canary file, remove the `ponytail:` marker, apply on the region, hourly must pass (`ws.*`, `snap.*`, `vol.*`).
- [ ] On any failure: revert the canary, keep the marker, record the failing command beside it.

### Batch 4: the audit listing stops walking whole months (#52–#56 class; `/admin/audit` 3–7 s and growing)

`crates/workspaces/src/audit.rs:162-183` lists every key under `audit/{yyyy-mm}/` for each month in the filter, sorts, then pages. The key is `audit/{yyyy-mm}/{ts}-{rand}.json` and `ts` is RFC 3339, so a day is a prefix too: `audit/2026-09/2026-09-12`.

**Files:**
- Modify: `crates/workspaces/src/audit.rs` (`list`: walk days newest-first — today, yesterday, … — listing `audit/{month}/{day}` prefixes and stopping as soon as `limit` rows past the cursor are in hand; a filter that names an actor or action keeps walking until it has `limit` matches or reaches the filter's `since`, bounded by `MAX_DAYS_WALKED` (say 400, the retention) with the bound's reason at its definition), `crates/workspaces/src/api/admin/audit.rs` (the CSV export with a `limit` uses the same walk; without one it keeps the full walk — it is the export, and the caller asked for everything).
- Test: `audit.rs` tests with an in-memory object store: 3 days × 5 rows, `limit=4` lists only today's prefix (assert on the store's recorded list calls); a cursor into yesterday resumes there without re-listing today; an actor filter that matches only rows from 3 days ago walks exactly 3 day prefixes; `MAX_DAYS_WALKED` stops an empty-forever walk.

- [ ] Implement the day walk; keep the reverse-lexicographic order guarantee (assert it in a test with rows across a day boundary).
- [ ] `cargo test -p kloudlite-workspaces audit`, clippy. Ship, pin, roll (admin). Verify: ClickStack `http.slow` rows for `GET /admin/audit` stop appearing (they were every 5 min at 3–7 s); the hourly's `audit.row` and `audit.export` pass.

### Batch 5: delete the pre-rename `kloudlite-git-*` objects (owner's kubectl)

Nothing references them (verified 2026-09-12: no pod uses those ServiceAccounts, no manifest names them); the ClusterRoleBindings are standing privilege for unused accounts. The session's permission gate refused the deletes, so this is yours:

```sh
KUBECONFIG=.local/k3s.yaml kubectl -n kube-system delete cm kloudlite-git-nix kloudlite-git-otel-agent kloudlite-git-otel-cluster sa kloudlite-git-admin kloudlite-git-agent kloudlite-git-api kloudlite-git-otel-agent kloudlite-git-slo
KUBECONFIG=.local/k3s.yaml kubectl delete clusterrolebinding kloudlite-git-admin kloudlite-git-agent kloudlite-git-api kloudlite-git-gateway kloudlite-git-otel-agent kloudlite-git-slo
KUBECONFIG=.local/k3s.yaml kubectl delete clusterrole kloudlite-git-admin kloudlite-git-agent kloudlite-git-agent-ws-secrets kloudlite-git-api kloudlite-git-api-secrets kloudlite-git-gateway kloudlite-git-otel-agent kloudlite-git-slo
kubectl delete clusterrolebinding kloudlite-git-otel-agent; kubectl delete clusterrole kloudlite-git-otel-agent
```

- [ ] Run the four commands; then `kubectl get cm,sa,clusterrole,clusterrolebinding -A | grep kloudlite-git` on both clusters must be empty.
- [ ] The hourly still passes (nothing should notice).

## Order and cost

| batch | tier rolled | est. | depends on |
|---|---|---|---|
| 1 knobs | admin, agent | 1 ship | — |
| 4 audit | admin | 1 ship (can share batch 1's) | — |
| 2 agent stores | agent | 1 ship | — (after 1/4 to keep one agent roll at a time) |
| 3 pool device | agent (region) | 0 ships, 1 canary + owner steps | 2 landed (one agent change in flight at a time) |
| 5 stale objects | none | minutes | owner |

Batches 1 and 4 ship together; 2 ships alone; 3 is a fleet test the owner runs with me.
