#!/usr/bin/env bash
# Watch runs in the pod through pm2.
#   deploy/dev/attach.sh            # pm2 logs, every suite, live (Ctrl-C to leave; the run goes on)
#   deploy/dev/attach.sh weekly     # one suite's log
#   deploy/dev/attach.sh monit      # pm2's dashboard
#   deploy/dev/attach.sh list       # what is running
#   deploy/dev/attach.sh shell      # tmux shell at /work/src (Ctrl-b d detaches)
set -euo pipefail
POD=$(kubectl -n kloudlite get pods -l app=dev -o jsonpath='{.items[0].metadata.name}')
case "${1:-logs}" in
  monit) exec kubectl -n kloudlite exec -it "$POD" -- pm2 monit ;;
  list)  exec kubectl -n kloudlite exec -it "$POD" -- pm2 list ;;
  shell) exec kubectl -n kloudlite exec -it "$POD" -- bash -c 'tmux has-session -t slo 2>/dev/null || tmux new-session -d -s slo -n shell -c /work/src; exec tmux attach -t slo' ;;
  logs)  exec kubectl -n kloudlite exec -it "$POD" -- pm2 logs --lines 200 ;;
  *)     exec kubectl -n kloudlite exec -it "$POD" -- pm2 logs "$1" --lines 200 ;;
esac
