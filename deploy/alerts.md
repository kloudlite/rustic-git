# Alerts

Every pod is annotated `prometheus.io/scrape` (no Operator assumed); the server tier serves
`/metrics` on the peer port (8081), every other binary on `RUSTIC_GIT_METRICS_ADDR` (9464).
Structured logs: `RUSTIC_GIT_LOG_FORMAT=json` on any pod. Node/disk signals come from
node-exporter, which is not deployed by this repo — install it before the last two rules fire.

Metric names below are the ones the binaries emit; `deploy/rustic-git.yaml` sets no `job` labels,
so select by `pod`/`container` from the kubernetes-pods scrape config.

| Alert | PromQL (for 5m unless noted) | Why |
|---|---|---|
| **LeaderUnreachable** | `absent(up{pod="rustic-git-leader-0"} == 1)` for 2m | No leader means no claims: every repo move stalls and 421s pile up on followers. |
| **LeaseRenewFailing** | `sum by (pod) (rate(ownership_renew_failures_total[5m])) > 0` for 3m | A node that cannot renew loses its leases at the TTL; another node claims, and its warm databases must close. |
| **DbFenceDetected** | `increase(db_fence_detected_total[10m]) > 0` | The invariant violation: two nodes opened one SlateDB. Zero is the only acceptable value. |
| **Http5xxRate** | `sum by (listener, class) (rate(http_requests_total{status="5xx"}[5m])) / sum by (listener, class) (rate(http_requests_total[5m])) > 0.05` | Per listener and route class so a registry outage is not hidden by healthy git traffic. |
| **MisdirectedWrites** | `sum(rate(http_requests_total{status="421"}[5m])) > 0.1` for 10m | 421s during a roll are expected; sustained ones mean the pods disagree about `RUSTIC_GIT_LEADER`. |
| **ReconcileErrors** | `sum by (kind) (rate(reconciles_total{result="error"}[10m])) / sum by (kind) (rate(reconciles_total[10m])) > 0.2` | A controller in an error loop keeps retrying with backoff; the ratio is what shows it. |
| **TunnelSaturation** | `max by (pod) (gateway_open_tunnels) > 800` | `MAX_TUNNELS` is 1000 per gateway pod; refusals start with 503 past it. |
| **WorkerHeartbeatStale** | `absent(up{container="worker"} == 1)` for 5m, plus `increase(kube_pod_container_status_restarts_total{container="worker"}[1h]) > 3` | The liveness probe only restarts; this pages when it keeps happening. Merge starvation itself: `increase(merge_outcomes_total[30m]) == 0 and increase(git_pack_requests_total{op="receive"}[30m]) > 0` is the softer signal. |
| **PoolAlmostFull** | `(node_filesystem_avail_bytes{mountpoint="/wspool-prod"} / node_filesystem_size_bytes{mountpoint="/wspool-prod"}) < 0.2` | btrfs past 80% starts failing allocations before `df` says full. Node-exporter. |
| **NodeDiskAlmostFull** | `(node_filesystem_avail_bytes{mountpoint="/"} / node_filesystem_size_bytes{mountpoint="/"}) < 0.15` | The worker's merge caches and the slatedb object cache live on the root disk. Node-exporter. |

When one of these fires on a cluster that is gone rather than sick, the rebuild is
`deploy/RECOVERY.md`; the retention switches it assumes were on are `deploy/BACKUPS.md`.

Useful dashboards, not alerts: `ownership_map_size` (set on each leader sweep),
`ownership_claims_total{result="moved"}` against `db_fence_detected_total`,
`rate(git_pack_bytes_in_total[5m])` and `rate(registry_blob_bytes_{in,out}_total[5m])`,
`histogram_quantile(0.95, sum by (le, class) (rate(http_request_duration_seconds_bucket[5m])))`.
