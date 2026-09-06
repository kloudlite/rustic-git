#!/usr/bin/env bash
# Attach to the pod's tmux session: the running suite is in a window named after it; `shell` is a
# shell at /work/src. Detach with Ctrl-b d; scroll with Ctrl-b [ (q to leave); windows: Ctrl-b n/p.
#   deploy/dev/attach.sh            # attach (creates the session if none)
#   deploy/dev/attach.sh lnav       # browse the newest run log in lnav instead
set -euo pipefail
POD=$(kubectl -n kloudlite get pods -l app=dev -o jsonpath='{.items[0].metadata.name}')
if [ "${1:-}" = "lnav" ]; then
  exec kubectl -n kloudlite exec -it "$POD" -- bash -c 'lnav "$(ls -t /work/runs/*.log | head -1)"'
fi
exec kubectl -n kloudlite exec -it "$POD" -- bash -c 'tmux has-session -t slo 2>/dev/null || tmux new-session -d -s slo -n shell -c /work/src; exec tmux attach -t slo'
