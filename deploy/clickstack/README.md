# ClickStack

ClickHouse + an OpenTelemetry gateway collector + HyperDX, from the official charts
(<https://clickhouse.github.io/ClickStack-helm-charts>). This is the history layer's substrate: the
collectors write `default.otel_*`, and the admin process owns a second database, `rustic`, which it
migrates at boot.

Applied by hand, like the k3s side — not by `deploy/roll.sh`, which only rolls our own images.

The value files were written against `helm show values clickstack/clickstack --version 3.2.0`,
`clickstack/clickstack-operators --version 1.1.0` and the upstream
`open-telemetry/opentelemetry-collector` chart the stack aliases as `otel-collector`. One key
could not be checked without a running install and carries a `# verify:` comment in
`clickstack-values.yaml`: the name of the collector image's built-in ClickHouse exporter, which is
what the raw-metric `ttl` is merged onto. Re-read it before bumping a chart version. Every object
name below was taken from `helm template clickstack /tmp/clickstack -f …`, so they hold for a
release named `clickstack` and no other.

## Install

```sh
helm repo add clickstack https://clickhouse.github.io/ClickStack-helm-charts
helm repo update

# 1. The operators (ClickHouse + MongoDB). Pin the version; never install unpinned.
helm upgrade --install clickstack-operators clickstack/clickstack-operators \
  --version 1.1.0 --namespace clickstack --create-namespace \
  -f deploy/clickstack/operators-values.yaml

# 2. The stack.
helm upgrade --install clickstack clickstack/clickstack \
  --version 3.2.0 --namespace clickstack \
  -f deploy/clickstack/clickstack-values.yaml
```

## The one manual step: the ingestion API key

HyperDX mints the key the collectors authenticate with; nothing in a values file can create it.

1. Open `https://hyperdx.dev.kloudlite.io` and create the first account. Do this **immediately**
   after the install — the first account is unauthenticated by design, which is also why the
   ingress carries basic auth (next section) before anyone can reach that form.
2. Team Settings → API Keys → copy the **ingestion** key.
3. Put it where the agent collectors read it, in **every** cluster:

```sh
kubectl -n kube-system create secret generic rustic-git-otel \
  --from-literal=key='<ingestion key>'          # each k3s region
kubectl -n rustic-git create secret generic rustic-git-otel \
  --from-literal=key='<ingestion key>'          # AKS
```

The basic-auth Secret in front of HyperDX's ingress, created before the first sign-in above:

```sh
htpasswd -c auth <superadmin-username>
kubectl -n clickstack create secret generic hyperdx-basic-auth --from-file=auth
rm auth
```

That is the "restricted to the superadmin path" the values file's annotations name: nginx refuses
the request before it reaches HyperDX, and the credentials are the superadmin's own — the same
person the `superadmin: true` claim is minted for, and nobody else has a reason to open this host.

## Wiring the admin process

The chart mints two ClickHouse users of its own in `clickstack-secret`: `app`
(`CLICKHOUSE_APP_PASSWORD`, `SELECT` on `default` only) and `otelcollector`
(`CLICKHOUSE_PASSWORD`, the exporter's writer). Neither can create `rustic`, so the admin process
gets a third user:

```sh
kubectl -n clickstack get secret clickstack-secret -o jsonpath='{.data}' | jq   # the chart's two
kubectl -n clickstack get svc                                                  # the ClickHouse Service name
```

```sql
CREATE USER IF NOT EXISTS rustic IDENTIFIED BY '<password>';
GRANT SELECT ON default.* TO rustic;
GRANT CREATE, INSERT, SELECT, ALTER ON rustic.* TO rustic;
```

Then, in the AKS namespace the admin process runs in:

```sh
kubectl -n rustic-git create secret generic rustic-git-clickhouse \
  --from-literal=user=rustic --from-literal=password='<password>'
```

`deploy/rustic-git.yaml` reads exactly that Secret for `RUSTIC_GIT_CLICKHOUSE_USER` /
`RUSTIC_GIT_CLICKHOUSE_PASSWORD`, both `optional: true`, and hard-codes
`RUSTIC_GIT_CLICKHOUSE_URL` as `http://clickstack-clickhouse.clickstack.svc:8123` — the chart's
fullname helper with the release named `clickstack`, so a release under any other name needs that
env changed too.

The admin process migrates `rustic` itself on its next start; `kubectl logs` shows
`clickhouse migrations applied` once and `clickhouse schema up to date` on every restart after.

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
