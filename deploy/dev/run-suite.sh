#!/usr/bin/env bash
# Run one probe suite from the dev pod, fail fast, and leave nothing behind.
#   deploy/dev/run-suite.sh weekly [--no-fail-fast]
# The suite runs as a process in the pod (nohup, log under /work/runs/). On the first failed step
# the process is killed, its `running` row is closed as failed through the admin api (or the next
# run of the suite yields to the ghost for ten minutes), and the failures are printed. Refuses to
# start while any SLO Job is active — a hand run and a scheduled one must never overlap.
set -euo pipefail
POD=$(kubectl -n kloudlite get pods -l app=dev -o jsonpath='{.items[0].metadata.name}')
FF=1; [ "${1:-}" = "--no-fail-fast" ] && FF=0
SUITE=${1:?fast|hourly|weekly|monthly}
case "$SUITE" in
  fast)    U=slo-probe;  O=slo-other;        K=/etc/slo-ssh-fast;   B=840  ;;
  hourly)  U=slo-hourly; O=slo-hourly-other; K=/etc/slo-ssh-hourly; B=3000 ;;
  weekly)  U=slo-drill;  O=slo-drill-other;  K=/etc/slo-ssh-drill;  B=6600 ;;
  monthly) U=slo-drill;  O=slo-drill-other;  K=/etc/slo-ssh-drill;  B=6900 ;;
  *) echo "unknown suite $SUITE" >&2; exit 2 ;;
esac
ACTIVE=$(kubectl -n kloudlite get jobs -o json | python3 -c 'import sys,json; print(" ".join(j["metadata"]["name"] for j in json.load(sys.stdin)["items"] if (j.get("status",{}).get("active") or 0)>0))')
[ -z "$ACTIVE" ] || { echo "refusing: probe Job(s) active: $ACTIVE" >&2; exit 3; }
LOG=/work/runs/$SUITE-$(date -u +%H%M).log
# The suite runs under pm2 as a one-shot process named after it: `pm2 logs <suite>` streams it,
# `pm2 monit` is the dashboard, `pm2 list` shows what is up (deploy/dev/attach.sh opens these).
# pm2 keeps the log under PM2_HOME; the run is also written to $LOG and to the pod's stdout
# (kubectl logs, HyperDX).
kubectl -n kloudlite exec "$POD" -- bash -c "mkdir -p /work/runs; pm2 describe $SUITE >/dev/null 2>&1 && pm2 delete $SUITE >/dev/null; \
  ps -o stat= -C kloudlite-slo 2>/dev/null | grep -qv Z && { echo 'a suite is already running in the pod' >&2; exit 3; }; \
  cd /work/src && KLOUDLITE_SLO_USER=$U KLOUDLITE_SLO_OTHER=$O KLOUDLITE_SLO_BUDGET_SECS=$B KLOUDLITE_SLO_SSH_KEY=$K/id_ed25519 \
  pm2 start --name $SUITE --no-autorestart --time --log $LOG --merge-logs /work/target/dev-image/kloudlite-slo -- run --suite $SUITE >/dev/null && \
  (tail -n +1 -f $LOG > /proc/1/fd/1 &) ; sleep 1; echo started $SUITE under pm2, log $LOG"
# The fail-fast watcher runs IN THE POD under pm2 (`pm2 logs watch-<suite>`), never on the laptop:
# deploy/dev/pod/watch.py stops the suite on the first failed step, closes its row and deletes its
# objects. This script returns as soon as both are started.
FFARG=""; [ "$FF" = 0 ] && FFARG="--no-fail-fast"
kubectl -n kloudlite exec "$POD" -- bash -c "pm2 delete watch-$SUITE >/dev/null 2>&1; pm2 start --name watch-$SUITE --no-autorestart --time --merge-logs python3 -- /work/src/deploy/dev/pod/watch.py $SUITE $U $LOG $FFARG >/dev/null && echo watcher started as watch-$SUITE"
echo "follow with: deploy/dev/attach.sh $SUITE   (or: deploy/dev/attach.sh watch-$SUITE)"
