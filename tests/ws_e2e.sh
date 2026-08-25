#!/usr/bin/env bash
# End-to-end workspaces/environments test: a real btrfs pool, a real rustic-git (server tier),
# rustic-git-api, rustic-git-agent, a real Cosmos DB, real Azure blob storage, real docker compose.
#
# Mirrors tests/registry_e2e.sh's conventions: exit 77 when a prerequisite is absent (root-capable
# btrfs, a working docker compose, Cosmos/Azure credentials) rather than failing mid-script; one
# trap tears everything down. This needs a Linux box with btrfs + root (the CLAUDE.md-documented
# `wssnap-bench` VM) — it cannot run on a Mac laptop, which is why this script was authored without
# ever being run locally: read it carefully before trusting a change to it.
#
# Three binaries now, not two: the volume registry (commits/history/ref, agent register/work/done)
# moved onto the server tier (rustic-git serve — see bins/server/src/vol_agent.rs), so the agent
# long-polls THAT process (WS_REGISTRY_URL), and rustic-git-api reaches it as a client
# (RUSTIC_GIT_VOL_AGENT_URL/_TOKEN) to serve GET /v1/volumes/*. rustic-git-api still owns
# /v1/workspaces|environments|regions (Cosmos-backed) — that split is unchanged.
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
ENV_DIR=""
WS_ID=""
CLONE1_ID=""
CLONE_ID=""
RESTORE_ID=""

cleanup() {
  set +e
  [ -n "$ENV_ID" ] && [ -n "$ENV_DIR" ] && docker compose -p "env-$ENV_ID" -f "$ENV_DIR/docker-compose.yml" down >/dev/null 2>&1
  # Every materialized workspace runs its own ws-{id} container now — rm -f each one this run
  # created (create/clone/restore ids), not just the compose project above.
  for id in "$WS_ID" "$CLONE1_ID" "$CLONE_ID" "$RESTORE_ID"; do
    [ -n "$id" ] && docker rm -f "ws-$id" >/dev/null 2>&1
  done
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
# (/vol-agent/register|work|jobs|commits|history|ref) — RUSTIC_GIT_VOL_AGENT_TOKENS is the
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
# Start the api: the user-facing /v1/workspaces|environments|regions|volumes surface. Reaches the
# server tier as a RegistryClient (RUSTIC_GIT_VOL_AGENT_URL/_TOKEN) purely to serve
# GET /v1/volumes/*; workspace/environment/region/job CRUD stays direct-to-Cosmos, same db as the
# server above so a job the api queues is visible to the agent leasing it from the server.
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

ADMIN_TOKEN=$(mint_jwt "$ADMIN_EMAIL" "E2E Admin" "e2eadmin")
USER_TOKEN=$(mint_jwt "$USER_EMAIL" "E2E User" "e2euser")

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

log "starting rustic-git-agent against pool $MOUNT (registry at $SERVER_BASE)"
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

# Push is the only commit/push verb that leaves a visible mark on the workspace doc: engine::ops's
# job-done handler writes `volume` (vol/{owner}/{id}) once the workspace's first push lands — see
# model.rs's doc on `Workspace::volume`. Poll that instead of guessing a sleep.
wait_ws_pushed() {
  local id="$1"
  for i in $(seq 1 60); do
    local body vol
    body=$(curl -fsS "$BASE/v1/workspaces/$id" -H "Authorization: Bearer $USER_TOKEN")
    vol=$(echo "$body" | field volume)
    [ -n "$vol" ] && return 0
    sleep 1
    [ "$i" -eq 60 ] && fail "workspace $id never got a volume pointer from push (last: $body)"
  done
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
wait_ws_state "$WS_ID" ready

log "checking the workspace container is running"
WS_CONTAINER=$(docker ps --filter "name=ws-$WS_ID" --format '{{.Names}}')
[ -n "$WS_CONTAINER" ] || fail "no running container named ws-$WS_ID after workspace reached ready"

log "writing a file into the live subvolume"
sudo bash -c "printf 'hello from ws_e2e' > '$(live_dir "$WS_ID")/hello.txt'"
[ -f "$(live_dir "$WS_ID")/hello.txt" ] || fail "write into live did not land"

# ---------------------------------------------------------------------------
# Commit: local-only (RO snapshot + lineage append, marked unpushed — see model.rs's JobKind::
# Commit doc). No network call the agent makes touches the registry, so the volume's registry
# history must stay EMPTY until push — this is the git-correlation proof: commit and push are
# now two different verbs with two different blast radii, and history is where that shows up.
# There is no client-visible "job done" signal for a local-only job (no `volume` change, no state
# change), so this waits a bounded few seconds — the job doc itself says "fast, no network".
# ---------------------------------------------------------------------------
log "committing workspace (local-only)"
# Workspace CREATION already pushed one record (init = create + commit + push of the empty
# subvolume), so the git-correlation proof is that a commit leaves that count UNCHANGED —
# not that history is empty.
BEFORE=$(curl -fsS "$BASE/v1/volumes/$WS_ID/history" -H "Authorization: Bearer $USER_TOKEN" | grep -o '"id"' | wc -l | tr -d ' ')
curl -fsS -X POST "$BASE/v1/workspaces/$WS_ID/commit" -H "Authorization: Bearer $USER_TOKEN" \
  -H 'Content-Type: application/json' -d '{"message":"first commit"}' >/dev/null
sleep 5

log "checking commit did NOT touch the volume registry"
AFTER=$(curl -fsS "$BASE/v1/volumes/$WS_ID/history" -H "Authorization: Bearer $USER_TOKEN" | grep -o '"id"' | wc -l | tr -d ' ')
[ "$AFTER" = "$BEFORE" ] || fail "volume history grew after commit alone ($BEFORE -> $AFTER): commit must stay local"

# ---------------------------------------------------------------------------
# Push: uploads the unpushed layer(s), posts their CommitRecords, moves the registry ref. This is
# the only step that reaches the server tier's /vol-agent/{owner}/{name}/commits — history must
# now be non-empty.
# ---------------------------------------------------------------------------
log "pushing workspace"
curl -fsS -X POST "$BASE/v1/workspaces/$WS_ID/push" -H "Authorization: Bearer $USER_TOKEN" >/dev/null

log "checking the push landed in the volume registry"
# The workspace's `volume` pointer was already set by creation's initial push, so the only
# honest signal that THIS push finished is the history itself growing. Poll for it.
PUSHED="$BEFORE"
for i in $(seq 1 30); do
  PUSHED=$(curl -fsS "$BASE/v1/volumes/$WS_ID/history" -H "Authorization: Bearer $USER_TOKEN" | grep -o '"id"' | wc -l | tr -d ' ')
  [ "$PUSHED" -gt "$BEFORE" ] && break
  sleep 2
done
[ "$PUSHED" -gt "$BEFORE" ] || fail "volume history did not grow after push ($BEFORE -> $PUSHED)"
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
wait_ws_state "$CLONE1_ID" ready
[ -f "$(live_dir "$CLONE1_ID")/hello.txt" ] || fail "clone is missing the pushed file (clone must materialize pushed history)"

# ---------------------------------------------------------------------------
# Delete the clone: proves the completed-delete half of the storage-hygiene work end to end — the
# doc goes to `deleted` and the agent's WsDelete job reclaims the clone's ENTIRE local volume dir
# (not just the live subvolume), never touching the source's registry history (blobs are shared,
# immutable, untouched by design).
# ---------------------------------------------------------------------------
log "deleting the cloned workspace"
curl -fsS -X DELETE "$BASE/v1/workspaces/$CLONE1_ID" -H "Authorization: Bearer $USER_TOKEN" >/dev/null
wait_ws_state "$CLONE1_ID" deleted

log "checking the clone's local volume directory was reclaimed"
CLONE1_VOLDIR="$MOUNT/vol/$CLONE1_ID"
for i in $(seq 1 30); do
  [ ! -d "$CLONE1_VOLDIR" ] && break
  sleep 1
  [ "$i" -eq 30 ] && fail "clone's volume dir $CLONE1_VOLDIR still present after delete"
done

# ---------------------------------------------------------------------------
# Clone (running-source path): the source's container is still up, so the agent's `WsClone` arm
# picks `Engine::clone_running` this time (prefetch, then a short stop/commit/push/start window)
# instead of the registry-history path used above — same pushed content either way, since the
# source has nothing unpushed left after the push above.
# ---------------------------------------------------------------------------
log "cloning workspace (running-source path)"
CLONE_JSON=$(curl -fsS -X POST "$BASE/v1/workspaces/$WS_ID/clone" -H "Authorization: Bearer $USER_TOKEN" \
  -H 'Content-Type: application/json' -d '{"name":"e2e-ws-clone"}')
CLONE_ID=$(echo "$CLONE_JSON" | field id)
[ -n "$CLONE_ID" ] || fail "no id in clone response: $CLONE_JSON"
wait_ws_state "$CLONE_ID" ready
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
wait_ws_state "$RESTORE_ID" ready
[ -f "$(live_dir "$RESTORE_ID")/hello.txt" ] || fail "restored workspace is missing the pushed file"

# ---------------------------------------------------------------------------
# Environment: an environment owns exactly ONE subvolume of its own (never a mounted
# workspace); every declared volume is a folder inside it (live/volumes/{name}). The alpine
# service mounts volume "data" and writes a marker file into it. `env stop` (EnvDown) always
# commits+pushes that one subvolume atomically before tearing compose down — see
# bins/agent/src/lib.rs — so, unlike the workspace above, there is no separate commit/push call to
# make: the registry history check below covers both in one step.
# ---------------------------------------------------------------------------
log "creating environment with a volume mount"
ENV_JSON=$(curl -fsS -X POST "$BASE/v1/environments" -H "Authorization: Bearer $USER_TOKEN" \
  -H 'Content-Type: application/json' -d '{
    "name":"e2e-env",
    "region":"'"$REGION_ID"'",
    "services":[{
      "name":"writer",
      "image":"alpine:3",
      "command":["sh","-c","echo hi from ws_e2e > /ws/marker.txt; sleep 300"],
      "env":{},
      "mounts":[{"folder":"data","path":"/ws"}]
    }]
  }')
ENV_ID=$(echo "$ENV_JSON" | field id)
[ -n "$ENV_ID" ] || fail "no id in environment create response: $ENV_JSON"
ENV_DIR="$MOUNT/env/$ENV_ID"
ENV_MARKER="$MOUNT/vol/$ENV_ID/live/volumes/data/marker.txt"
wait_env_state "$ENV_ID" running

log "checking the service wrote its marker into the env's own subvolume"
for i in $(seq 1 30); do
  [ -f "$ENV_MARKER" ] && break
  sleep 1
  [ "$i" -eq 30 ] && fail "marker.txt never appeared in the env's volume mount"
done
grep -q "hi from ws_e2e" "$ENV_MARKER" || fail "marker.txt has unexpected content"

log "stopping environment (this commits+pushes the env's own subvolume)"
curl -fsS -X POST "$BASE/v1/environments/$ENV_ID/stop" -H "Authorization: Bearer $USER_TOKEN" >/dev/null
wait_env_state "$ENV_ID" stopped

log "checking the env's volume registry history is non-empty after stop"
for i in $(seq 1 30); do
  ENV_HISTORY=$(curl -fsS "$BASE/v1/volumes/$ENV_ID/history" -H "Authorization: Bearer $USER_TOKEN")
  [ "$ENV_HISTORY" != "[]" ] && break
  sleep 1
  [ "$i" -eq 30 ] && fail "env volume history is still empty after stop: $ENV_HISTORY"
done

echo
echo "OK: create -> ready, write, commit (local, empty history), push (history+refs), clone (pushed content), clone (running source), restore (explicit snapshot), env up (own subvolume + write), env down (commit+push+stop, history) all passed"
