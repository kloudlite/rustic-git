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

## A. History layer — on ClickStack

Owner (2026-09-04): "use clickstack for monitoring". ClickStack is ClickHouse's open-source
observability stack: ClickHouse + an OpenTelemetry collector (the ClickHouse exporter) +
HyperDX (UI, search, dashboards, alerts) + MongoDB for HyperDX's own state. We deploy it with
the official Helm charts (`https://clickhouse.github.io/ClickStack-helm-charts`:
`clickstack-operators`, then `clickstack`) on AKS in a `clickstack` namespace, and we do NOT
write a monitor binary or our own metrics tables: telemetry arrives through OpenTelemetry,
lands in the exporter's standard tables, and both HyperDX and our console read them.

### A1. Store

ClickHouse from the chart (one replica, 100 Gi PVC on `managed-csi`, ClusterIP only). Two
databases:

- `default` — the OTel exporter's tables, owned by the collector: `otel_metrics_gauge`,
  `otel_metrics_sum`, `otel_metrics_histogram`, `otel_logs`, `otel_traces`
  (`TimeUnix`, `MetricName`, `Value`, `Attributes`, `ResourceAttributes`, …). Retention is the
  exporter's `ttl` (30 d for raw metrics) plus a materialized 5-minute rollup we add
  (`rustic.metrics_5m`, 400 d) for the long sparklines.
- `rustic` — ours, owned by the admin process (`bins/api`, role `admin`, the only writer of
  this database, migrations at boot from `crates/workspaces/src/history/schema.rs`):
  `events` (ReplacingMergeTree on `id`; `ts, kind, actor, owner, target, region, attrs`;
  no TTL — the record), `usage_hourly` (`owner, is_team, dimension, used, limit`; 2 y),
  `fleet_hourly` (`region, nodes_total, nodes_ready, agents_ready, live_workspaces,
  live_environments, snapshots, disk_gb, cpu, memory_gb, pool_used_bytes, pool_total_bytes`;
  2 y), `alerts` (ReplacingMergeTree; `region, rule, state, detail`; 400 d).

Credentials: the chart's ClickHouse secret; the admin process gets `RUSTIC_GIT_CLICKHOUSE_URL`
(HTTP, 8123, user with rights on `rustic` and read on `default`). Optional everywhere — a
process without it runs as today and the console shows "history unavailable" for a series.
Access from Rust is ClickHouse's HTTP interface over `reqwest` (`JSONEachRow` in,
`JSONCompact` out); no new crate.

### A2. Telemetry: OpenTelemetry collectors, no custom monitor

- **Gateway collector** — the chart's OTel collector in `clickstack` (OTLP gRPC 4317 / HTTP
  4318, `authorization: <ingestion API key>` from HyperDX Team Settings; the key lives in
  Secret `rustic-git-otel` and is what every sender uses). Exposed to the regions through an
  Ingress path `otel.dev.kloudlite.io` (TLS, HTTP/2 for gRPC) — the k3s clusters cannot reach
  a ClusterIP on AKS.
- **Agent collectors** — the official `opentelemetry-collector-contrib` as a Deployment in
  every cluster (`deploy/k3s/otel-agent.yaml` per region; the same manifest in `rustic-git`
  on AKS), config in a ConfigMap: `prometheus` receiver scraping every pod annotated
  `prometheus.io/scrape: "true"` (`kubernetes_sd_configs`, 15 s), `k8s_cluster` and
  `kubeletstats` receivers for node/pod CPU, memory and filesystem, `k8sattributes` +
  `resource` processors stamping `region`, `batch`, and an `otlphttp` exporter to the
  gateway with the key. RBAC for service discovery in a header-table Role like the agent's.
  A collector that cannot reach the gateway retries with its own queue; nothing of ours
  buffers.
- **Logs** — the same agent collectors ship pod logs (`filelog` receiver on
  `/var/log/pods`) so HyperDX has logs beside metrics; our binaries keep plain `tracing`
  output, unchanged.
- **Node gauges** — `rustic-git-agent` gains a 15 s stats beat on its own `/metrics`:
  `node_pool_bytes_total`, `node_pool_bytes_used` (btrfs usage of the pool),
  `node_working_copies_running`. CPU, memory and load come from `kubeletstats`, not from us.

### A3. Alerts

The catalogue in `deploy/alerts.md` is evaluated in two places on purpose: HyperDX alerts
(saved searches with thresholds, notifying by webhook/email — the operator's pager) are
created once from the catalogue and documented beside it; and the admin process evaluates the
same rules every 30 s as SQL over `otel_metrics_*` with real `for` windows, writing state
transitions to `rustic.alerts`, which is what the console's Signals table and Overview read.
Two evaluators, one catalogue, so a difference is a bug in one of them, never a mystery.
The previous on-request scrape (`GET /admin/monitoring/signals`) keeps its response shape and
reads `rustic.alerts`; a region with no collector reporting shows every rule `unknown`.

### A4. Feeds into `rustic`

Unchanged from the first draft: the Redis `events` stream consumer group `history` in the
admin process (XREADGROUP/XAUTOCLAIM as the worker, XACK after insert — the stream stays a
nudge, never the record); per-region Kubernetes watches turning CRD/Node transitions into
`events` rows with idempotent ids (`{uid}:{resourceVersion}:{transition}`); audit rows dual-
written as `admin.<action>` events; hourly `usage_hourly` and `fleet_hourly` beats computed
from CRDs every run.

### A5. History API

`GET /admin/history/{series}?range=7d|30d|90d&step=1h|1d&region=&owner=` →
`{ series: [{ts, value}], summary: {last, delta, min, max} }` for the fixed series the console
needs (`pending_requests`, `firing_signals`, `owners_over_80`, `live_workspaces`,
`live_environments`, `decided_requests`, `time_to_decide_p50`, `pool_used`, `cpu_used`,
`memory_used`, `restarts`, `audit_events`, `usage:{owner}:{dimension}`); each series is one
SQL statement in `history/series.rs` over `rustic.*` or `otel_metrics_*`; unknown series 404;
no ClickHouse → `503 history unavailable` (the web renders a flat placeholder).
`GET /admin/history/events?kind=&owner=&region=&from=&to=&cursor=` pages `rustic.events`.
A "Open in HyperDX" link on Monitoring uses `RUSTIC_GIT_HYPERDX_URL` when set.

### A6. Deploy

`deploy/clickstack/` — the two Helm value files (operators, clickstack: one ClickHouse
replica with the PVC, HyperDX behind `hyperdx.dev.kloudlite.io` gated to superadmin emails at
the Ingress, the gateway collector with its Ingress), a README with the exact `helm` commands
and the one manual step (create the ingestion API key, store it in `rustic-git-otel`).
`deploy/rustic-git.yaml`: `RUSTIC_GIT_CLICKHOUSE_URL` and `RUSTIC_GIT_HYPERDX_URL` on the admin
Deployment, the AKS agent collector. `deploy/k3s/otel-agent.yaml` per region;
`agent-peer.yaml`'s metrics NetworkPolicy admits the collector's namespace. `deploy/alerts.md`
gains the HyperDX alert definitions.

## B. Generic requests

One cluster-scoped CRD `Request` (`rustic-git.io/v1alpha1`) replaces `QuotaRequest` for new
requests; existing `QuotaRequest` objects stay readable (the admin list unions both until a
one-shot migration copies them, then the old CRD is retired in a later release).

```yaml
spec:
  owner: acme            # person or team slug — truth, never a label; for kind=access the ASKER's slug (access.team names the team)
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
access → the admin process grants through its own directory handle (`Directory::grant_access`, add member + set role; no peer hop — the deciding process already holds the directory), then marks; region → records the grant on the owner's `Quota` (`spec.regions`, the one per-owner admin-written object; an `OwnerBinding` is per {owner, region} and controller-authored), then marks —
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

Prometheus/Grafana (ClickStack replaces both);
per-owner region gating in placement (B records the grant, placement follows in a later spec);
multi-node ClickHouse; retention below the TTLs above.

## Decisions to confirm (defaults applied if unchanged)

1. ClickStack via the official Helm charts on AKS, one ClickHouse replica, 100 Gi PVC (vs Managed ClickStack in ClickHouse Cloud).
2. Regions ship telemetry to the ClickStack gateway collector at `otel.dev.kloudlite.io`
   with the HyperDX ingestion key (vs a per-region ClickHouse or a VPN).
3. Retention: samples 30 d raw / 400 d at 5-minute rollup; usage and fleet 2 y; events forever.
4. `Request` replaces `QuotaRequest` (old objects readable, migrated once, retired later).
5. Region requests are recorded grants until per-owner region gating exists.
