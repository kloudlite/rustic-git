#!/usr/bin/env bash
# Follow a probe run live, one line per step: time, id, ok/FAIL, ms, detail.
#   deploy/dev/tail.sh                 # the newest run log in the pod
#   deploy/dev/tail.sh weekly-1551.log # a specific one (see /work/runs/)
set -euo pipefail
POD=$(kubectl -n kloudlite get pods -l app=dev -o jsonpath='{.items[0].metadata.name}')
LOG=${1:-$(kubectl -n kloudlite exec "$POD" -- bash -c 'ls -t /work/runs/*.log 2>/dev/null | head -1')}
[ -n "$LOG" ] || { echo "no run log in /work/runs yet" >&2; exit 1; }
case "$LOG" in /*) ;; *) LOG=/work/runs/$LOG ;; esac
echo "following $LOG"
kubectl -n kloudlite exec "$POD" -- tail -n +1 -f "$LOG" | python3 -u -c '
import sys, json
for line in sys.stdin:
    try: d = json.loads(line[line.index("{"):]) if "{" in line else None
    except Exception: continue
    if not d: continue
    m = d.get("message", ""); t = d.get("timestamp", "")[11:19]
    if m == "slo.step.done":
        print(t, "FAIL" if not d.get("ok") else " ok ", f"{d.get(\"ms\", 0):>7}ms", d.get("slo_id"), (d.get("detail") or "")[:140])
    elif m == "slo.step.skipped": print(t, "skip", "        ", d.get("slo_id"), (d.get("reason") or d.get("detail") or "")[:100])
    elif m in ("slo.stage.done", "slo.run.started", "slo.run.finished"): print(t, "----", m, d.get("stage") or d.get("run_id") or "", "failed=%s" % d.get("failed", "") if m == "slo.stage.done" else "")
'
