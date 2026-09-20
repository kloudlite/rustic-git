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
    labels = j["metadata"].get("labels", {})
    owners = j["metadata"].get("ownerReferences", [])
    probe = labels.get("kloudlite.io/probe") == "true" or any(o.get("kind") == "CronJob" and o.get("name", "").startswith("kloudlite-slo-") for o in owners) or n.startswith(("slo-", "fast-", "hourly-", "weekly-", "monthly-"))
    status = j.get("status", {})
    pending = not status.get("completionTime") and not status.get("failed") and not status.get("succeeded")
    if probe and ((status.get("active") or 0) > 0 or pending):
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
ROLL_RUN="roll-$(date -u +%s)-$RANDOM"
POD_UID=$(kubectl -n kloudlite get pod "${HOSTNAME:-}" -o jsonpath='{.metadata.uid}' 2>/dev/null || true)
HOLDER="${POD_UID:-operator}/$ROLL_RUN"
# R-1: this script has no pod identity of its own to check against, so a takeover is judged
# entirely from what the EXISTING lock names — never age. Mirrors `coordination::take_over` in
# `bins/slo/src/coordination.rs`: a job-owned (hourly probe) lock is gone/terminal when its
# `job_uid` names no Job with a True `Complete`/`Failed` condition, or names no Job at all; a
# plain lock (another `roll.sh`, or a fast run) is gone when its `owner_pod_uid` (or, for a lock
# this old script itself wrote before that field existed, the pod uid `holder` starts with) names
# no live Pod.
holder_is_live() {
  local existing_json="$1"
  local job_uid owner_pod_uid holder
  job_uid=$(jq -r '.data.job_uid // empty' <<<"$existing_json")
  owner_pod_uid=$(jq -r '.data.owner_pod_uid // empty' <<<"$existing_json")
  holder=$(jq -r '.data.holder // empty' <<<"$existing_json")
  if [ -n "$job_uid" ]; then
    local job
    job=$(kubectl -n kloudlite get jobs -o json | jq -c --arg uid "$job_uid" '.items[] | select(.metadata.uid == $uid)')
    [ -z "$job" ] && return 1
    jq -e '(.status.conditions // []) | any(.type == "Complete" or .type == "Failed"; .status == "True")' <<<"$job" >/dev/null && return 1
    return 0
  fi
  local pod_uid="${owner_pod_uid:-${holder%%/*}}"
  [ -z "$pod_uid" ] && return 1
  kubectl -n kloudlite get pods -o json \
    | jq -e --arg uid "$pod_uid" '.items[] | select(.metadata.uid == $uid and (.status.phase != "Succeeded" and .status.phase != "Failed"))' >/dev/null
}
LOCK_JSON=$(kubectl -n kloudlite create configmap kloudlite-roll-coordination \
  --from-literal="holder=$HOLDER" -o json 2>/dev/null) || {
  EXISTING_JSON=$(kubectl -n kloudlite get configmap kloudlite-roll-coordination -o json) || {
    echo "roll coordination is held or unavailable" >&2
    exit 3
  }
  if holder_is_live "$EXISTING_JSON"; then
    echo "roll coordination is held by $(jq -r '.data.holder // "an unknown holder"' <<<"$EXISTING_JSON")" >&2
    exit 3
  fi
  EXISTING_UID=$(jq -r '.metadata.uid' <<<"$EXISTING_JSON")
  EXISTING_RV=$(jq -r '.metadata.resourceVersion' <<<"$EXISTING_JSON")
  # Replace (PUT), CAS'd on the resourceVersion just read — the same reason the Rust side uses
  # `replace` rather than delete-then-create: a delete-then-create window is exactly where a
  # second, racing takeover would land, and a PUT's precondition catches that race instead.
  REPLACE_BODY=$(jq -n --arg uid "$EXISTING_UID" --arg rv "$EXISTING_RV" --arg holder "$HOLDER" \
    '{apiVersion:"v1",kind:"ConfigMap",metadata:{name:"kloudlite-roll-coordination",namespace:"kloudlite",uid:$uid,resourceVersion:$rv},data:{holder:$holder}}')
  LOCK_JSON=$(kubectl -n kloudlite replace -f - -o json <<<"$REPLACE_BODY") || {
    echo "roll coordination takeover was refused (a racing takeover likely won)" >&2
    exit 3
  }
}
LOCK_UID=$(jq -r '.metadata.uid' <<<"$LOCK_JSON")
LOCK_RV=$(jq -r '.metadata.resourceVersion' <<<"$LOCK_JSON")
LOCK_DELETE_OPTIONS=$(mktemp)
cat >"$LOCK_DELETE_OPTIONS" <<EOF
{"apiVersion":"v1","kind":"DeleteOptions","preconditions":{"uid":"$LOCK_UID","resourceVersion":"$LOCK_RV"}}
EOF
release_roll_lock() {
  CURRENT_UID=$(kubectl -n kloudlite get configmap kloudlite-roll-coordination -o jsonpath='{.metadata.uid}') || {
    echo "could not verify roll coordination ownership; lock remains held" >&2
    return 1
  }
  [ "$CURRENT_UID" = "$LOCK_UID" ] || return 0
  kubectl delete --raw '/api/v1/namespaces/kloudlite/configmaps/kloudlite-roll-coordination' \
    -f "$LOCK_DELETE_OPTIONS" >/dev/null || {
    echo "roll coordination release was refused; lock remains held" >&2
    return 1
  }
}
trap 'command_status=$?; release_status=0; release_roll_lock || release_status=$?; rm -f "$LOCK_DELETE_OPTIONS"; [ "$command_status" -eq 0 ] || exit "$command_status"; exit "$release_status"' EXIT
# A schedule suspended by hand stays suspended across the roll. The manifest says `suspend: false`
# for the fast and hourly probes, so a plain apply would switch them back on — and a CronJob that
# missed a tick fires the moment it is unsuspended, i.e. straight into the rollout, which is a
# failed sample that measured this script rather than the platform.
# Set IN the manifest before the apply, not patched back after it: the CronJob controller fires a
# missed tick within the second between the two, straight into the rollout (seen 18:41 on
# 2026-09-06 with the patch-after version).
suspended=$(kubectl -n kloudlite get cronjobs -o jsonpath='{range .items[?(@.spec.suspend==true)]}{.metadata.name}{" "}{end}')
# `create --dry-run=client`, never `apply --dry-run=client`: the latter prints the object AFTER
# merging with what is live, so piping it back applied the running state and moved nothing
# (2026-09-06, a roll that "succeeded" with the old image everywhere).
kubectl create --dry-run=client -o json -f kloudlite.yaml -f kloudlite-web.yaml \
  | jq -s --arg keep "$suspended" '{apiVersion: "v1", kind: "List", items: map(if .kind=="CronJob" and (.metadata.name as $n | $keep | split(" ") | index($n)) then .spec.suspend = true else . end) | walk(if type == "object" then with_entries(select(.value != null)) else . end)}' \
  | kubectl apply -f -
for c in $suspended; do echo "kept $c suspended"; done
kubectl -n kloudlite rollout status statefulset/kloudlite-srv --timeout=900s
for d in kloudlite-api kloudlite-worker kloudlite-admin kloudlite-web; do
  kubectl -n kloudlite rollout status "deployment/$d" --timeout=300s
done
echo "AKS rolled. The k3s side is separate: kubectl apply -f deploy/k3s/agent-daemonset.yaml -f deploy/k3s/gateway.yaml with that cluster's kubeconfig."
