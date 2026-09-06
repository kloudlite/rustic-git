#!/usr/bin/env bash
# Bring the pod's checkout to origin/master and build there. No rsync: the pod pulls from GitHub,
# so what runs in the cluster is always a pushed commit.
#   deploy/dev/sync.sh            # pull + incremental build of every binary
#   deploy/dev/sync.sh --slo      # pull + build kloudlite-slo and kl only
set -euo pipefail
POD=$(kubectl -n kloudlite get pods -l app=dev -o jsonpath='{.items[0].metadata.name}')
BINS="--bins"; [ "${1:-}" = "--slo" ] && BINS="--bin kloudlite-slo --bin kl"
kubectl -n kloudlite exec "$POD" -- bash -c "cd /work/src && git pull -q --ff-only origin master && git log --oneline -1 && cargo build --profile dev-image --locked $BINS 2>&1 | tail -3"
