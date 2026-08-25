#!/usr/bin/env bash
# End-to-end workspaces/environments test: a real btrfs pool, a real rustic-git (server tier),
# rustic-git-api, rustic-git-agent, a real Cosmos DB, real Azure blob storage, a real k3s cluster.
#
# Mirrors tests/registry_e2e.sh's conventions: exit 77 when a prerequisite is absent (root-capable
# btrfs, a reachable cluster with the CRDs installed, Cosmos/Azure credentials) rather than failing
# mid-script; one trap tears everything down. This needs a Linux box with btrfs + root AND a k3s
# node (the CLAUDE.md-documented `wssnap-bench` VM) — it cannot run on a Mac laptop, which is why
# this script was authored without ever being run locally: read it carefully before trusting a
# change to it. A single-node k3s carrying both role labels is enough; nothing here needs two nodes.
#
# Three binaries now, not two: the volume registry (commits/history/ref) moved onto the server tier
# (rustic-git serve — see bins/server/src/vol_agent.rs), and rustic-git-api reaches it as a client
# (RUSTIC_GIT_VOL_AGENT_URL/_TOKEN) to serve GET /v1/volumes/*. rustic-git-api still owns
# /v1/workspaces|environments|regions (Cosmos-backed) — that split is unchanged. The agent is a
# CONTROLLER now, not a poller: it watches the CRDs, so this script waits on the conditions those
# controllers write (`kubectl wait --for=condition=Ready`) rather than polling document state.
#
# Namespaces (crd.rs): all of an owner's workspace pods share `ws-{owner}`; an environment gets its
# own `env-{id}`. Live volumes are local PVs claimed as `live-{volume-id}` through the
# `rustic-git-local` StorageClass, and every namespace enforces Pod Security Admission `baseline`.
#
# What it does NOT cover, on purpose (ponytail: exercise the plumbing end to end, not every knob):
#   - the fancier "clone while a writer is mutating the source, stop hook pauses it" path is
#     already covered by crates/workspaces/tests/engine_ops.rs; this script clones an idle
#     workspace, which is enough to prove the HTTP round trip end to end.
#
# Exit status: 0 = every step passed. 77 = a prerequisite was missing and nothing was started.
# Anything else = a real failure.
set -euo pipefail

log() { echo "==> $*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }

# Count records in a JSON array body. `grep` exits 1 when it matches nothing, and under
# `set -o pipefail` that ends the script — so an EMPTY history (a perfectly ordinary state before
# the first push) looked exactly like a crash, silently, with no message. Never count with a bare
# grep pipeline in this file.
id_count() { printf '%s' "$1" | grep -c '"id"' || true; }

# ---------------------------------------------------------------------------
# Prerequisites
# ---------------------------------------------------------------------------
command -v btrfs >/dev/null 2>&1 && command -v mkfs.btrfs >/dev/null 2>&1 || {
  echo "SKIP: btrfs/mkfs.btrfs not on PATH" >&2
  exit 77
}
sudo -n true 2>/dev/null || {
  echo "SKIP: passwordless sudo not available (btrfs subvolume/mount need root)" >&2
  exit 77
}
kubectl version --request-timeout=5s >/dev/null 2>&1 || {
  echo "SKIP: no reachable kubernetes cluster" >&2
  exit 77
}
kubectl get crd volumes.rustic-git.io >/dev/null 2>&1 || {
  echo "SKIP: rustic-git CRDs not installed (deploy/k3s/crds.yaml)" >&2
  exit 77
}
[ -n "${COSMOS_ENDPOINT:-}" ] && [ -n "${COSMOS_KEY:-}" ] || { echo "SKIP: COSMOS_ENDPOINT/COSMOS_KEY not set" >&2; exit 77; }
[ -n "${AZURE_ACCOUNT:-}" ] && [ -n "${AZURE_KEY:-}" ] && [ -n "${AZURE_CONTAINER:-}" ] || {
  echo "SKIP: AZURE_ACCOUNT/AZURE_KEY/AZURE_CONTAINER not set" >&2
  exit 77
}

SERVER_BIN="${WS_E2E_SERVER_BIN:-target/debug/rustic-git}"
API_BIN="${WS_E2E_API_BIN:-target/debug/rustic-git-api}"
AGENT_BIN="${WS_E2E_AGENT_BIN:-target/debug/rustic-git-agent}"
if [ ! -x "$SERVER_BIN" ] || [ ! -x "$API_BIN" ] || [ ! -x "$AGENT_BIN" ]; then
  log "building rustic-git/rustic-git-api/rustic-git-agent (not found at $SERVER_BIN / $API_BIN / $AGENT_BIN)"
  cargo build -q --bin rustic-git --bin rustic-git-api --bin rustic-git-agent
fi

# ---------------------------------------------------------------------------
# State torn down by the trap below
# ---------------------------------------------------------------------------
SERVER_PID=""
API_PID=""
AGENT_PID=""
MOUNT=""
IMG=""
TMPD=""
COSMOS_DB="wse2e-$RANDOM$RANDOM"
ENV_ID=""
WS_ID=""
WS_NS=""
PROBE_NS=""
CLONE1_ID=""
CLONE_ID=""
RESTORE_ID=""

cleanup() {
  set +e
  # The CRDs are cluster-scoped and OWN everything namespaced they produced (namespace, PV/PVC,
  # pod, deployments, services, policies), so deleting the four objects is the whole teardown —
  # garbage collection does the rest. The probe namespace is ours, not the controller's.
  [ -n "$ENV_ID" ] && kubectl delete environment "$ENV_ID" --ignore-not-found --wait=false >/dev/null 2>&1
  for id in "$WS_ID" "$CLONE1_ID" "$CLONE_ID" "$RESTORE_ID"; do
    [ -n "$id" ] && kubectl delete workspace "$id" --ignore-not-found --wait=false >/dev/null 2>&1
  done
  [ -n "$PROBE_NS" ] && kubectl delete namespace "$PROBE_NS" --ignore-not-found --wait=false >/dev/null 2>&1
  [ -n "$AGENT_PID" ] && kill "$AGENT_PID" >/dev/null 2>&1
  [ -n "$API_PID" ] && kill "$API_PID" >/dev/null 2>&1
  [ -n "$SERVER_PID" ] && kill "$SERVER_PID" >/dev/null 2>&1
  [ -n "$AGENT_PID" ] && wait "$AGENT_PID" 2>/dev/null
  [ -n "$API_PID" ] && wait "$API_PID" 2>/dev/null
  [ -n "$SERVER_PID" ] && wait "$SERVER_PID" 2>/dev/null
  [ -n "$MOUNT" ] && sudo umount "$MOUNT" >/dev/null 2>&1
  # Cosmos has no admin-delete route in this API (see api.rs — only create/list regions exist),
  # so cleanup of the throwaway per-run database needs the az CLI; best-effort, and not assumed
  # to be installed on every runner.
  if command -v az >/dev/null 2>&1; then
    az cosmosdb sql database delete --account-name "${COSMOS_ACCOUNT:-}" --resource-group "${COSMOS_RESOURCE_GROUP:-}" \
      --name "$COSMOS_DB" --yes >/dev/null 2>&1
  else
    echo "NOTE: az CLI not found — Cosmos test db '$COSMOS_DB' was not deleted, clean it up by hand" >&2
  fi
  # Azure blobs live under layers/{uuid}.zst with no per-run prefix (the
  # engine keys them by digest, not by run), so a run's blobs cannot be scoped and deleted here —
  # they are left behind as orphans. That is acceptable: they are immutable by design (the whole
  # point of content addressing) and harmless to leave, exactly like any other blob GC would sweep
  # eventually. Not attempting to delete them is deliberate, not an oversight.
  [ -n "$TMPD" ] && rm -rf "$TMPD"
  echo "cleanup done (Azure blobs from this run, if any, were left in place — see script comments)"
}
trap cleanup EXIT

# ---------------------------------------------------------------------------
# A loopback btrfs pool: 2G sparse image, mkfs.btrfs, mounted under a mktemp dir. Same shape as
# crates/workspaces/tests/engine_ops.rs's LoopbackPool fixture, in shell.
# ---------------------------------------------------------------------------
TMPD=$(mktemp -d)
IMG="$TMPD/pool.img"
MOUNT="$TMPD/mnt"
mkdir -p "$MOUNT"
log "creating 2G loopback btrfs pool at $MOUNT"
truncate -s 2G "$IMG"
sudo mkfs.btrfs -q "$IMG"
sudo mount -o loop "$IMG" "$MOUNT"
sudo chmod 0777 "$MOUNT"

# ---------------------------------------------------------------------------
# Shared secrets/addresses
# ---------------------------------------------------------------------------
JWT_SECRET="e2e-jwt-secret-$(head -c24 /dev/urandom | od -An -tx1 | tr -d ' \n')"
PEER_SECRET="e2e-peer-secret-$RANDOM"
VOL_AGENT_TOKEN="e2e-vol-agent-token-$RANDOM$RANDOM"
SERVER_HTTP_ADDR="127.0.0.1:8180"
SERVER_PEER_ADDR="127.0.0.1:8181"
# peer_stream binds PEER_ADDR+1 (8182), so ssh moves clear of it
SERVER_SSH_ADDR="127.0.0.1:8183"
API_ADDR="127.0.0.1:8190"
ADMIN_EMAIL="ws-e2e-admin@example.test"
USER_EMAIL="ws-e2e-user@example.test"
SERVER_BASE="http://$SERVER_HTTP_ADDR"
BASE="http://$API_ADDR"

# curl prints 000 on a refused connection everywhere below — 000 means "not up yet", never
# "answered". Using `curl -f` here would also be wrong: an unauthenticated probe answering 401 (or
# any HTTP status at all) means the listener is up.
wait_for_listener() {
  local url="$1" name="$2"
  for i in $(seq 1 60); do
    local code
    code=$(curl -sS -o /dev/null -w '%{http_code}' "$url" 2>/dev/null || echo "")
    [ -n "$code" ] && [ "$code" != "000" ] && return 0
    sleep 1
    [ "$i" -eq 60 ] && fail "$name never came up on $url"
  done
}

# ---------------------------------------------------------------------------
# Start the server tier: this is what now hosts the agent work surface
# (/vol-agent/{owner}/{name}/{commits,history,ref}) — RUSTIC_GIT_VOL_AGENT_TOKENS is the
# break-glass shared secret rustic-git-api's RegistryClient presents to read history/refs for
# GET /v1/volumes/*. Solo mode (no RUSTIC_GIT_PEER_SVC/RUSTIC_GIT_LEADER): a single node needs no
# ownership map. mem:// S3 because this script never touches the git/registry side of this
# process, only its workspaces surface.
# ---------------------------------------------------------------------------
log "starting rustic-git serve on $SERVER_HTTP_ADDR (Cosmos db $COSMOS_DB)"
RUSTIC_GIT_S3_URL="mem://" \
RUSTIC_GIT_JWT_SECRET="$JWT_SECRET" \
RUSTIC_GIT_HTTP_ADDR="$SERVER_HTTP_ADDR" \
RUSTIC_GIT_PEER_ADDR="$SERVER_PEER_ADDR" \
RUSTIC_GIT_SSH_ADDR="$SERVER_SSH_ADDR" \
RUSTIC_GIT_HOST_KEY="$TMPD/host_key" \
RUSTIC_GIT_VOL_AGENT_TOKENS="$VOL_AGENT_TOKEN" \
COSMOS_ENDPOINT="$COSMOS_ENDPOINT" \
COSMOS_KEY="$COSMOS_KEY" \
COSMOS_DB="$COSMOS_DB" \
"$SERVER_BIN" serve &
SERVER_PID=$!

log "waiting for the server to answer"
wait_for_listener "$SERVER_BASE/healthz" "rustic-git serve"

# ---------------------------------------------------------------------------
# Start the api: the user-facing /v1/workspaces|environments|regions|volumes surface — the one writer of the CRDs the
# controllers reconcile. Reaches the
# server tier as a RegistryClient (RUSTIC_GIT_VOL_AGENT_URL/_TOKEN) purely to serve
# GET /v1/volumes/*; workspace/environment/region CRUD stays direct-to-Cosmos, same db as the
# server above.
# ---------------------------------------------------------------------------
log "starting rustic-git-api on $API_ADDR (Cosmos db $COSMOS_DB)"
RUSTIC_GIT_S3_URL="mem://" \
RUSTIC_GIT_JWT_SECRET="$JWT_SECRET" \
RUSTIC_GIT_PEER_SECRET="$PEER_SECRET" \
RUSTIC_GIT_API_ADDR="$API_ADDR" \
RUSTIC_GIT_WORKSPACES_ADMINS="$ADMIN_EMAIL" \
RUSTIC_GIT_VOL_AGENT_URL="$SERVER_BASE" \
RUSTIC_GIT_VOL_AGENT_TOKEN="$VOL_AGENT_TOKEN" \
COSMOS_ENDPOINT="$COSMOS_ENDPOINT" \
COSMOS_KEY="$COSMOS_KEY" \
COSMOS_DB="$COSMOS_DB" \
"$API_BIN" &
API_PID=$!

log "waiting for the api to answer"
wait_for_listener "$BASE/v1/regions" "rustic-git-api"

# ---------------------------------------------------------------------------
# Mint HS256 session JWTs by hand: there is no CLI in this repo that mints one (checked
# bins/server's `admin` subcommands — those are registry tokens, a different mechanism), and this
# script owns the signing secret it just started the api with, so it can sign the same tokens the
# api's own crates/core/src/jwt.rs `Jwt::mint` would produce (same header, same Claims shape).
# ---------------------------------------------------------------------------
b64url() { openssl base64 -A | tr '+/' '-_' | tr -d '='; }

mint_jwt() {
  # Workspace/volume ownership keys on the USERNAME claim (vol/{owner}/... paths validate it
  # as an owner name; an email's @/. can never route), so every minted token carries one.
  local email="$1" name="$2" username="$3"
  local now exp header payload signing_input sig
  now=$(date +%s)
  exp=$((now + 43200))
  header=$(printf '{"typ":"JWT","alg":"HS256"}' | b64url)
  payload=$(printf '{"sub":"%s","name":"%s","username":"%s","typ":"session","iat":%d,"exp":%d}' "$email" "$name" "$username" "$now" "$exp" | b64url)
  signing_input="$header.$payload"
  sig=$(printf '%s' "$signing_input" | openssl dgst -sha256 -hmac "$JWT_SECRET" -binary | b64url)
  echo "$signing_input.$sig"
}

# The username is also the workspace NAMESPACE's name (crd.rs's `ws_namespace` lowercases it), so
# every kubectl below is scoped by this one value rather than by workspace id.
USER_NAME="e2euser"
ADMIN_TOKEN=$(mint_jwt "$ADMIN_EMAIL" "E2E Admin" "e2eadmin")
USER_TOKEN=$(mint_jwt "$USER_EMAIL" "E2E User" "$USER_NAME")

log "verifying the minted admin token is accepted"
curl -fsS "$BASE/v1/regions" -H "Authorization: Bearer $ADMIN_TOKEN" >/dev/null || fail "minted admin JWT was rejected"

# ---------------------------------------------------------------------------
# Register a region, start the agent
# ---------------------------------------------------------------------------
REGION_ID="e2e-$RANDOM"
log "registering region $REGION_ID"
REGION_JSON=$(curl -fsS -X POST "$BASE/v1/regions" -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H 'Content-Type: application/json' \
  -d "{\"id\":\"$REGION_ID\",\"name\":\"E2E Region\",\"storage_account\":\"$AZURE_ACCOUNT\",\"blob_container\":\"$AZURE_CONTAINER\"}")
AGENT_TOKEN=$(echo "$REGION_JSON" | sed -n 's/.*"agent_token":"\([^"]*\)".*/\1/p')
[ -n "$AGENT_TOKEN" ] || fail "no agent_token in region create response: $REGION_JSON"

# The controller shards on `spec.nodeName`, so it needs to know which node it IS. This must be the
# node's KUBERNETES name (what `kubectl get nodes` prints), which is the hostname on a default k3s
# install — override with WS_E2E_NODE where it is not.
#
# NB this script runs its own agent against its own loopback pool, so the DaemonSet controller must
# not also be watching this node: two controllers reconciling one Volume would materialize it into
# two different pools and fight over its status. Take the pool label off this node first:
#   kubectl label node "$(hostname)" rustic-git.io/pool-
E2E_NODE="${WS_E2E_NODE:-$(hostname)}"
kubectl get node "$E2E_NODE" >/dev/null 2>&1 || fail "node $E2E_NODE is not in the cluster (set WS_E2E_NODE)"
if kubectl get pods -n kube-system -l app=rustic-git-agent \
     --field-selector "spec.nodeName=$E2E_NODE" --no-headers 2>/dev/null | grep -q .; then
  fail "the rustic-git-agent DaemonSet is running on $E2E_NODE; remove the rustic-git.io/pool label first"
fi

log "starting rustic-git-agent against pool $MOUNT as node $E2E_NODE (registry at $SERVER_BASE)"
NODE_NAME="$E2E_NODE" \
WS_REGISTRY_URL="$SERVER_BASE" \
WS_REGION="$REGION_ID" \
WS_AGENT_TOKEN="$AGENT_TOKEN" \
WS_POOL="$MOUNT" \
HOSTNAME="ws-e2e-agent" \
COSMOS_ENDPOINT="$COSMOS_ENDPOINT" \
COSMOS_KEY="$COSMOS_KEY" \
COSMOS_DB="$COSMOS_DB" \
AZURE_ACCOUNT="$AZURE_ACCOUNT" \
AZURE_KEY="$AZURE_KEY" \
AZURE_CONTAINER="$AZURE_CONTAINER" \
sudo -E "$AGENT_BIN" &
AGENT_PID=$!
sleep 3
kill -0 "$AGENT_PID" 2>/dev/null || fail "agent exited immediately"

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------
field() { sed -n "s/.*\"$1\":\"\{0,1\}\([^,}\"]*\)\"\{0,1\}.*/\1/p"; }

# Waiting is the controller's own contract, not a state string this script re-derives: `Ready`
# means ready, and a `kubectl wait` that loses fails with the condition's message instead of a bare
# timeout. The CRDs are cluster-scoped, so none of these take a -n.
wait_ws_ready() {
  kubectl wait --for=condition=Ready "workspace/$1" --timeout=300s \
    || fail "workspace $1 never became Ready"
}

wait_ws_gone() {
  kubectl wait --for=delete "workspace/$1" --timeout=300s \
    || fail "workspace $1 object still present after delete"
}

wait_env_ready() {
  kubectl wait --for=condition=Ready "environment/$1" --timeout=300s \
    || fail "environment $1 never became Ready"
}

# Stopped is asserted on `status.phase`, NOT on Ready.
#
# `Ready` on these objects means "the controller converged this object to its spec", so an
# environment that was asked to stop and HAS stopped is Ready=True with reason `Stopped` — the
# reconcile finished and there is nothing outstanding. Waiting for Ready=false here would wait
# forever for a state that correctly never happens. `phase` is the field that distinguishes running
# from stopped, and it is what a human reads in `kubectl get environments` too.
wait_env_stopped() {
  kubectl wait --for=jsonpath='{.status.phase}'=stopped "environment/$1" --timeout=300s \
    || fail "environment $1 never reached phase=stopped"
}

live_dir() { echo "$MOUNT/vol/$1/live"; }

# ---------------------------------------------------------------------------
# Create workspace, wait ready, write into live
# ---------------------------------------------------------------------------
log "creating workspace"
WS_JSON=$(curl -fsS -X POST "$BASE/v1/workspaces" -H "Authorization: Bearer $USER_TOKEN" \
  -H 'Content-Type: application/json' -d '{"name":"e2e-ws","region":"'"$REGION_ID"'","quota_gb":5}')
WS_ID=$(echo "$WS_JSON" | field id)
[ -n "$WS_ID" ] || fail "no id in workspace create response: $WS_JSON"
wait_ws_ready "$WS_ID"
WS_NS="ws-$(echo "$USER_NAME" | tr '[:upper:]' '[:lower:]')"

log "checking the workspace pod is running and bound to its live-$WS_ID claim"
kubectl -n "$WS_NS" wait --for=condition=Ready "pod/$WS_ID" --timeout=120s \
  || fail "no ready pod $WS_ID in $WS_NS after the workspace reached Ready"
kubectl -n "$WS_NS" get "pvc/live-$WS_ID" -o jsonpath='{.status.phase}' | grep -q Bound \
  || fail "workspace claim live-$WS_ID is not Bound (StorageClass rustic-git-local)"

log "writing a file into the live subvolume"
sudo bash -c "printf 'hello from ws_e2e' > '$(live_dir "$WS_ID")/hello.txt'"
[ -f "$(live_dir "$WS_ID")/hello.txt" ] || fail "write into live did not land"

# ---------------------------------------------------------------------------
# Push: the one mutating verb — snapshot + upload the layer, POST its CommitRecord, move the
# registry ref, all in one call. This is the only step that reaches the server tier's
# /vol-agent/{owner}/{name}/commits — history must grow by exactly one after it.
# ---------------------------------------------------------------------------
log "pushing workspace"
# Workspace CREATION already pushed one record (init = create + push of the empty subvolume),
# so the baseline captures that before asserting this push adds exactly one more.
BEFORE=$(id_count "$(curl -fsS "$BASE/v1/volumes/$WS_ID/history" -H "Authorization: Bearer $USER_TOKEN")")
curl -fsS -X POST "$BASE/v1/workspaces/$WS_ID/push" -H "Authorization: Bearer $USER_TOKEN" \
  -H 'Content-Type: application/json' -d '{"message":"first push"}' >/dev/null

log "checking the push landed in the volume registry with its message"
PUSHED="$BEFORE"
HISTORY=""
for i in $(seq 1 30); do
  HISTORY=$(curl -fsS "$BASE/v1/volumes/$WS_ID/history" -H "Authorization: Bearer $USER_TOKEN")
  PUSHED=$(id_count "$HISTORY")
  [ "$PUSHED" -gt "$BEFORE" ] && break
  sleep 2
done
[ "$PUSHED" -eq "$((BEFORE + 1))" ] || fail "volume history did not grow by exactly one after push ($BEFORE -> $PUSHED)"
echo "$HISTORY" | grep -q '"message":"first push"' || fail "pushed message missing from history: $HISTORY"
REFS=$(curl -fsS "$BASE/v1/volumes/$WS_ID/refs" -H "Authorization: Bearer $USER_TOKEN")
echo "$REFS" | grep -q '"main":"' || fail "volume refs has no main ref after push: $REFS"

# ---------------------------------------------------------------------------
# Clone (registry-history path): new workspace grafted onto the source's PUSHED history
# (crates/workspaces/src/engine/ops.rs's `clone_local` reads the source's history from the
# registry, not the source's live filesystem, when the source isn't materialized on the
# same pool — true here since this is a fresh clone). hello.txt was pushed above, so the
# clone MUST contain it: it always starts from the last thing the registry knows about, and
# that is now this exact commit. "fork" is not a route any more — clone is the one
# local-copy verb.
# ---------------------------------------------------------------------------
log "cloning workspace (registry-history path)"
CLONE1_JSON=$(curl -fsS -X POST "$BASE/v1/workspaces/$WS_ID/clone" -H "Authorization: Bearer $USER_TOKEN" \
  -H 'Content-Type: application/json' -d '{"name":"e2e-ws-clone1"}')
CLONE1_ID=$(echo "$CLONE1_JSON" | field id)
[ -n "$CLONE1_ID" ] || fail "no id in clone response: $CLONE1_JSON"
wait_ws_ready "$CLONE1_ID"
[ -f "$(live_dir "$CLONE1_ID")/hello.txt" ] || fail "clone is missing the pushed file (clone must materialize pushed history)"

# ---------------------------------------------------------------------------
# Delete the clone: proves the completed-delete half of the storage-hygiene work end to end — the
# object is deleted and the controller reclaims the clone's ENTIRE local volume dir
# (not just the live subvolume), never touching the source's registry history (blobs are shared,
# immutable, untouched by design).
# ---------------------------------------------------------------------------
log "deleting the cloned workspace"
curl -fsS -X DELETE "$BASE/v1/workspaces/$CLONE1_ID" -H "Authorization: Bearer $USER_TOKEN" >/dev/null
wait_ws_gone "$CLONE1_ID"

log "checking the clone's local volume directory was reclaimed"
CLONE1_VOLDIR="$MOUNT/vol/$CLONE1_ID"
for i in $(seq 1 30); do
  [ ! -d "$CLONE1_VOLDIR" ] && break
  sleep 1
  [ "$i" -eq 30 ] && fail "clone's volume dir $CLONE1_VOLDIR still present after delete"
done

# ---------------------------------------------------------------------------
# Clone (running-source path): the source's pod is still up, so the controller's clone arm
# picks `Engine::clone_running` this time (prefetch, then a short stop/push/start window)
# instead of the registry-history path used above — same pushed content either way, since the
# source has nothing unpushed left after the push above.
# ---------------------------------------------------------------------------
log "cloning workspace (running-source path)"
CLONE_JSON=$(curl -fsS -X POST "$BASE/v1/workspaces/$WS_ID/clone" -H "Authorization: Bearer $USER_TOKEN" \
  -H 'Content-Type: application/json' -d '{"name":"e2e-ws-clone"}')
CLONE_ID=$(echo "$CLONE_JSON" | field id)
[ -n "$CLONE_ID" ] || fail "no id in clone response: $CLONE_JSON"
wait_ws_ready "$CLONE_ID"
[ -f "$(live_dir "$CLONE_ID")/hello.txt" ] || fail "cloned workspace is missing the file written into the source"

# ---------------------------------------------------------------------------
# Restore: new workspace grafted onto an EXPLICIT past snapshot (the newest entry in the
# source's registry history), rather than the source's current tip — the same distinction
# `crates/workspaces/src/api.rs`'s `restore_ws` doc comment draws against `clone`.
# ---------------------------------------------------------------------------
log "restoring workspace from the newest snapshot in history"
RESTORE_SNAPSHOT_ID=$(curl -fsS "$BASE/v1/volumes/$WS_ID/history" -H "Authorization: Bearer $USER_TOKEN" \
  | grep -o '"id":"[^"]*"' | head -1 | cut -d'"' -f4)
[ -n "$RESTORE_SNAPSHOT_ID" ] || fail "no snapshot id found in $WS_ID history"
RESTORE_JSON=$(curl -fsS -X POST "$BASE/v1/workspaces/restore" -H "Authorization: Bearer $USER_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"name":"e2e-ws-restore","snapshot_id":"'"$RESTORE_SNAPSHOT_ID"'","src_workspace":"'"$WS_ID"'"}')
RESTORE_ID=$(echo "$RESTORE_JSON" | field id)
[ -n "$RESTORE_ID" ] || fail "no id in restore response: $RESTORE_JSON"
wait_ws_ready "$RESTORE_ID"
[ -f "$(live_dir "$RESTORE_ID")/hello.txt" ] || fail "restored workspace is missing the pushed file"

# ---------------------------------------------------------------------------
# Environment: an environment owns exactly ONE subvolume of its own (never a mounted
# workspace); every declared volume is a folder inside it (live/volumes/{name}), reached as a
# subPath on the env's one `live-{id}` claim. The service is named `db` and mounts volume "data":
# it writes a marker file into it and then serves that same folder over port 27017, which is what
# makes the connectivity assertions below non-vacuous — a port nobody listens on would "fail to
# connect" for every reason, including the wrong one. `env stop` (EnvDown) always pushes that one
# subvolume atomically before tearing the deployments down — see bins/agent/src/lib.rs — so,
# unlike the workspace above, there is no separate push call to make.
# ---------------------------------------------------------------------------
# busybox:1.36, not alpine: alpine ships busybox WITHOUT the httpd applet (it is in busybox-extras),
# so `httpd` there dies with `sh: httpd: not found` and every connectivity assertion below becomes
# vacuous — the service never listens, so "denied" and "broken" look identical.
log "creating environment with a volume mount and a listening port"
ENV_JSON=$(curl -fsS -X POST "$BASE/v1/environments" -H "Authorization: Bearer $USER_TOKEN" \
  -H 'Content-Type: application/json' -d '{
    "name":"e2e-env",
    "region":"'"$REGION_ID"'",
    "services":[{
      "name":"db",
      "image":"busybox:1.36",
      "command":["sh","-c","echo hi from ws_e2e > /ws/marker.txt; httpd -f -p 27017 -h /ws"],
      "env":{},
      "mounts":[{"folder":"data","path":"/ws"}],
      "ports":[27017]
    }]
  }')
ENV_ID=$(echo "$ENV_JSON" | field id)
[ -n "$ENV_ID" ] || fail "no id in environment create response: $ENV_JSON"
ENV_NS="env-$ENV_ID"
ENV_MARKER="$MOUNT/vol/$ENV_ID/live/volumes/data/marker.txt"
wait_env_ready "$ENV_ID"

log "checking the service wrote its marker into the env's own subvolume"
for i in $(seq 1 30); do
  [ -f "$ENV_MARKER" ] && break
  sleep 1
  [ "$i" -eq 30 ] && fail "marker.txt never appeared in the env's volume mount"
done
grep -q "hi from ws_e2e" "$ENV_MARKER" || fail "marker.txt has unexpected content"

# Same content, read back through the container this time — the kubectl replacement for what used
# to be a container-runtime exec against `env-{id}-db-1`. It doubles as the positive control for assertion 2: the
# listener really is up and really does answer, so a refusal from elsewhere means a policy, not a
# dead port.
log "reading the marker back out of the db container, and proving its port answers"
kubectl -n "$ENV_NS" exec deploy/db -- wget -qO- 127.0.0.1:27017/marker.txt \
  | grep -q "hi from ws_e2e" || fail "db container does not serve its own marker on 27017"

# --- assertion 1: cross-namespace service DNS ------------------------------------------------
# The thing compose genuinely provided for free, and the reason the replaced design was going to
# hand-roll a DNS resolver. A workspace lives in ws-{owner}; the environment's Service lives in
# env-{id}; the fully-qualified name has to resolve across that boundary.
log "checking cross-namespace service DNS from the workspace to the environment"
kubectl -n "$WS_NS" exec "$WS_ID" -- getent hosts "db.$ENV_NS" | grep -q . \
  || fail "cross-namespace DNS: db.$ENV_NS does not resolve from workspace $WS_ID"

# --- assertion 2: default-deny actually denies -------------------------------------------------
# A NetworkPolicy nobody tests is a NetworkPolicy that may silently not be enforced: k3s shipping
# without kube-router is one flag away and produces no error at all, only permitted traffic. So
# prove the NEGATIVE from a namespace that was never attached to this environment. `|| true` on the
# wget is load-bearing — under `set -e` a correctly-refused connection would otherwise abort the
# script as a "failure" and never reach the assertion.
log "checking a default-deny namespace genuinely refuses the environment"
PROBE_NS="ws-e2e-probe-$RANDOM"
kubectl create namespace "$PROBE_NS" >/dev/null
kubectl -n "$PROBE_NS" run probe --image=busybox:1.36 --restart=Never \
  --command -- sleep 600 >/dev/null
kubectl -n "$PROBE_NS" wait --for=condition=Ready pod/probe --timeout=120s \
  || fail "probe pod never became ready"
PROBE_OUT=$(kubectl -n "$PROBE_NS" exec probe -- \
  timeout 5 wget -q -O- "db.$ENV_NS:27017/marker.txt" 2>&1 || true)
echo "$PROBE_OUT" | grep -q "hi from ws_e2e" \
  && fail "default-deny is not enforced: an unattached namespace reached db.$ENV_NS"

# --- assertion 3: the controller reconciles ----------------------------------------------------
# Delete a Deployment out from under a running environment and assert it comes back with nobody
# calling the API. That convergence is the entire claim of moving from a job queue to controllers;
# under the old poller this deletion was simply permanent.
log "deleting the db Deployment and waiting for the controller to put it back"
kubectl -n "$ENV_NS" delete deploy db --wait=true >/dev/null
# `rollout status` on an object that does not exist yet is an error, not a wait, so the recreate is
# waited for first and only then the rollout.
for i in $(seq 1 120); do
  kubectl -n "$ENV_NS" get deploy db >/dev/null 2>&1 && break
  sleep 1
  [ "$i" -eq 120 ] && fail "controller did not converge: deploy/db never came back"
done
kubectl -n "$ENV_NS" rollout status deploy/db --timeout=120s \
  || fail "controller recreated deploy/db but it never became available"

log "stopping environment (this pushes the env's own subvolume)"
curl -fsS -X POST "$BASE/v1/environments/$ENV_ID/stop" -H "Authorization: Bearer $USER_TOKEN" >/dev/null
wait_env_stopped "$ENV_ID"

log "checking the env's volume registry history is non-empty after stop"
for i in $(seq 1 30); do
  ENV_HISTORY=$(curl -fsS "$BASE/v1/volumes/$ENV_ID/history" -H "Authorization: Bearer $USER_TOKEN")
  [ "$ENV_HISTORY" != "[]" ] && break
  sleep 1
  [ "$i" -eq 30 ] && fail "env volume history is still empty after stop: $ENV_HISTORY"
done

echo
echo "OK: create -> Ready, write, push (message+history+refs), clone (pushed content), clone (running source), restore (explicit snapshot), env up (own subvolume + write), cross-namespace DNS, default-deny enforced, controller reconcile, env down (push+stop, history) all passed"
