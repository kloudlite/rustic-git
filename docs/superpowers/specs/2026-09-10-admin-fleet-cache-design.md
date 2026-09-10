# The admin process reads the fleet from reflector stores

**Date:** 2026-09-10 · **Status:** approved (item 4 of the review follow-ups)

## Problem

Every admin page walks the fleet on every request: `GET /admin/owners` lists Quota, QuotaRequest,
Workspace, Environment, Volume and Snapshot cluster-wide, `/admin/clusters` lists Regions, Nodes,
Workspaces and Environments, the request pages list every Request. Six API-server lists per page
view is fine at three nodes and a few hundred objects; it is the wrong shape at three hundred, and
it makes the console's latency the API server's list latency.

## Decision

The admin process (`bins/api` with `KLOUDLITE_API_ROLE=admin`, the only process that serves these
pages) keeps one `kube::runtime::reflector::Store` per kind it reads — Node, Region, Workspace,
Environment, Volume, VolumeReplica, Snapshot, Quota, QuotaRequest, Request — fed by a watcher with
the runtime's default backoff, in `api::admin::fleet::FleetCache`. Admin readers take their rows
from the store; they no longer list.

- **Staleness is bounded by the watch**, milliseconds in practice. A store is read only once it has
  been initialised (`wait_until_ready` at boot, with a 30 s cap so an unreachable cluster still lets
  the process come up); until then, and whenever no cache is configured — the `user` role, the
  mock-client tests — the reader lists exactly as today. One helper, `fleet::all`, makes that
  choice, so no reader carries two code paths.
- **Memory** is bounded by the object count, which is the same bound the API server's own list
  answer already imposed on the reader per request — now held once instead of copied per page.
- **Only the primary region's client is cached.** A page that reads a specific region through
  `region_client` (`/admin/clusters/{region}`) keeps listing that region; the multi-region cache is
  the next step if a second region ever exists.
- **`/v1` is untouched.** `quota::usage` keeps its per-request, label-selected walk: the guide's
  rule that allocation is never decided from a cached count stands.

## Not doing

No cache in the `user` role (two replicas, no admin pages), no cache invalidation API (the watch
is the invalidation), no change to the history reflectors (they keep the previous object, which a
`Store` cannot give them).

## Verification

Unit: `fleet::all` answers from the store when one is ready and lists otherwise (mock client).
Fleet: `/admin/owners` and `/admin/clusters` return the same rows before and after the roll
(compared by the probe's admin stage, `admin.*` ids), and the admin pod's API-server list rate on
`/apis/kloudlite.io` drops to the watch's resync cadence.
