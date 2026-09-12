#!/usr/bin/env bash
# Run one suite as a Job from its pinned CronJob and follow it, failing fast:
#   deploy/dev/run-job.sh <fast|hourly|weekly|monthly>
# On the first failed step the probe POD is force-killed (a plain `delete job` leaves it running
# through its grace period — it went on to run a drain drill and labelled a node), the Job is
# deleted, the run's row is closed and its objects swept (deploy/dev/pod/close-run.py), and any
# decommission label a drill left on a node is removed. Exit 0 only on `state: passed` — a run
# that SKIPPED any step finishes `skipped` and exits 2, because a run made of no samples is not
# evidence the fleet is well (hourly-1789188203 met a roll guard after stage 2, skipped the rest
# and printed "hourly passed", 2026-09-12).
set -uo pipefail
SUITE=${1:?suite}
case "$SUITE" in fast) T=slo-probe;; hourly) T=slo-hourly;; weekly|monthly) T=slo-drill;; *) echo "unknown suite $SUITE" >&2; exit 2;; esac
ACTIVE=$(kubectl -n kloudlite get jobs -o json | python3 -c 'import sys,json; print(" ".join(j["metadata"]["name"] for j in json.load(sys.stdin)["items"] if (j.get("status",{}).get("active") or 0)>0))')
[ -z "$ACTIVE" ] || { echo "a probe Job is active: $ACTIVE" >&2; exit 3; }
# A manual run and the cron's own run share one owner and its quota (two workspaces), so one that
# straddles the cron's firing fails on `quota.refused` and files a false sample. Refuse to start
# within the two minutes before the CronJob's next tick; the active-Job check above covers after.
SCHED=$(kubectl -n kloudlite get cronjob "kloudlite-slo-$SUITE" -o jsonpath='{.spec.schedule}')
python3 - "$SCHED" <<'PY' || exit 3
import sys, datetime
mins, hour = sys.argv[1].split()[0], sys.argv[1].split()[1]
allowed = set()
for part in mins.split(","):
    if part.startswith("*/"): allowed |= set(range(0, 60, int(part[2:])))
    elif part == "*": allowed = set(range(60))
    else: allowed.add(int(part))
now = datetime.datetime.now(datetime.timezone.utc)
for ahead in range(0, 121, 30):
    t = now + datetime.timedelta(seconds=ahead)
    if t.minute in allowed and (hour == "*" or t.hour == int(hour)) and t.second < 30:
        sys.exit(f"the cron fires at :{t.minute:02d}, within two minutes; run after it finishes")
PY
J=$SUITE-$(date -u +%H%M%S)
kubectl -n kloudlite create job "$J" --from="cronjob/kloudlite-slo-$SUITE" >/dev/null || exit 1
for _ in $(seq 1 60); do
  P=$(kubectl -n kloudlite get pods -l job-name="$J" -o jsonpath='{.items[0].metadata.name}' 2>/dev/null)
  case "$(kubectl -n kloudlite get pod "$P" -o jsonpath='{.status.phase}' 2>/dev/null)" in Running|Succeeded|Failed) break;; esac
  sleep 5
done
[ -n "${P:-}" ] || { echo "$J: no pod"; exit 3; }
echo "$J running as $P; follow with: kubectl -n kloudlite logs -f $P"
DEV=$(kubectl -n kloudlite get pods -l app=dev -o jsonpath='{.items[0].metadata.name}')
LOG=$(mktemp)
kubectl -n kloudlite logs -f "$P" 2>/dev/null | tee "$LOG" | grep -E --line-buffered '"ok":false|slo.run.finished|slo.stage.done' | while read -r l; do
  echo "$l" | cut -c1-400
  case "$l" in *'"ok":false'*|*slo.run.finished*) break;; esac
done
RUN=$(grep -o '"run_id":"[^"]*"' "$LOG" | head -1 | cut -d'"' -f4)
FIN=$(grep -o 'slo.run.finished.*' "$LOG" | head -1)
VERDICT=$(echo "$FIN" | grep -o '"state":"[a-z]*"' | cut -d'"' -f4)
WHY=$(echo "$FIN" | grep -o '"skipped_id":"[^"]*","reason":"[^"]*"' | sed 's/"skipped_id":"//;s/","reason":"/: /;s/"$//')
COUNTS=$(echo "$FIN" | grep -o '"passed":[0-9]*,"failed":[0-9]*,"skipped":[0-9]*' | tr -d '"')
rm -f "$LOG"
# A dead log stream is not a verdict: `kubectl logs -f` dropped mid-stage on 2026-09-12 during
# the Experience stage's silent minutes and this script killed a run that finished `passed` ten
# seconds later. While the pod still runs, wait for it, then read the whole log once.
if [ -z "$VERDICT" ]; then
  for _ in $(seq 1 90); do
    case "$(kubectl -n kloudlite get pod "$P" -o jsonpath='{.status.phase}' 2>/dev/null)" in Running) sleep 10;; *) break;; esac
  done
  FIN=$(kubectl -n kloudlite logs "$P" 2>/dev/null | grep -o 'slo.run.finished.*' | head -1)
  VERDICT=$(echo "$FIN" | grep -o '"state":"[a-z]*"' | cut -d'"' -f4)
  WHY=$(echo "$FIN" | grep -o '"skipped_id":"[^"]*","reason":"[^"]*"' | sed 's/"skipped_id":"//;s/","reason":"/: /;s/"$//')
  COUNTS=$(echo "$FIN" | grep -o '"passed":[0-9]*,"failed":[0-9]*,"skipped":[0-9]*' | tr -d '"')
  [ -n "$VERDICT" ] && echo "log stream dropped; verdict read from the finished pod"
fi
if [ -z "$VERDICT" ]; then
  kubectl -n kloudlite delete pod "$P" --grace-period=0 --force >/dev/null 2>&1
  kubectl -n kloudlite delete job "$J" --wait=false >/dev/null 2>&1
  echo "FAIL FAST: $J killed"
fi
[ -n "$RUN" ] && kubectl -n kloudlite exec -c dev "$DEV" -- /work/src/deploy/dev/pod/close-run.py "$T" "$RUN"
K3S="${K3S_KUBECONFIG:-$(dirname "$0")/../../.local/k3s.yaml}"
for n in $(kubectl --kubeconfig "$K3S" get nodes -l kloudlite.io/decommission=true -o name 2>/dev/null); do
  kubectl --kubeconfig "$K3S" label "$n" kloudlite.io/decommission- >/dev/null && kubectl --kubeconfig "$K3S" annotate "$n" kloudlite.io/decommission-status- >/dev/null; echo "undid a drill's decommission label on $n"
done
case "$VERDICT" in
  passed) kubectl -n kloudlite delete job "$J" >/dev/null 2>&1; echo "$SUITE passed ($RUN)"; exit 0;;
  skipped) echo "$SUITE skipped ($RUN): ${WHY:-no reason recorded} [${COUNTS:-}]"; exit 2;;
  "") exit 1;;
  *) echo "$SUITE $VERDICT ($RUN) [${COUNTS:-}]"; exit 2;;
esac
