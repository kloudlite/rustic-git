#!/usr/bin/env bash
# Rotate a region's agent token: mint a new one at the api, install it in the region's agent
# Secret, restart the agents. Run from a laptop with both kubeconfigs.
#
# WHY a script: the api endpoint exists (`POST /v1/regions/{id}/rotate-token`) but a token that is
# rotated in one place and not the other takes every push in the region down; doing both halves in
# one command is what makes rotation something you actually do after a scare, not something you
# plan for a quiet week. The old token stops working the moment the api call lands, so the
# DaemonSet restart follows immediately.
#
#   ADMIN_JWT=<admin session token> ./rotate-agent-token.sh centralindia-k3s [k3s-kubeconfig]
set -euo pipefail
REGION="${1:?region id}"
K3S_KUBECONFIG="${2:-$(dirname "$0")/../../.local/k3s.yaml}"
API="${API:-https://dev.kloudlite.io}"
: "${ADMIN_JWT:?an admin session JWT}"

NEW=$(curl -fsS -X POST -H "Authorization: Bearer $ADMIN_JWT" "$API/v1/regions/$REGION/rotate-token" \
  | python3 -c 'import sys,json; print(json.load(sys.stdin)["agent_token"])')
[ -n "$NEW" ] || { echo "no token in the response" >&2; exit 1; }

KUBECONFIG="$K3S_KUBECONFIG" kubectl -n kube-system patch secret rustic-git-agent --type merge \
  -p "{\"data\":{\"WS_AGENT_TOKEN\":\"$(printf %s "$NEW" | base64 | tr -d '\n')\"}}" >/dev/null
KUBECONFIG="$K3S_KUBECONFIG" kubectl -n kube-system rollout restart ds/rustic-git-agent >/dev/null
KUBECONFIG="$K3S_KUBECONFIG" kubectl -n kube-system rollout status ds/rustic-git-agent --timeout=300s
echo "rotated $REGION; the previous token is dead"
