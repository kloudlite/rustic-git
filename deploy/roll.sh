#!/usr/bin/env bash
# Roll the AKS side in the only order that is safe: the leader, wait until it is Ready again,
# THEN the servers (and the api, worker and web, which can go any time).
#
# One `kubectl apply` of both StatefulSets rolls the leader and starts the srv roll in the same
# second; the leader's ~30s restart then overlaps the first srv ordinal's re-claim, which is the
# window in which claims fail (`421`s in srv logs). Splitting the leader into its own file made
# the order expressible; this is the order.
set -euo pipefail
cd "$(dirname "$0")"
kubectl apply -f rustic-git-leader.yaml
kubectl -n rustic-git rollout status statefulset/rustic-git-leader --timeout=300s
kubectl apply -f rustic-git.yaml -f rustic-git-web.yaml
kubectl -n rustic-git rollout status statefulset/rustic-git-srv --timeout=900s
for d in rustic-git-api rustic-git-worker rustic-git-web; do
  kubectl -n rustic-git rollout status "deployment/$d" --timeout=300s
done
echo "AKS rolled. The k3s side is separate: kubectl apply -f deploy/k3s/agent-daemonset.yaml -f deploy/k3s/gateway.yaml with that cluster's kubeconfig."
