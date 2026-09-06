#!/usr/bin/env bash
# A shell (or one command) inside the dev pod, in the synced checkout.
#   deploy/dev/exec.sh                 # interactive shell at /work/src
#   deploy/dev/exec.sh cargo test -p kloudlite-workspaces --lib k8s
set -euo pipefail
POD=$(kubectl -n kloudlite get pods -l app=dev -o jsonpath='{.items[0].metadata.name}')
if [ $# -eq 0 ]; then exec kubectl -n kloudlite exec -it "$POD" -- bash -c 'cd /work/src && exec bash'; fi
exec kubectl -n kloudlite exec "$POD" -- bash -c "cd /work/src && $*"
