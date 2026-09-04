# Alerts

Every pod is annotated `prometheus.io/scrape` (no Operator assumed); every binary, the server tier
included, serves `/metrics` on `RUSTIC_GIT_METRICS_ADDR` (9464). The region's OTel collector
(`deploy/k3s/otel-agent.yaml`) is what reads that annotation now, and it exports to ClickStack.
Structured logs: every pod runs with `RUSTIC_GIT_LOG_FORMAT=json`; the collectors parse it, so HyperDX has `severity`, `code.namespace` (the Rust module) and each call-site field as columns, and every row carries `service.name` (the workload), `service.instance.id` (the pod), `region` and `tier`.

Every rule below is evaluated TWICE, from this one table: HyperDX pages a human, and the admin
process evaluates the same rule as SQL over the collector's `otel_metrics_*` tables every 30 s and
writes state transitions to `rustic.alerts`, which is what the console's Signals table reads
(`crates/workspaces/src/history/alerts.rs`). Adding a rule here means adding it in both places —
`the_catalogue_matches_deploy_alerts_md` fails the build if the Rust half is missed, and the
HyperDX half is the "HyperDX alert" column. Node and disk signals come from the collector's
`kubeletstats` receiver and the agent's own `node_pool_bytes_*` gauges; node-exporter is not
deployed and is no longer needed.

Metric names below are the ones the binaries emit; `deploy/rustic-git.yaml` sets no `job` labels,
so select by `pod`/`container` from the kubernetes-pods scrape config. In HyperDX the same labels
are `ResourceAttributes['k8s.pod.name']` and `['k8s.container.name']`, and every rule is scoped by
`ResourceAttributes['region']` — the value the collector's `resource` processor stamps. Create each
alert on the **Metrics** source, grouped by `region`, notifying the ops webhook.

| Alert | PromQL (for 5m unless noted) | HyperDX alert | Why |
|---|---|---|---|
| **NoLeader** | `sum(ownership_is_leader) != 1` for 2m | Search `MetricName:ownership_is_leader`, chart `sum(Value)` grouped by `region`, alert when the value `!= 1` for 2m | Zero: nobody holds the lease, so no claim in the fleet succeeds; two: the epoch check failed and the fence is all that stands between two writers. `ownership_demotions_total` rising with it says which pod keeps losing the lease. |
| **LeaseRenewFailing** | `sum by (pod) (rate(ownership_renew_failures_total[5m])) > 0` for 3m | Search `MetricName:ownership_renew_failures_total`, chart the 5m delta grouped by `region`, alert when `> 0` for 3m | A node that cannot renew loses its leases at the TTL; another node claims, and its warm databases must close. |
| **DbFenceDetected** | `increase(db_fence_detected_total[10m]) > 0` | Search `MetricName:db_fence_detected_total`, chart the 10m delta grouped by `region`, alert when `> 0` (no `for` — one fence is the page) | The invariant violation: two nodes opened one SlateDB. Zero is the only acceptable value. |
| **Http5xxRate** | `sum by (listener, class) (rate(http_requests_total{status="5xx"}[5m])) / sum by (listener, class) (rate(http_requests_total[5m])) > 0.05` | Search `MetricName:http_requests_total`, chart the 5m-delta ratio of `Attributes['status']:'5xx'` to all, grouped by `region`/`listener`/`class`, alert when `> 0.05` for 5m | Per listener and route class so a registry outage is not hidden by healthy git traffic. |
| **MisdirectedWrites** | `sum(rate(http_requests_total{status="421"}[5m])) > 0.1` for 10m | Search `MetricName:http_requests_total AND Attributes.status:"421"`, chart the per-second 5m delta grouped by `region`, alert when `> 0.1` for 10m | 421s during a roll are expected; sustained ones mean the pods disagree about who holds the leader lease. |
| **ReconcileErrors** | `sum by (kind) (rate(reconciles_total{result="error"}[10m])) / sum by (kind) (rate(reconciles_total[10m])) > 0.2` | Search `MetricName:reconciles_total`, chart the 10m-delta ratio of `Attributes['result']:'error'` to all, grouped by `region`/`kind`, alert when `> 0.2` for 5m | A controller in an error loop keeps retrying with backoff; the ratio is what shows it. |
| **TunnelSaturation** | `max by (pod) (gateway_open_tunnels) > 800` | Search `MetricName:gateway_open_tunnels`, chart `max(Value)` grouped by `region`, alert when `> 800` for 5m | `MAX_TUNNELS` is 1000 per gateway pod; refusals start with 503 past it. |
| **WorkerHeartbeatStale** | `absent(up{container="worker"} == 1)` for 5m, plus `increase(kube_pod_container_status_restarts_total{container="worker"}[1h]) > 3` | Search `MetricName:"k8s.container.restarts" AND ResourceAttributes.k8s.container.name:"worker"`, chart the 1h delta per pod grouped by `region`, alert when `max > 3` (the whole hour is the window; there is no second `for`) | The liveness probe only restarts; this pages when it keeps happening. Merge starvation itself: `increase(merge_outcomes_total[30m]) == 0 and increase(git_pack_requests_total{op="receive"}[30m]) > 0` is the softer signal. HyperDX has no `absent()`, so only the restart half is an alert — a worker that is simply gone shows as an uncovered window, which the console reports `unknown` rather than `ok`. |
| **PoolAlmostFull** | `node_pool_bytes_used / node_pool_bytes_total > 0.8`, per node | Search `MetricName:node_pool_bytes_used OR MetricName:node_pool_bytes_total`, chart their ratio grouped by `region`/`k8s.node.name`, alert when the WORST node is `> 0.8` for 5m | btrfs past 80% starts failing allocations before `df` says full. The agent's own gauges, so no node-exporter. |
| **NodeDiskAlmostFull** | `k8s.node.filesystem.usage / (usage + available) > 0.85`, per node | Search `MetricName:"k8s.node.filesystem.usage" OR MetricName:"k8s.node.filesystem.available"`, chart `usage / (usage + available)` grouped by `region`/`k8s.node.name`, alert when the WORST node is `> 0.85` for 5m | The worker's merge caches and the slatedb object cache live on the root disk. `kubeletstats`, so no node-exporter. |

The last two rows' PromQL is written against the metrics that actually exist now, not against
node-exporter's `node_filesystem_*` — that is the change that made them evaluable at all, since
they were permanently `unknown` while they depended on an exporter nobody deployed.

When one of these fires on a cluster that is gone rather than sick, the rebuild is
`deploy/RECOVERY.md`; the retention switches it assumes were on are `deploy/BACKUPS.md`.

Useful dashboards, not alerts: `ownership_map_size` (set on each leader sweep),
`ownership_claims_total{result="moved"}` against `db_fence_detected_total`,
`rate(git_pack_bytes_in_total[5m])` and `rate(registry_blob_bytes_{in,out}_total[5m])`,
`histogram_quantile(0.95, sum by (le, class) (rate(http_request_duration_seconds_bucket[5m])))`.
