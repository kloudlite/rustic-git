#!/usr/bin/env bash
# Roll the AKS side. One apply: the ownership map's writer is elected by lease
# (`ownership::lease`), so there is no leader pod to roll first and no order between the
# StatefulSet and the Deployments — a srv pod that goes down mid-roll takes its lease with it,
# and a peer holds the writer inside LEADER_TTL plus one tick. The rollout waits say when it is done.
set -euo pipefail
cd "$(dirname "$0")"
# Never roll over a probe run. A pod restarting under an in-flight suite fails that suite's
# samples for the roll's reasons, not the fleet's — an agent DaemonSet roll under a push at 14:08
# on 2026-09-06 cost an hourly two ids that had nothing to do with what was being deployed. The
# probe cannot yield to a roll that starts after it did, so the roll yields to the probe: wait,
# up to the longest suite's deadline, for every SLO Job to be finished. The k3s agent roll is a
# hand step; run this script's wait (`deploy/roll.sh --wait-only`) before it for the same reason.
wait_for_probes() {
  local waited=0
  while :; do
    local active
    active=$(kubectl -n kloudlite get jobs -o json | python3 -c '
import sys, json
for j in json.load(sys.stdin)["items"]:
    n = j["metadata"]["name"]
    if ("slo-" in n) and (j.get("status", {}).get("active") or 0) > 0:
        print(n)')
    [ -z "$active" ] && return 0
    if [ "$waited" -ge 7200 ]; then
      echo "probe still running after 2 h: $active" >&2
      return 1
    fi
    [ "$waited" -eq 0 ] && echo "waiting for the probe to finish before rolling: $active"
    sleep 15; waited=$((waited + 15))
  done
}
wait_for_probes
[ "${1:-}" = "--wait-only" ] && exit 0
kubectl apply -f kloudlite.yaml -f kloudlite-web.yaml
kubectl -n kloudlite rollout status statefulset/kloudlite-srv --timeout=900s
for d in kloudlite-api kloudlite-worker kloudlite-web; do
  kubectl -n kloudlite rollout status "deployment/$d" --timeout=300s
done
echo "AKS rolled. The k3s side is separate: kubectl apply -f deploy/k3s/agent-daemonset.yaml -f deploy/k3s/gateway.yaml with that cluster's kubeconfig."
