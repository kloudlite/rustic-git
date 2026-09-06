#!/usr/bin/env bash
# Pull, then run tests in the dev pod. Arguments go to `cargo test` verbatim.
#   deploy/dev/test.sh -p kloudlite-agent-bin --lib
#   deploy/dev/test.sh --workspace          # everything, as CI runs it
set -euo pipefail
POD=$(kubectl -n kloudlite get pods -l app=dev -o jsonpath='{.items[0].metadata.name}')
kubectl -n kloudlite exec "$POD" -- bash -c "cd /work/src && git pull -q --ff-only origin master && cargo test --locked $* 2>&1 | grep -E '^test result|FAILED|panicked|error(\[|:)' | head -40"
