# History layer, generic requests and console v2

**Date:** 2026-09-04
**Status:** draft for owner review. Supersedes the Monitoring and Overview sections of
`2026-09-04-superadmin-console-design.md` and the Requests section's quota-only scope; every
route that spec added stays and is reused.

Owner asks (2026-09-04): "add more dashboard elements and ensure that they are working";
"requests can be of any type"; "widescreen layout instead of container"; "add clickhouse db and
use redis to process events and add monitoring agents on cluster to maintain historic details".
Approved design: `docs/superpowers/design/superadmin-console/` (v2, ten screens).

Three sub-projects, built in this order because each feeds the next:

- **A. History layer** — ClickHouse as the durable record of what happened and what was
  measured; a Redis-stream consumer and per-cluster monitors feeding it; a history API.
- **B. Generic requests** — one `Request` CRD with kinds quota / access / region / other,
  raised by users, decided by superadmins.
- **C. Console v2** — the ten approved screens, full-width, every number backed by A or B.

## A. History layer

### A1. Store

One ClickHouse server, `rustic-git-clickhouse`, a StatefulSet with one replica in the
`rustic-git` namespace on AKS, image `clickhouse/clickhouse-server` pinned by digest, one
PVC (`managed-csi`, 100 Gi, resizable), HTTP port 8123 and native 9000 exposed only as a
ClusterIP Service. Credentials in Secret `rustic-git-clickhouse` (`url`, `user`, `password`);
`RUSTIC_GIT_CLICKHOUSE_URL` is optional everywhere — a process without it runs exactly as
today and the console shows "history unavailable" where a series would be.

**The admin process is the only writer** (`bins/api`, role `admin`), through ClickHouse's HTTP
interface with `reqwest` (already a dependency) — inserts as `JSONEachRow` batches, queries as
`JSONCompact`. No ORM, no new crate. It owns the schema: at boot it runs the numbered
migrations in `crates/workspaces/src/history/schema.rs` (`CREATE TABLE IF NOT EXISTS`, recorded
in `schema_migrations`), so a fresh ClickHouse becomes usable with no manual step.

Tables (all `MergeTree`, partitioned by month, ordered as noted):

| table | row | order | TTL |
|---|---|---|---|
| `events` | `ts DateTime64(3)`, `id String` (dedupe key), `kind LowCardinality`, `actor`, `owner`, `target`, `region`, `attrs String` (JSON) | `(kind, ts)` | none — this is the record |
| `samples` | `ts DateTime`, `region`, `node`, `workload`, `pod`, `metric LowCardinality`, `labels String`, `value Float64` | `(region, metric, node, ts)` | 30 days |
| `samples_5m` | materialized view over `samples`: 5-minute `avg/max/last` per `(region, metric, node, workload, labels)` | same | 400 days |
| `usage_hourly` | `ts`, `owner`, `is_team`, `dimension`, `used`, `limit` | `(owner, dimension, ts)` | 2 years |
| `fleet_hourly` | `ts`, `region`, `nodes_total`, `nodes_ready`, `agents_ready`, `live_workspaces`, `live_environments`, `snapshots`, `disk_gb`, `cpu`, `memory_gb`, `pool_used_bytes`, `pool_total_bytes` | `(region, ts)` | 2 years |
| `alerts` | `ts`, `region`, `rule`, `state` (firing/ok/unknown), `detail` | `(region, rule, ts)` | 400 days |

`events` and `alerts` are `ReplacingMergeTree` on their `id` so at-least-once delivery never
double-counts; every reader queries `FINAL` or groups by `id`.

### A2. Feeds

**Redis stream consumer.** The existing `events` stream (`crates/storage/src/events.rs`, PR
and merge events) gets a second consumer group, `history`, read by the admin process with
`XREADGROUP` + periodic `XAUTOCLAIM` exactly as the worker does; a batch is `XACK`ed only after
its ClickHouse insert returns 200. The stream stays what CLAUDE.md says it is — a nudge, never
the record: if Redis is down the consumer idles and the beats below keep writing.

**Kubernetes watches.** The admin process runs one reflector per region (and one for
central) over `Workspace`, `Environment`, `Snapshot`, `Volume`, `QuotaRequest`/`Request`,
`Region`, `ClusterSettings` and `Node`, and turns transitions into `events` rows:
`workspace.created/started/stopped/deleted`, `environment.*`, `snapshot.ready/deleted`,
`volume.moved/released/unavailable`, `request.opened/approved/denied`, `region.activated/
deactivated`, `node.ready/notready/draining/drained/cordoned`. The event `id` is
`{uid}:{resourceVersion}:{transition}`, so a restart replaying the watch is idempotent.

**Audit.** Every audit row the admin process writes (`crate::audit::record`) is also an
`events` row (`kind = "admin.<action>"`). The object-store audit log stays the append-only
legal record; ClickHouse is the queryable copy.

**Beats.** The admin process runs an hourly `usage_hourly` beat (the `owners::fleet` fold,
one row per owner per dimension) and an hourly `fleet_hourly` beat (the `clusters` fold plus
the latest node pool gauges). Both compute from CRDs on every run — nothing is derived from an
earlier row.

**Monitors.** A new binary, `rustic-git-monitor`, runs as a one-replica Deployment in every
cluster (k3s `kube-system` per region, and `rustic-git` on AKS for the central tier). Every
15 s it lists pods annotated `prometheus.io/scrape: "true"` in its cluster (RBAC: `pods`
get/list), scrapes each `/metrics`, and ships the samples to central as one batch:
`POST /ingest/v1/samples` on the admin process, authenticated with the region's peer secret
(`RUSTIC_GIT_PEER_SECRET`, the same secret the agents use), never the superadmin claim. It
also evaluates the alert catalogue in `deploy/alerts.md` **with real windows** (it keeps the
last 15 minutes of samples in memory) and posts state transitions to `POST /ingest/v1/alerts`.
The ingest router is mounted on the admin process OUTSIDE `refuse_without_claim`, on its own
path prefix, and reaches k3s through a new Ingress path `dev.kloudlite.io/ingest/*` →
`rustic-git-admin` (the web Ingress host; the admin process still has no Ingress of its own for
`/admin/*`). Payloads are bounded (1 MiB, 5 000 rows) and a monitor that cannot reach central
buffers up to 15 minutes then drops oldest.

**Node gauges.** `rustic-git-agent` gains a 15 s stats beat exposing, on its own `/metrics`:
`node_pool_bytes_total`, `node_pool_bytes_used` (btrfs filesystem usage of the pool),
`node_cpu_cores`, `node_memory_bytes_total`, `node_memory_bytes_available`, `node_load1`,
`node_working_copies_running`. The monitor scrapes them like any other metric, which is how a
region's disk pool, CPU and memory bars get real numbers.

The on-request scrape from the previous plan (`GET /admin/monitoring/signals`) is retired:
signals are read from `alerts` (current state = latest row per rule) and workload restarts
from `samples`. A cluster with no monitor yet shows every rule `unknown` with the reason
"no monitor reporting for this region".

### A3. History API

`GET /admin/history/{series}?range=7d|30d|90d&step=1h|1d&region=&owner=` returns
`{ series: [{ts, value}], summary: {last, delta, min, max} }` for a fixed set of named series
the console needs (`pending_requests`, `firing_signals`, `owners_over_80`, `live_workspaces`,
`live_environments`, `decided_requests`, `time_to_decide_p50`, `pool_used`, `cpu_used`,
`memory_used`, `restarts`, `audit_events`, plus `usage:{owner}:{dimension}`). Each series is
one SQL statement in `history/series.rs`; an unknown series is 404; a missing ClickHouse is
`503 history unavailable`, which the web renders as a flat placeholder, never an error page.
`GET /admin/history/events?kind=&owner=&region=&from=&to=&cursor=` pages the events table
(the timelines on Overview, Owner and Cluster).

### A4. Deploy

`deploy/rustic-git.yaml` gains the ClickHouse StatefulSet, Service and PVC, the monitor
Deployment for AKS, the Ingress path, and `RUSTIC_GIT_CLICKHOUSE_URL` on the admin Deployment.
`deploy/k3s/monitor.yaml` (Deployment + ServiceAccount + Role for `pods` get/list in every
namespace) is applied per region; `agent-peer.yaml`'s metrics NetworkPolicy is widened to admit
the monitor's namespace. `deploy/alerts.md` gains a column "evaluated by monitor since v2".

## B. Generic requests

One cluster-scoped CRD `Request` (`rustic-git.io/v1alpha1`) replaces `QuotaRequest` for new
requests; existing `QuotaRequest` objects stay readable (the admin list unions both until a
one-shot migration copies them, then the old CRD is retired in a later release).

```yaml
spec:
  owner: acme            # person or team slug — truth, never a label
  kind: quota | access | region | other
  requestedBy: meera@…   # the signed-in user; a team member with role ≥ admin for a team
  reason: "…"            # required, shown to the decider
  quota:  { workspaces: 40 }                 # kind = quota: the RequestedQuota overlay
  access: { team: acme, role: admin }        # kind = access: join or change role on a team
  region: { region: westeurope-k3s }         # kind = region: use a region not yet enabled for the owner
  other:  { title: "…", body: "…" }          # kind = other: free text
status:
  state: pending | approved | denied
  decidedBy, decidedAt, note                  # note required on deny, optional on approve
  resolution: "…"                             # what approve did (quota written / role set / free text)
```

Rules: one pending request per owner **per kind**; the API refuses a second with 409.
Approve semantics: quota → write the `Quota` then mark (as today, editable values allowed);
access → the admin process asks the server tier (peer route) to set the membership/role, then
marks; region → records the grant on the `OwnerBinding` (`status.regions`), then marks —
`/v1` placement honours it once per-owner region gating exists, and until then it is a
recorded decision only (said plainly in the decision panel); other → a required free-text
resolution, then marks. Every decision audits and emits `request.approved/denied`.

User side: the 409 dialog keeps opening a quota request; a "New request" entry in the profile
dropdown opens a kind picker (quota / access / region / other) and the matching small form;
"My requests" lists the caller's own with state and note. Nothing shows limits or quotas.

## C. Console v2

The ten approved screens in `docs/superpowers/design/superadmin-console/`, built with the
app's components and tokens, **full width** (the superadmin place drops the centred container;
the content column stretches with a 28 px gutter). Shared section chrome — eyebrow, title,
count chip, right toolbar — as one component, used everywhere. KPI strips read A3; capacity
bars read the latest node gauges; timelines read `history/events`; Requests reads B; the
decision panel is one component with a kind-specific facts block. Every page polls 10 s and
keeps its previous content while refreshing. Empty states: one sentence + one action.

"Ensure they are working" is a gate, not a wish: every page is verified against a local
admin process + a local ClickHouse (docker) + a `mem://` store with seeded data, by screenshot,
before merge; the e2e script gains one assertion per history series.

## Not doing

Prometheus/Grafana (the monitor covers alerting; Grafana can point at ClickHouse later);
per-owner region gating in placement (B records the grant, placement follows in a later spec);
multi-node ClickHouse; retention below the TTLs above.

## Decisions to confirm (defaults applied if unchanged)

1. ClickHouse in-cluster on AKS, single node, 100 Gi PVC (vs a managed service).
2. Monitor ships to central through `dev.kloudlite.io/ingest/*` with the peer secret (vs a
   per-region ClickHouse or a VPN).
3. Retention: samples 30 d raw / 400 d at 5-minute rollup; usage and fleet 2 y; events forever.
4. `Request` replaces `QuotaRequest` (old objects readable, migrated once, retired later).
5. Region requests are recorded grants until per-owner region gating exists.
