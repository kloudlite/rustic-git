#!/usr/bin/env bash
# Roll the AKS side. One apply: the ownership map's writer is elected by lease
# (`ownership::lease`), so there is no leader pod to roll first and no order between the
# StatefulSet and the Deployments — a srv pod that goes down mid-roll takes its lease with it,
# and a peer holds the writer inside LEADER_TTL plus one tick. The rollout waits say when it is done.
set -euo pipefail
cd "$(dirname "$0")"
[ "${1:-}" = "--wait-only" ] && WAIT_ONLY=1 || WAIT_ONLY=0
ROLL_RUN="roll-$(date -u +%s)-$RANDOM"
POD_UID=$(kubectl -n kloudlite get pod "${HOSTNAME:-}" -o jsonpath='{.metadata.uid}' 2>/dev/null || true)
HOLDER="${POD_UID:-operator}/$ROLL_RUN"
# R-1 / ruling 3: this script has no pod identity of its own to check a PROBE holder against, so a
# takeover is judged entirely from what the EXISTING lock names — never age (mirrors
# `coordination::holder_is_live` in `bins/slo/src/coordination.rs`) — with one exception: a roll
# lock with no `owner_pod_uid` (an operator's own earlier `roll.sh`, run from a laptop with no pod
# to ask about) is judged by age against the same 2 h bound `coordination::OPERATOR_ROLL_MAX_AGE`
# uses, because there is no API object to check instead.
holder_is_live() {
  local existing_json="$1"
  local job_uid owner_pod_uid holder kind created
  job_uid=$(jq -r '.data.job_uid // empty' <<<"$existing_json")
  owner_pod_uid=$(jq -r '.data.owner_pod_uid // empty' <<<"$existing_json")
  holder=$(jq -r '.data.holder // empty' <<<"$existing_json")
  kind=$(jq -r '.data.kind // empty' <<<"$existing_json")
  if [ -n "$job_uid" ]; then
    local job
    job=$(kubectl -n kloudlite get jobs -o json | jq -c --arg uid "$job_uid" '.items[] | select(.metadata.uid == $uid)')
    [ -z "$job" ] && return 1
    jq -e '(.status.conditions // []) | any(.type == "Complete" or .type == "Failed"; .status == "True")' <<<"$job" >/dev/null && return 1
    return 0
  fi
  local pod_uid="${owner_pod_uid:-${holder%%/*}}"
  if [ -n "$pod_uid" ] && [ "$pod_uid" != "operator" ]; then
    kubectl -n kloudlite get pods -o json \
      | jq -e --arg uid "$pod_uid" '.items[] | select(.metadata.uid == $uid and (.status.phase != "Succeeded" and .status.phase != "Failed"))' >/dev/null
    return $?
  fi
  # No resolvable pod: only sound for a ROLL lock (a probe lock always has an `owner_pod_uid` — it
  # runs as a Job's pod, never by hand), and only for the 2 h bound `roll.sh` itself never exceeds.
  [ "$kind" = "roll" ] || return 1
  created=$(jq -r '.metadata.creationTimestamp // empty' <<<"$existing_json")
  [ -z "$created" ] && return 0
  local created_epoch now_epoch age
  # Fail SAFE, not fail dead: if neither `date` form can parse the timestamp, `created_epoch` is
  # empty rather than 0 — 0 would compute an age of ~1.8 billion seconds and take over a live
  # operator roll lock. Uncertainty here must read as live, the same rule
  # `coordination::roll_holder_is_live` applies to a missing `creationTimestamp` in Rust.
  created_epoch=$(date -u -d "$created" +%s 2>/dev/null || date -u -j -f "%Y-%m-%dT%H:%M:%SZ" "$created" +%s 2>/dev/null || echo "")
  [ -z "$created_epoch" ] && return 0
  now_epoch=$(date -u +%s)
  age=$((now_epoch - created_epoch))
  [ "$age" -lt 7200 ]
}
is_kind() {
  jq -e --arg k "$2" '.data.kind == $k' <<<"$1" >/dev/null
}
# Lock FIRST, wait for running probes only after: waiting first left a gap in which a probe could
# start between the wait finishing and the lock being taken. A live PROBE holder is not a refusal
# here — the roll waits for it, inside the same two-hour bound the old `wait_for_probes` used; only
# a live ROLL holder (another roll already in progress) is a hard `exit 3`.
LOCK_WAITED=0
while :; do
  LOCK_JSON=$(kubectl -n kloudlite create configmap kloudlite-roll-coordination \
    --from-literal="holder=$HOLDER" --from-literal="kind=roll" -o json 2>/dev/null) && break
  EXISTING_JSON=$(kubectl -n kloudlite get configmap kloudlite-roll-coordination -o json) || {
    echo "roll coordination is held or unavailable" >&2
    exit 3
  }
  if holder_is_live "$EXISTING_JSON"; then
    EXISTING_HOLDER=$(jq -r '.data.holder // "an unknown holder"' <<<"$EXISTING_JSON")
    if is_kind "$EXISTING_JSON" "roll"; then
      echo "roll coordination is held by $EXISTING_HOLDER" >&2
      exit 3
    fi
    if [ "$LOCK_WAITED" -ge 7200 ]; then
      echo "probe still running after 2 h: $EXISTING_HOLDER" >&2
      exit 3
    fi
    [ "$LOCK_WAITED" -eq 0 ] && echo "waiting for the probe to finish before rolling: $EXISTING_HOLDER"
    sleep 15; LOCK_WAITED=$((LOCK_WAITED + 15))
    continue
  fi
  EXISTING_UID=$(jq -r '.metadata.uid' <<<"$EXISTING_JSON")
  EXISTING_RV=$(jq -r '.metadata.resourceVersion' <<<"$EXISTING_JSON")
  DELETE_OPTIONS=$(mktemp)
  printf '{"apiVersion":"v1","kind":"DeleteOptions","preconditions":{"uid":"%s","resourceVersion":"%s"}}' \
    "$EXISTING_UID" "$EXISTING_RV" >"$DELETE_OPTIONS"
  # Delete preconditioned on uid+resourceVersion, then the ordinary create — never a PUT (the
  # Role has no `update`, and needs none: the delete's precondition is what stops two racing
  # takeovers both winning). A refused delete (someone else's takeover already landed) falls
  # through to retrying the loop, which re-reads and re-judges the new holder.
  kubectl delete --raw '/api/v1/namespaces/kloudlite/configmaps/kloudlite-roll-coordination' \
    -f "$DELETE_OPTIONS" >/dev/null 2>&1 || true
  rm -f "$DELETE_OPTIONS"
done
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
# `--wait-only` is its own bounded step (a human runs it right before the separate, manual k3s
# agent roll) — it still takes the lock first and waits for probes inside the loop above, closing
# the same start-in-the-gap window for ITS OWN duration, but releases before returning rather than
# holding the lock across the human's next, unbounded step.
[ "$WAIT_ONLY" -eq 1 ] && exit 0
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
