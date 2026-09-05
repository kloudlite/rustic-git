#!/usr/bin/env bash
# Roll the AKS side. One apply: the ownership map's writer is elected by lease
# (`ownership::lease`), so there is no leader pod to roll first and no order between the
# StatefulSet and the Deployments — a srv pod that goes down mid-roll takes its lease with it,
# and a peer holds the writer inside LEADER_TTL plus one tick. The rollout waits say when it is done.
set -euo pipefail
cd "$(dirname "$0")"
kubectl apply -f kloudlite.yaml -f kloudlite-web.yaml
kubectl -n kloudlite rollout status statefulset/kloudlite-srv --timeout=900s
for d in kloudlite-api kloudlite-worker kloudlite-web; do
  kubectl -n kloudlite rollout status "deployment/$d" --timeout=300s
done
echo "AKS rolled. The k3s side is separate: kubectl apply -f deploy/k3s/agent-daemonset.yaml -f deploy/k3s/gateway.yaml with that cluster's kubeconfig."
