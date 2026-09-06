#!/usr/bin/env bash
# Pull, then run tests in the pod under pm2 (`pm2 logs test`). Arguments go to `cargo test`.
#   deploy/dev/test.sh -p kloudlite-agent-bin --lib
set -euo pipefail
POD=$(kubectl -n kloudlite get pods -l app=dev -o jsonpath='{.items[0].metadata.name}')
kubectl -n kloudlite exec "$POD" -- /work/src/deploy/dev/pod/run.sh test "cd /work/src && git pull -q --ff-only origin master && cargo test --locked $* 2>&1 | grep -E '^test result|FAILED|panicked|error(\\[|:)' | head -40"
