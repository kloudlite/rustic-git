# Disk quota by usage — implementation plan

> **For agentic workers:** implemented task by task by Opus implementers dispatched from the main
> session (Fable), which reviews each diff itself.

**Spec:** `docs/superpowers/specs/2026-09-17-disk-quota-by-usage-design.md` — binding.

## Global constraints

- Worktree `/Volumes/kdisk/rustic-git-wt/desktop-login`, branch `desktop-login`; prefix every
  shell command with `cd` there; never `cargo` in `/Users/karthik/rustic-git`.
- Gates: `cargo clippy --workspace --all-targets -- -D warnings` + touched crates' tests; web
  `bun test` when web touched.
- Wire names verbatim: `Volume.status.usedBytes`, `Volume.status.usedAt`, `GET /v1/quota` →
  `disk: {usedGb, budgetGb}`. No floor, no disk refusals.
- Status is written only through `/status`; spec untouched. Commits imperative, no attribution.

### Task 1: the agent stamps usage (bins/agent + crd)

- `crd::VolumeStatus { used_bytes: Option<u64>, used_at: Option<String> }` (serde `usedBytes`,
  `usedAt`); regenerate `deploy/k3s/crds.yaml`.
- `bins/agent/src/sync.rs` beat and the push/snapshot-delete paths: after the cut, read the
  volume qgroup's referenced bytes (find the existing qgroup helper the agent uses for
  `quotaGb` enforcement in `crates/storage`/`bins/agent/src/btrfs*`; add `qgroup_referenced(pool,
  id) -> io::Result<u64>`) and patch `status.usedBytes/usedAt` only when the value changed by
  ≥ 1 MiB. Only when the beat already cut (generation moved) — never a read for an idle volume,
  never a new beat. Only the holding node writes.
- Tests: reconcile test with a fake qgroup reader; no write when unchanged.
- Commit `Stamp each volume's occupied bytes on the sync beat`.

### Task 2: quota sums usage (crates/workspaces)

- `quota.rs`: diskGb usage = Σ `used_bytes` (0 when unstamped) over the owner's Volumes, ceil to
  GB; drop `quotaGb` and `BUILDER_CACHE_GB` as inputs. REMOVE the disk dimension from
  `guard_alloc` entirely (owner: disk is never enforced); no `guard_fill`. The `Quota` CRD's
  `diskGb` field is renamed in meaning only ("budget"): keep the field, change the doc comment.
- `GET /v1/quota` adds `disk: {usedGb, budgetGb}`; Volume doc adds `usedBytes`, `usedAt`.
- Admin `fold_usage` mirrors. Tests: usage math, floor, fill refusals, doc shapes. Fixtures.
- Commit `Charge disk quota by occupied bytes with a one-gigabyte floor`.

### Task 3: web + desktop show used vs ceiling

- Quota views (`web/apps/web` owner quota page, superadmin owners detail; desktop MachinePanel
  if it shows disk) render `used / limit` and per-volume `used / ceiling`. Tests where the
  siblings have them.
- Commit `Show occupied disk against the quota, not reserved`.

### Task 4: probes

- `quota.view` reads the new shape; `quota.refused` refuses on the workspaces COUNT, never disk;
  new hourly `vol.usage.stamped` (write 200 MB in the run's workspace via the
  tool server, wait two sync beats, `usedBytes ≥ 200 MB`). Catalogue + `deploy/slo.md` + fixture.
- Commit `Probe occupied-bytes quota`.

### Task 5: ship (main session)

CRDs → agents (stamps appear within one beat) → api → web; the owner's account then reads a
few GB used; the temporary 200 GB Quota CR for karthik1729 and the 200 GB probe quotas are
reverted to defaults after one green hourly.

Order: T1 → T2 → {T3, T4} → T5.
