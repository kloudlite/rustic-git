# ClickStack

ClickHouse + an OpenTelemetry gateway collector + HyperDX, from the official charts
(<https://clickhouse.github.io/ClickStack-helm-charts>). This is the history layer's substrate: the
collectors write `default.otel_*`, and the admin process owns a second database, `kloudlite`, which it
migrates at boot.

Applied by hand, like the k3s side — not by `deploy/roll.sh`, which only rolls our own images.

The value files were written against `helm show values clickstack/clickstack --version 3.2.0`,
`clickstack/clickstack-operators --version 1.1.0` and the upstream
`open-telemetry/opentelemetry-collector` chart the stack aliases as `otel-collector`. One thing could not be checked without a running install and carries a `# verify:` comment: the
name of the collector image's built-in ClickHouse exporter, which the raw-metric `ttl` is merged
onto (`clickstack-values.yaml`). The ClickHouse Service host the admin process connects to was
read off the first install (below). Re-read both before bumping a chart version. Every other object name below was taken from
`helm template clickstack …`, so they hold for a release named `clickstack` and no other.

## Install

```sh
helm repo add clickstack https://clickhouse.github.io/ClickStack-helm-charts
helm repo update

# 1. The operators (ClickHouse + MongoDB). Pin the version; never install unpinned.
helm upgrade --install clickstack-operators clickstack/clickstack-operators \
  --version 1.1.0 --namespace clickstack --create-namespace \
  -f deploy/clickstack/operators-values.yaml

# 2. HyperDX's database: a `hyperdx` database on the platform's Cosmos DB for MongoDB account
#    (kolomi-rg/kloudlite-kloudlite-git, MongoDB API 7.0, serverless). HyperDX creates it on first
#    connect; the Secret carries the account's connection string with `/hyperdx` as the path.
#    Runs on YOUR shell — it handles a credential.
SUB=$(az account show --query id -o tsv)
CS=$(az rest --method post --url "https://management.azure.com/subscriptions/$SUB/resourceGroups/kolomi-rg/providers/Microsoft.DocumentDB/databaseAccounts/kloudlite-kloudlite-git/listConnectionStrings?api-version=2024-05-15" --query 'connectionStrings[0].connectionString' -o tsv)
URI=$(python3 -c 'import sys,urllib.parse as u; p=u.urlsplit(sys.argv[1]); print(u.urlunsplit((p.scheme,p.netloc,"/hyperdx",p.query,"")))' "$CS")
kubectl -n clickstack create secret generic hyperdx-mongo --from-literal=MONGO_URI="$URI"
unset CS URI

# 3. The stack. HyperDX reads `hyperdx.config` through envFrom at start and the chart stamps no
#    checksum, so a values change under `config` needs `kubectl -n clickstack rollout restart
#    deploy/clickstack-app` after the upgrade.
helm upgrade --install clickstack clickstack/clickstack \
  --version 3.2.0 --namespace clickstack \
  -f deploy/clickstack/clickstack-values.yaml
```

The operators chart also installs the MongoDB operator (no off switch in 1.1.0). With
`mongodb.enabled: false` it has nothing to reconcile, so it was deleted by hand on 2026-09-04
(Deployment, webhook Service, its RBAC and the `mongodbcommunity` CRD). A `helm upgrade` of the
operators chart brings it back; delete it again afterwards.

## The one manual step: the ingestion API key

HyperDX mints the key the collectors authenticate with; nothing in a values file can create it.

1. Open `https://hyperdx-dev.kloudlite.io` and create the first account. Do this **immediately**
   after the install — the first account is unauthenticated by design, and from then on HyperDX's
   own login is the only gate (no basic auth in front; it asked on every visit).
2. Team Settings → API Keys → copy the **ingestion** key.
3. Put it where the agent collectors read it, in **every** cluster:

```sh
kubectl -n kube-system create secret generic kloudlite-git-otel \
  --from-literal=key='<ingestion key>'          # each k3s region
kubectl -n kloudlite-git create secret generic kloudlite-git-otel \
  --from-literal=key='<ingestion key>'          # AKS
```

Sign-up against the Cosmos account can answer `MongoServerError 16500 / TooManyRequests (429)`:
HyperDX creates its collections and indexes in one burst and the serverless tier throttles that.
Retrying the form works; enabling the account's `DisableRateLimitingResponses` capability
(server-side retry) makes it a non-event.

## Wiring the admin process

The chart mints two ClickHouse users of its own in `clickstack-secret`: `app`
(`CLICKHOUSE_APP_PASSWORD`, `SELECT` on `default` only) and `otelcollector`
(`CLICKHOUSE_PASSWORD`, the exporter's writer). Neither can create `kloudlite`, so the admin process
gets a third user:

```sh
kubectl -n clickstack get secret clickstack-secret -o jsonpath='{.data}' | jq   # the chart's two
kubectl -n clickstack get svc   # ClickHouse is `clickstack-clickhouse-clickhouse-headless`
```

```sql
CREATE USER IF NOT EXISTS kloudlite IDENTIFIED BY '<password>';
GRANT SELECT ON default.* TO kloudlite;
GRANT CREATE, INSERT, SELECT, ALTER, DROP TABLE ON kloudlite.* TO kloudlite;   -- DROP: an engine change rebuilds a table beside itself and swaps (migrations 8-15)
```

Then, in the AKS namespace the admin process runs in:

```sh
kubectl -n kloudlite-git create secret generic kloudlite-git-clickhouse \
  --from-literal=user=kloudlite --from-literal=password='<password>'
```

`deploy/kloudlite-git.yaml` reads exactly that Secret for `KLOUDLITE_GIT_CLICKHOUSE_USER` /
`KLOUDLITE_GIT_CLICKHOUSE_PASSWORD`, both `optional: true`, and hard-codes
`KLOUDLITE_GIT_CLICKHOUSE_URL` as `http://clickstack-clickhouse-clickhouse-headless.clickstack.svc:8123`
— the operator's name for the Service, not the chart's fullname (`helm template` cannot show it;
`kubectl -n clickstack get svc` did, on the first install). Re-check it after a chart or operator
bump: a wrong host is a silent 503 on every /admin/history route.

The admin process migrates `kloudlite` itself on its next start; `kubectl logs` shows
`history.migrations.applied` with `count` > 0 once, and `count=0` on every restart after.

## Alerts

`deploy/alerts.md` is the catalogue, and it is evaluated TWICE on purpose: HyperDX alerts page a
human, and the admin process evaluates the same rules in SQL for the console's Signals table. The
HyperDX definitions live in that file, next to each rule, in the "HyperDX alert" column. Create
them once from there; a rule added to the file must be added to both, or the two disagree with no
way to tell which is right.

## Recovery

Losing ClickHouse loses history and nothing else — no repository, workspace, snapshot or quota
lives here. Reinstall the chart, let the admin process re-migrate, and accept the gap. That is why
one replica is enough.

## Azure Monitor: Cosmos and Redis

The AKS cluster collector (`kloudlite-git-otel-cluster` in `deploy/kloudlite-git.yaml`) pulls the
two managed dependencies through the `azuremonitor` receiver — the directory's Cosmos account and
the Redis every server and worker nudges through. It authenticates with the service principal
`kloudlite-git-azuremonitor` (Monitoring Reader on kolomi-rg), whose credentials live in the
Secret `kloudlite-git-azuremonitor` (`tenant`, `client-id`, `client-secret`, `subscription`).
Created 2026-09-04 with `az ad sp create-for-rbac --role "Monitoring Reader" --scopes <rg id>`;
rotate by re-running that and replacing the Secret. Metrics land as `azure_<metric>_<aggregation>`
(`azure_totalrequestunits_total`, `azure_usedmemorypercentage_maximum`, …) with `region=central`.
Cosmos metrics exist at a five-minute grain only, hence the second receiver instance.
