# Deploys and drains without user-visible errors

**Status:** design, 2026-09-05. Baseline measured by the SLO probe the same day: every `deploy/roll.sh`
and every node drain fails one fast sample (502 from git push, registry push, the JWT tier check) and,
during a drain, the console answers 503 for the admin process's restart window. `bins/server/src/main.rs:145-151`
records the same thing from the inside: "1–2 failures per pod, 7–14 s after Killing, on three consecutive rolls."

## What actually breaks, and why

| Symptom | Cause (file:line) |
|---|---|
| Push or blob upload gets 502 for ~10 s per rolled `kloudlite-srv` pod | A repo's database is open on exactly one pod. SIGTERM already releases the pool first, resigns the lease, then drains for 5 s (`main.rs:179-214`), but a request that reaches a follower for a repo whose owner is mid-release has no owner yet: the lease is re-taken every 3 s with a 10 s TTL (`ownership/lease.rs:20,22`), so up to ~15 s pass before another pod opens the map. The router's one-hop recovery (`router/route.rs:508-560`) only replays **bodiless** GETs; anything with a body is answered 502. |
| First registry request to a moved image 500s once | The git router recovers from a fenced handle (`router/git.rs:325,401,503`); the registry never got that arm — every store error becomes `oci_internal` (`crates/registry/src/lib.rs:57`). |
| Everything 502s for the length of an ingress restart | `ingress-nginx` is one replica, Helm-installed out of band, no PodDisruptionBudget; our two Ingress patches set no `proxy-next-upstream`, so nginx retries nothing either. |
| Console 503 while the admin pod restarts | `kloudlite-admin` is one replica with no preStop; the same for worker and gateway. Only srv, api and web have PDBs. |
| A user's page load fails during any of the above | No client retries anywhere: one fetch in `web/apps/web/src/lib/api.ts:95`; `crates/api/src/forward.rs:29` turns any upstream error into one 502 and `relay` passes 421/503 straight through. |
| The probe files the failure as a bad sample | By design — it is a user. But nothing tells it a rollout is in flight, and its AKS Role cannot read Deployments/StatefulSets (`deploy/kloudlite.yaml:1508-1560`). |

The srv StatefulSet sets no `updateStrategy`/`partition`/`podManagementPolicy`, so a roll is
OrderedReady with no surge: the pod is gone before its replacement is Ready, and the ownership
map loses a node for the whole restart.

## Design

Four independent changes, in the order they pay off. Each is a task in the plan; none depends on
another except where stated.

### 1. Ownership handover before exit (server)

Today's shutdown releases ownership and then waits for *clients* to finish. Reverse the emphasis:
on SIGTERM the pod first **hands its repos to a live peer** and only then stops serving.

- `preStop` becomes a call to a new peer-only endpoint `POST /peer/v1/drain` (same trust as the
  other peer routes). The handler: stop accepting new ownership (a `draining` flag the election
  tick and `route_inner` read), then for every repo it owns, close the database and write the map
  entry to a peer it picks by the existing rendezvous (`OwnershipStore` already has the writer
  lock; a non-leader sends the reassignments to the leader through the same peer path claims use).
  The map update is what `route_inner` reads, so followers route to the new owner at once instead
  of waiting a lease cycle.
- The map writer (leader) must not be the draining pod: if it is, it resigns the lease **first**
  (`main.rs:192` already does; move it ahead of the release) and waits for a new leader before
  reassigning, bounded by `LEADER_TTL + LEADER_RENEW`.
- Requests that arrive during the window get the existing 421-with-location, which the router's
  recovery follows — and the recovery is extended to bodied requests when the body is still
  buffered (git pushes are, up to `max_body`; blob PUTs stream and are retried by the client,
  see §3).
- `terminationGracePeriodSeconds` stays 90; the drain endpoint is bounded to 30 s and logs
  `ownership.drained {repos, ms}`.
- Registry: add the fenced-handle arm the git router has (`is_fenced` → reopen once, then serve)
  in `crates/registry/src/{manifests,blobs}.rs`, closing the "500 once" gap.

Test: the existing three-roll measurement in `main.rs` becomes a test in `tests/` that rolls one
pod of a three-node `file://`-less harness and asserts zero non-2xx on a push loop.

### 2. Deployment shape (yaml)

- `kloudlite-srv`: `updateStrategy: RollingUpdate` with `maxUnavailable: 1` (k8s ≥1.24 for
  StatefulSets) and `podManagementPolicy: Parallel` so a replacement can start while the drained
  pod finishes; readiness gate stays `/healthz`, which now also reports `draining: true` as
  not-ready so the Service stops sending it traffic before the handover.
- `kloudlite-admin`, `kloudlite-worker`: `strategy.rollingUpdate.maxUnavailable: 0, maxSurge: 1`
  plus a 10 s preStop sleep so the old pod leaves the endpoints before SIGTERM. Two replicas of
  admin is **not** the answer: it is the single writer of the `kloudlite` database and the alert
  evaluator; a surge pod overlapping for seconds is fine, two permanent ones are not.
- PDBs for admin and worker (`minAvailable: 0` during a roll is the default; the PDB is for
  drains: `maxUnavailable: 1`). A PDB and two replicas for `ingress-nginx`, applied as a third
  server-side patch beside the two the repo already owns; the ingress annotations gain
  `nginx.ingress.kubernetes.io/proxy-next-upstream: "error timeout http_502 http_503"` and
  `proxy-next-upstream-tries: "2"` for the app host only (the registry host keeps buffering off).
- `deploy/roll.sh` orders the tiers: srv first (one pod at a time, waiting for
  `ownership.drained` in the pod log), then api and admin, then web, then worker; and it refuses to
  start within 60 s of a fast-probe tick (`*/5`), so a roll never lands on a sample.

### 3. Client retries where they are safe

- `crates/api/src/forward.rs` and `browse.rs`: on 502/503/421 from upstream, retry once after
  250 ms for GET/HEAD and for the bodied calls whose body is fully held (JSON bodies are); never
  for streamed bodies.
- `web/apps/web/src/lib/api.ts`: one retry after 300 ms on 502/503 for GET; a page render that
  still fails shows the existing error surface.
- The registry's own client behaviour is fine: `crane`/docker already retry blob PUTs on 502
  (the probe's log shows `retrying PUT`), so the server only has to stop 500ing after a move.

### 4. The probe yields to a rollout

`bins/slo` gets one more prelude check beside `hourly_in_flight`: `rollout_in_flight`, true when
any of the KNOWN workloads (`admin::workloads::KNOWN`, the same list the settings roll uses) has
`updatedReplicas < replicas` or `readyReplicas < replicas` on AKS, or the agent DaemonSet on the
region has `updatedNumberScheduled < desired`. A fast run that sees one skips every id with
"a rollout is in flight" and files no sample. RBAC: `deployments`/`statefulsets: get,list` on the probe's AKS Role and `daemonsets: get,list` on
the region ClusterRole (the only DaemonSet the probe watches is the region's agent). The hourly does not yield; it takes the sample,
because its window is the operator's own choice.

## Not in scope

- Multi-writer or replicated SlateDB. Ownership stays one-pod-per-repo; this design makes the
  move fast and announced, not unnecessary.
- The gateway's `Recreate` (hostPort) and the SSH/peer-stream listeners' drain
  (`main.rs:242` ponytail) — SSH clients retry on their own and the gateway is per-region.

## Acceptance

`deploy/roll.sh` on a busy cluster and a `kubectl drain` of one AKS node each complete with
**zero** failed fast-probe samples (the probe yields to the roll and the drain shows no 502s to
the hourly), measured over three consecutive rolls.
