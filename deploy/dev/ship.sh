#!/usr/bin/env bash
# Build and push the images from the pod (test + clippy + release build + buildkit push, all under
# pm2 as `ship`), then print the pin/roll line. See deploy/dev/pod/ship.sh.
#   deploy/dev/ship.sh            # full gate
#   deploy/dev/ship.sh --no-gate  # skip test + clippy
set -euo pipefail
POD=$(kubectl -n kloudlite get pods -l app=dev -o jsonpath='{.items[0].metadata.name}')
kubectl -n kloudlite exec "$POD" -- /work/src/deploy/dev/pod/run.sh ship "/work/src/deploy/dev/pod/ship.sh ${1:-}"
