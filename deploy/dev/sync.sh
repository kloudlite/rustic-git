#!/usr/bin/env bash
# Bring the pod's checkout to origin/master and build there, under pm2 (`pm2 logs build`).
#   deploy/dev/sync.sh            # pull + incremental build of every binary
#   deploy/dev/sync.sh --slo      # pull + build kloudlite-slo and kl-connect only
set -euo pipefail
POD=$(kubectl -n kloudlite get pods -l app=dev -o jsonpath='{.items[0].metadata.name}')
BINS="--bins"; [ "${1:-}" = "--slo" ] && BINS="--bin kloudlite-slo --bin kl-connect"
kubectl -n kloudlite exec "$POD" -- /work/src/deploy/dev/pod/run.sh build "cd /work/src && git pull -q --ff-only origin master && git log --oneline -1 && cargo build --profile dev-image --locked $BINS 2>&1 | tail -3"
