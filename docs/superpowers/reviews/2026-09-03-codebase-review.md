# Codebase review, 2026-09-03 (master `4c7e94c9`)

Second whole-tree review, one day after the first (`2026-09-02-codebase-review.md`, all 22
in-scope findings verified landed). Five reviewers in parallel; the full reports with file:line
and concrete fixes are under `2026-09-03-details/`:

| area | report | Critical | Important | Minor | Cleanup |
|---|---|---|---|---|---|
| workspaces crate + `/v1` | `workspaces-api.md` | 2 | 7 | 10 | 12 |
| agent | `agent.md` | 3 | 6 | 7 | 9 |
| web | `web.md` | 0 | 4 | 6 | 7 |
| server, registry, deploy | `server-deploy.md` | 0 | 3 | 7 | 2 |
| over-engineering audit | `audit.md` | 13 cuts, net −900 lines, −4 deps | | | |

## Critical (fix before anything else ships)

1. **A team member can delete the team's whole history.** `delete_volume`/`delete_snapshot`
   decide "the volume is empty" from an owner-filtered snapshot list, then delete the Volume CR.
   Count unfiltered, as `parents_of_volume` already does. (`workspaces-api.md` C1)
2. **No cap or rate limit on workspace/environment creation.** Any account can loop `POST` until
   memory and the btrfs pool are gone. Per-owner limit in `/v1` first; a ResourceQuota per
   namespace as the backstop. (`workspaces-api.md` C2; the owner planned "an upper limit later")
3. **The pull path can delete a fresh push's bytes.** `retired()` in `peer.rs` deletes local
   snapshot bytes from a listing taken before `local_commits`, no fresh GET — the same race the
   byte sweep guards against. (`agent.md` C1)
4. **A crash between the release CAS and the un-place strands every parent on the volume.** No
   watch matches them afterwards and the sweep skips empty-pinned volumes. Make the sweep re-run
   the un-place for an empty-pinned volume whose parents are still placed. (`agent.md` C2)
5. **Deleting an interrupted source destroys the cut its rescue clone seeds from.**
   `cleanup_parent` deletes every sync point of the worktree without consulting
   `seeded_from_cuts`. (`agent.md` C3)

## Important

- **Authorization by label** in `ssh_session`'s name fallback (mints an ssh token), `list_ws`,
  `list_volumes`, `pushed_volumes` — one `.filter(spec.owner)` each. (`workspaces-api.md` I1, I2)
- `delete_snapshot`'s running-base check reads only `status.head`; a live parent whose head is
  unset is not protected. (`workspaces-api.md` I3)
- `GET /v1/regions` returns the Azure storage account names to every caller; project to
  `id`, `name`, `status`. (I4)
- `parents_of_volume` = two unfiltered cluster-wide LISTs per delete; add a selectable field on
  `status.volumeRef`. (I5)
- Neither restore checks the frozen state's kind (a workspace restore accepts an environment
  snapshot). (I6)
- `api.rs` is 2 659 lines, five resources; mechanical split. (I7)
- Agent: blocking btrfs calls on the reactor in `retire_pass` (I1); an authenticated peer can
  drive the pull beat continuously via `/peer/v1/wake` (I2); the send lock is held for the whole
  stream, one stalled puller blocks a volume for an hour (I3); `btrfs receive` has no size
  ceiling (I4); `sync_one`'s newest-sync-point comparison lets a missing annotation win (I5);
  every node reconciles every Snapshot in the cluster (I6).
- Web: a failed history fetch renders as "No snapshots left to restore from" (I1); the
  environment header and its Snapshots tab disagree on the current snapshot (I2); `provenanceOf`
  reads a shape the API no longer sends (I3); two sequential round trips on both list pages (I4).
- Deploy: the admission policy fences CREATE/UPDATE only, so the agent's deletes (services,
  pods, networkpolicies, statefulsets) are unfenced and the file's stated ceiling is false (1);
  `apps/deployments: get, delete` is a dead grant (2); the `tests/routing.rs` forwarding flake is
  the harness never renewing the lease — a renew loop fixes it (3).

## Over-engineering (audit.md, biggest first)

- The Cosmos tier (`azure_data_cosmos`, `azure_core`, a duplicate `reqwest 0.12`) exists for an
  85-line file holding `Region` rows. A `Region` CRD written by `/v1/regions` deletes `cosmos.rs`,
  `store.rs`, `MetaStore`, `MemStore`, the `COSMOS_*` env triple and four dependencies. It also
  removes the "restart forgets the region" trap that cost time today.
- `VolumeSource::RestoreOf` and its tolerance arms protect objects that no longer exist.
- Four hand-built CAS JSON patches (`take/release/attach/detach_volume` + one in `peer.rs`) →
  one `cas()` helper.
- `MetaStore` trait, `crates/api`'s re-export shims, and the rest in the file.

## What is good and should stay

Per the reports: the one-invariant routing middleware; keep-biased sweeps with fresh GETs (byte
sweep, record sweep, unreferenced-volume sweep); guarded CAS on every pin/owner change; the
finalizer's "lost detach is an error"; `is_snapshot()` as the single predicate; the recorder-based
tests; RBAC header tables that are the role; the web's shared archived-snapshots component and
pure, tested copy builders.

## Suggested order

1. Criticals 1, 3, 5 (data loss), then 4 (stranding), then 2 (limits).
2. Authorization-by-label fixes and the admission DELETE fence (small, security).
3. Region CRD + Cosmos removal; `RestoreOf` and CAS dedupe; `api.rs` split; agent `peer.rs` split.
4. Web I1–I4; routing-test renew loop; the remaining minors and cleanups.
