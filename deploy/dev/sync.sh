#!/usr/bin/env bash
# rsync the working tree into the dev pod (deploy/dev/builder.yaml) and build there.
#   deploy/dev/sync.sh                    # sync + incremental build of every binary
#   deploy/dev/sync.sh --slo              # sync + build kloudlite-slo and kl only
# Then: deploy/dev/slo.sh <suite>         # run the probe from the pod, exactly as the CronJob does
set -euo pipefail
cd "$(dirname "$0")/../.."
POD=$(kubectl -n kloudlite get pods -l app=dev -o jsonpath='{.items[0].metadata.name}')
kubectl -n kloudlite exec "$POD" -- test -x /work/bin/crane 2>/dev/null || { echo "dev pod is still installing its tools"; exit 1; }
# rsync over `kubectl exec` (deploy/dev/krsh.sh): no ssh, no credentials, and --delete so a removed
# file cannot linger on the builder and get compiled in.
rsync -az --delete --blocking-io --exclude target --exclude node_modules --exclude web --exclude .local \
  --rsh="$(pwd)/deploy/dev/krsh.sh" . "$POD:/work/src/"
BINS="--bins"
[ "${1:-}" = "--slo" ] && BINS="--bin kloudlite-slo --bin kl"
kubectl -n kloudlite exec "$POD" -- bash -c "cd /work/src && cargo build --profile dev-image --locked $BINS 2>&1 | tail -3"
