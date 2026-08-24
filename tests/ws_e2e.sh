#!/usr/bin/env bash
# End-to-end workspaces/environments test: a real btrfs pool, a real rustic-git-api and
# rustic-git-agent, a real Cosmos DB, real Azure blob storage, real docker compose.
#
# Mirrors tests/registry_e2e.sh's conventions: exit 77 when a prerequisite is absent (root-capable
# btrfs, a working docker compose, Cosmos/Azure credentials) rather than failing mid-script; one
# trap tears everything down. This needs a Linux box with btrfs + root (the CLAUDE.md-documented
# `wssnap-bench` VM) — it cannot run on a Mac laptop, which is why this script was authored without
# ever being run locally: read it carefully before trusting a change to it.
#
# What it does NOT cover, on purpose (ponytail: exercise the plumbing end to end, not every knob):
#   - the `WsPush` job has no user-facing route (crates/workspaces/src/api.rs only wires
#     WsCreate/WsFork/WsClone/WsDelete/EnvUp/EnvDown) — a v1 API gap. Push is exercised the only
#     way a client can reach it today: `env stop` always pushes every mounted workspace
#     (bins/agent/src/lib.rs's EnvDown arm) before compose down.
#   - the fancier "clone while a writer is mutating the source, stop hook pauses it" path is
#     already covered by crates/workspaces/tests/engine_ops.rs; this script clones an idle
#     workspace, which is enough to prove the HTTP round trip end to end.
#
# Exit status: 0 = every step passed. 77 = a prerequisite was missing and nothing was started.
# Anything else = a real failure.
set -euo pipefail

log() { echo "==> $*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }

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
docker compose version >/dev/null 2>&1 || {
  echo "SKIP: docker compose not working" >&2
  exit 77
}
[ -n "${COSMOS_ENDPOINT:-}" ] && [ -n "${COSMOS_KEY:-}" ] || { echo "SKIP: COSMOS_ENDPOINT/COSMOS_KEY not set" >&2; exit 77; }
[ -n "${AZURE_ACCOUNT:-}" ] && [ -n "${AZURE_KEY:-}" ] && [ -n "${AZURE_CONTAINER:-}" ] || {
  echo "SKIP: AZURE_ACCOUNT/AZURE_KEY/AZURE_CONTAINER not set" >&2
  exit 77
}

API_BIN="${WS_E2E_API_BIN:-target/debug/rustic-git-api}"
AGENT_BIN="${WS_E2E_AGENT_BIN:-target/debug/rustic-git-agent}"
if [ ! -x "$API_BIN" ] || [ ! -x "$AGENT_BIN" ]; then
  log "building rustic-git-api/rustic-git-agent (not found at $API_BIN / $AGENT_BIN)"
  cargo build -q --bin rustic-git-api --bin rustic-git-agent
fi

# ---------------------------------------------------------------------------
# State torn down by the trap below
# ---------------------------------------------------------------------------
API_PID=""
AGENT_PID=""
MOUNT=""
IMG=""
TMPD=""
COSMOS_DB="wse2e-$RANDOM$RANDOM"
ENV_ID=""
ENV_DIR=""

cleanup() {
  set +e
  [ -n "$ENV_ID" ] && [ -n "$ENV_DIR" ] && docker compose -p "env-$ENV_ID" -f "$ENV_DIR/docker-compose.yml" down >/dev/null 2>&1
  [ -n "$AGENT_PID" ] && kill "$AGENT_PID" >/dev/null 2>&1
  [ -n "$API_PID" ] && kill "$API_PID" >/dev/null 2>&1
  [ -n "$AGENT_PID" ] && wait "$AGENT_PID" 2>/dev/null
  [ -n "$API_PID" ] && wait "$API_PID" 2>/dev/null
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
# Start the api
# ---------------------------------------------------------------------------
JWT_SECRET="e2e-jwt-secret-$(head -c24 /dev/urandom | od -An -tx1 | tr -d ' \n')"
PEER_SECRET="e2e-peer-secret-$RANDOM"
API_ADDR="127.0.0.1:8190"
ADMIN_EMAIL="ws-e2e-admin@example.test"
USER_EMAIL="ws-e2e-user@example.test"

log "starting rustic-git-api on $API_ADDR (Cosmos db $COSMOS_DB)"
RUSTIC_GIT_S3_URL="mem://" \
RUSTIC_GIT_JWT_SECRET="$JWT_SECRET" \
RUSTIC_GIT_PEER_SECRET="$PEER_SECRET" \
RUSTIC_GIT_API_ADDR="$API_ADDR" \
RUSTIC_GIT_WORKSPACES_ADMINS="$ADMIN_EMAIL" \
COSMOS_ENDPOINT="$COSMOS_ENDPOINT" \
COSMOS_KEY="$COSMOS_KEY" \
COSMOS_DB="$COSMOS_DB" \
"$API_BIN" &
API_PID=$!

BASE="http://$API_ADDR"
log "waiting for the api to answer"
for i in $(seq 1 60); do
  # Any HTTP response at all (even 401 for a missing token) means the listener is up — don't use
  # curl -f here, a 401 is exactly what an unauthenticated probe should get.
  # curl prints 000 on a refused connection, so 000 means "not up yet", not "answered".
  code=$(curl -sS -o /dev/null -w '%{http_code}' "$BASE/v1/regions" 2>/dev/null || echo "")
  [ -n "$code" ] && [ "$code" != "000" ] && break
  sleep 1
  [ "$i" -eq 60 ] && fail "api never came up on $BASE"
done

# ---------------------------------------------------------------------------
# Mint HS256 session JWTs by hand: there is no CLI in this repo that mints one (checked
# bins/server's `admin` subcommands — those are registry tokens, a different mechanism), and this
# script owns the signing secret it just started the api with, so it can sign the same tokens the
# api's own crates/core/src/jwt.rs `Jwt::mint` would produce (same header, same Claims shape).
# ---------------------------------------------------------------------------
b64url() { openssl base64 -A | tr '+/' '-_' | tr -d '='; }

mint_jwt() {
  local email="$1" name="$2"
  local now exp header payload signing_input sig
  now=$(date +%s)
  exp=$((now + 43200))
  header=$(printf '{"typ":"JWT","alg":"HS256"}' | b64url)
  payload=$(printf '{"sub":"%s","name":"%s","typ":"session","iat":%d,"exp":%d}' "$email" "$name" "$now" "$exp" | b64url)
  signing_input="$header.$payload"
  sig=$(printf '%s' "$signing_input" | openssl dgst -sha256 -hmac "$JWT_SECRET" -binary | b64url)
  echo "$signing_input.$sig"
}

ADMIN_TOKEN=$(mint_jwt "$ADMIN_EMAIL" "E2E Admin")
USER_TOKEN=$(mint_jwt "$USER_EMAIL" "E2E User")

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

log "starting rustic-git-agent against pool $MOUNT"
WS_API_URL="$BASE" \
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

wait_ws_state() {
  local id="$1" want="$2"
  for i in $(seq 1 60); do
    local body state
    body=$(curl -fsS "$BASE/v1/workspaces/$id" -H "Authorization: Bearer $USER_TOKEN")
    state=$(echo "$body" | field state)
    [ "$state" = "$want" ] && return 0
    [ "$state" = "error" ] && fail "workspace $id went to state error: $body"
    sleep 1
  done
  fail "workspace $id never reached state $want (last: $state)"
}

wait_env_state() {
  local id="$1" want="$2"
  for i in $(seq 1 60); do
    local body state
    body=$(curl -fsS "$BASE/v1/environments/$id" -H "Authorization: Bearer $USER_TOKEN")
    state=$(echo "$body" | field state)
    [ "$state" = "$want" ] && return 0
    [ "$state" = "error" ] && fail "environment $id went to state error: $body"
    sleep 1
  done
  fail "environment $id never reached state $want (last: $state)"
}

live_dir() { echo "$MOUNT/ws/$1/live"; }

# ---------------------------------------------------------------------------
# Create workspace, wait ready, write into live
# ---------------------------------------------------------------------------
log "creating workspace"
WS_JSON=$(curl -fsS -X POST "$BASE/v1/workspaces" -H "Authorization: Bearer $USER_TOKEN" \
  -H 'Content-Type: application/json' -d '{"name":"e2e-ws","region":"'"$REGION_ID"'","quota_gb":5}')
WS_ID=$(echo "$WS_JSON" | field id)
[ -n "$WS_ID" ] || fail "no id in workspace create response: $WS_JSON"
wait_ws_state "$WS_ID" ready

log "writing a file into the live subvolume"
sudo bash -c "printf 'hello from ws_e2e' > '$(live_dir "$WS_ID")/hello.txt'"
[ -f "$(live_dir "$WS_ID")/hello.txt" ] || fail "write into live did not land"

# ---------------------------------------------------------------------------
# Fork: new workspace grafted onto the same ref/state
# ---------------------------------------------------------------------------
log "forking workspace"
FORK_JSON=$(curl -fsS -X POST "$BASE/v1/workspaces/$WS_ID/fork" -H "Authorization: Bearer $USER_TOKEN" \
  -H 'Content-Type: application/json' -d '{"name":"e2e-ws-fork"}')
FORK_ID=$(echo "$FORK_JSON" | field id)
[ -n "$FORK_ID" ] || fail "no id in fork response: $FORK_JSON"
wait_ws_state "$FORK_ID" ready
# Fork semantics: a fork materializes the last SAVED snapshot, and hello.txt was written after
# the create-time push (there is no direct push route in v1) — so the fork must NOT have it.
# Clone is the verb that captures the running state; asserted below.
[ ! -f "$(live_dir "$FORK_ID")/hello.txt" ] || fail "fork unexpectedly contains an unpushed live file"

# ---------------------------------------------------------------------------
# Clone: independent copy, same shape check
# ---------------------------------------------------------------------------
log "cloning workspace"
CLONE_JSON=$(curl -fsS -X POST "$BASE/v1/workspaces/$WS_ID/clone" -H "Authorization: Bearer $USER_TOKEN" \
  -H 'Content-Type: application/json' -d '{"name":"e2e-ws-clone"}')
CLONE_ID=$(echo "$CLONE_JSON" | field id)
[ -n "$CLONE_ID" ] || fail "no id in clone response: $CLONE_JSON"
wait_ws_state "$CLONE_ID" ready
[ -f "$(live_dir "$CLONE_ID")/hello.txt" ] || fail "cloned workspace is missing the file written into the source"

# ---------------------------------------------------------------------------
# Environment: alpine service mounts the original workspace and writes a marker file into it.
# `env stop` (EnvDown) always pushes every mounted workspace before tearing compose down — see
# bins/agent/src/lib.rs — which is how this script exercises WsPush (no direct route exists).
# ---------------------------------------------------------------------------
log "creating environment mounting the workspace"
ENV_JSON=$(curl -fsS -X POST "$BASE/v1/environments" -H "Authorization: Bearer $USER_TOKEN" \
  -H 'Content-Type: application/json' -d '{
    "name":"e2e-env",
    "region":"'"$REGION_ID"'",
    "services":[{
      "name":"writer",
      "image":"alpine:3",
      "command":["sh","-c","echo hi from ws_e2e > /ws/marker.txt; sleep 300"],
      "env":{},
      "mounts":[{"workspace":"'"$WS_ID"'","path":"/ws"}]
    }]
  }')
ENV_ID=$(echo "$ENV_JSON" | field id)
[ -n "$ENV_ID" ] || fail "no id in environment create response: $ENV_JSON"
ENV_DIR="$MOUNT/env/$ENV_ID"
wait_env_state "$ENV_ID" running

log "checking the service wrote its marker into the live subvolume"
for i in $(seq 1 30); do
  [ -f "$(live_dir "$WS_ID")/marker.txt" ] && break
  sleep 1
  [ "$i" -eq 30 ] && fail "marker.txt never appeared in the live subvolume"
done
grep -q "hi from ws_e2e" "$(live_dir "$WS_ID")/marker.txt" || fail "marker.txt has unexpected content"

log "stopping environment (this pushes the mounted workspace)"
curl -fsS -X POST "$BASE/v1/environments/$ENV_ID/stop" -H "Authorization: Bearer $USER_TOKEN" >/dev/null
wait_env_state "$ENV_ID" stopped

echo
echo "OK: create -> ready, write, fork, clone, env up (mount + write), env down (push + stop) all passed"
