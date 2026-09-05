# Zero-downtime deploys — implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development. Steps use `- [ ]`.

**Goal:** a roll or a drain fails no user request and no fast-probe sample.
**Spec:** docs/superpowers/specs/2026-09-05-zero-downtime-deploys-design.md
**Architecture:** four independent changes — ownership handover on SIGTERM, deployment shape, client retries, probe yield — in that order.
**Tech stack:** Rust (axum, kube), Kubernetes yaml, Next.js, bash.

## Global constraints

- Ownership invariant unchanged: exactly one pod opens a repo's database; the map has one elected writer.
- Peer routes stay behind `trust_peer`; the drain endpoint is peer-only.
- No retry of a streamed body anywhere; a retry may only replay a request whose body is fully held.
- `deploy/roll.sh` remains one command; the ordering lives inside it.
- Every change measured by the probe: the acceptance number is zero failed fast samples over three consecutive rolls.

---

### Task 1: Drain endpoint and handover on the server
**Files:** `bins/server/src/main.rs`, `bins/server/src/router/peer.rs` (new route), `crates/storage/src/ownership/{mod.rs,lease.rs}`, `bins/server/src/router/route.rs`, `deploy/kloudlite.yaml` (preStop).
- [ ] Add `draining: AtomicBool` on `App`; `election_tick` never claims and `route_inner` answers 421-to-peer for new ownership while set.
- [ ] `POST /peer/v1/drain`: set draining; if leader, resign and wait for a new leader (≤ TTL+renew); reassign every owned repo to a live peer by rendezvous through the writer; close each DB after its map entry moves; log `ownership.drained`; 30 s bound.
- [ ] preStop calls the endpoint; readiness reports not-ready while draining.
- [ ] Extend one-hop recovery to bodied requests whose body is buffered.
- [ ] Test: harness with three servers, push loop during one pod's drain, zero non-2xx.

### Task 2: Registry fenced-handle recovery
**Files:** `crates/registry/src/{lib.rs,manifests.rs,blobs.rs}`.
- [ ] Mirror `router/git.rs`'s `is_fenced` arm: reopen once, then serve; only then `oci_internal`.
- [ ] Test: a fenced handle on first manifest GET serves 200, not 500.

### Task 3: Deployment shape
**Files:** `deploy/kloudlite.yaml`, `deploy/ingress-nginx-patch.yaml` (new, third SSA patch), `deploy/roll.sh`, `deploy/k3s/README.md`.
- [ ] srv: `updateStrategy.rollingUpdate.maxUnavailable: 1`, `podManagementPolicy: Parallel`.
- [ ] admin, worker: `maxUnavailable: 0, maxSurge: 1`, 10 s preStop, PDB `maxUnavailable: 1`.
- [ ] ingress-nginx: replicas 2, PDB, `proxy-next-upstream` annotations on the app Ingress only.
- [ ] roll.sh: tier order srv → api/admin → web → worker; wait for `ownership.drained` per srv pod; refuse to start within 60 s of a `*/5` tick.

### Task 4: Client retries
**Files:** `crates/api/src/{forward.rs,browse.rs}`, `web/apps/web/src/lib/api.ts` (+ test).
- [ ] One retry after 250 ms on 502/503/421 for GET/HEAD and held JSON bodies; none for streams.
- [ ] Web: one retry after 300 ms on 502/503 for GET.
- [ ] Tests: a first-502-then-200 upstream yields 200 once; a streamed PUT is never replayed.

### Task 5: Probe yields to a rollout
**Files:** `bins/slo/src/suite.rs`, `bins/slo/src/kube.rs`, `deploy/kloudlite.yaml` (Role), `deploy/k3s/slo-rbac.yaml`, `deploy/slo.md`.
- [ ] `rollout_in_flight` over `admin::workloads::KNOWN` (AKS, in-cluster client) and the region agent DaemonSet; fast suite skips every id with "a rollout is in flight".
- [ ] RBAC rows; a unit test that a not-ready Deployment yields and an all-ready fleet does not.

### Task 6: Measure and close
- [ ] Three consecutive `deploy/roll.sh` runs and one AKS node drain with the fast schedule live; record failed samples per run in the ledger (target 0).
- [ ] CLAUDE.md "Deploying" paragraph: the handover, the ordering, and that a roll no longer costs a sample.
