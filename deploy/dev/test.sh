#!/usr/bin/env bash
# Sync, then run tests and clippy in the dev pod. Arguments go to `cargo test` verbatim.
#   deploy/dev/test.sh -p kloudlite-agent-bin --lib
#   deploy/dev/test.sh --workspace          # everything, as CI runs it (minutes, not seconds)
set -euo pipefail
cd "$(dirname "$0")/../.."
POD=$(kubectl -n kloudlite get pods -l app=dev -o jsonpath='{.items[0].metadata.name}')
rsync -az --delete --blocking-io --exclude target --exclude node_modules --exclude web --exclude .local \
  --rsh="$(pwd)/deploy/dev/krsh.sh" . "$POD:/work/src/"
kubectl -n kloudlite exec "$POD" -- bash -c "cd /work/src && cargo test --locked $* 2>&1 | grep -E '^test result|FAILED|panicked|error(\[|:)' | head -40"
