#!/usr/bin/env bash
# End-to-end workspaces/environments test: a real btrfs pool, a real rustic-git (server tier),
# rustic-git-api, rustic-git-agent, a real Cosmos DB, real Azure blob storage, a real k3s cluster.
#
# Mirrors tests/registry_e2e.sh's conventions: exit 77 when a prerequisite is absent (root-capable
# btrfs, a reachable cluster with the CRDs installed, Cosmos/Azure credentials) rather than failing
# mid-script; one trap tears everything down. This needs a Linux box with btrfs + root AND a k3s
# node (the CLAUDE.md-documented `wssnap-bench` VM) — it cannot run on a Mac laptop, which is why
# this script was authored without ever being run locally: read it carefully before trusting a
# change to it. CI does not pre-build anything for it — build the binaries on the VM itself
# (`cargo build --bin rustic-git --bin rustic-git-api --bin rustic-git-agent`). A single-node k3s carrying both role labels is enough; nothing here needs two nodes.
#
# Three binaries, and none of them talks to a volume registry: a volume's history is the chain of
# Ready `Snapshot` CRs a push wrote, so GET /v1/volumes/* reads the CRDs (`Volume.status.head` names the tip)
# and the agent reaches nothing over HTTP. /v1/workspaces|environments own the CRDs; only /v1/regions is
# Cosmos-backed. The agent is a
# CONTROLLER now, not a poller: it watches the CRDs, so this script waits on the conditions those
# controllers write (`kubectl wait --for=condition=Ready`) rather than polling document state.
#
# Namespaces (crd.rs): all of an owner's workspace pods share `ws-{owner}`; an environment gets its
# own `env-{id}`. Live volumes are typed `hostPath` mounts straight off the node's btrfs pool (no
# PV/PVC layer), and every namespace enforces Pod Security Admission `privileged` to admit them.
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
command -v git >/dev/null 2>&1 || {
  echo "SKIP: git not on PATH (the seeded-workspace phase pushes a real repository)" >&2
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
kubectl -n rustic-git-system rollout status deployment/rustic-git-gateway --timeout=60s >/dev/null 2>&1 || {
  echo "SKIP: rustic-git-gateway deployment not Ready in rustic-git-system (not applied, or still in kube-system — see deploy/k3s/gateway.yaml)" >&2
  exit 77
}
[ -n "${COSMOS_ENDPOINT:-}" ] && [ -n "${COSMOS_KEY:-}" ] || { echo "SKIP: COSMOS_ENDPOINT/COSMOS_KEY not set" >&2; exit 77; }
[ -n "${AZURE_ACCOUNT:-}" ] && [ -n "${AZURE_KEY:-}" ] && [ -n "${AZURE_CONTAINER:-}" ] || {
  echo "SKIP: AZURE_ACCOUNT/AZURE_KEY/AZURE_CONTAINER not set" >&2
  exit 77
}
kubectl -n rustic-git-system get deploy zerofs >/dev/null 2>&1 || {
  echo "SKIP: zerofs Deployment not in rustic-git-system (not applied — see deploy/k3s/zerofs.yaml)" >&2
  exit 77
}

SERVER_BIN="${WS_E2E_SERVER_BIN:-target/debug/rustic-git}"
API_BIN="${WS_E2E_API_BIN:-target/debug/rustic-git-api}"
AGENT_BIN="${WS_E2E_AGENT_BIN:-target/debug/rustic-git-agent}"
KL_BIN="${WS_E2E_KL_BIN:-target/debug/kl}"
if [ ! -x "$SERVER_BIN" ] || [ ! -x "$API_BIN" ] || [ ! -x "$AGENT_BIN" ] || [ ! -x "$KL_BIN" ]; then
  log "building rustic-git/rustic-git-api/rustic-git-agent/kl (not found at $SERVER_BIN / $API_BIN / $AGENT_BIN / $KL_BIN)"
  cargo build -q --bin rustic-git --bin rustic-git-api --bin rustic-git-agent --bin kl
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
# ONE database, reused, not a fresh `wse2e-$RANDOM` per run.
#
# The per-run name was only deletable by the `az` branch in `cleanup`, which needs the Azure CLI on
# the runner — and the runner is a cluster node, where installing it is not worth it. So every run
# leaked a database: 34 of them had accumulated before anyone looked. A single reused name cannot
# accumulate, and stale rows inside it are harmless because everything this script creates is
# already namespaced by a random id (region, workspace, environment), so no run can see another's.
#
# Override with WS_E2E_COSMOS_DB when two runs must genuinely not share — but note the script
# already requires exclusive use of the node's pool and its agent, so concurrent runs are not a
# supported shape anyway.
COSMOS_DB="${WS_E2E_COSMOS_DB:-wse2e}"
ENV_ID=""
DELETED_ENV_ID=""
WS_ID=""
WS_NS=""
PROBE_NS=""
CLONE1_ID=""
CLONE_ID=""
CM_CLONE_ID=""
CM_RESTORE_ID=""
RESTORE_ID=""
SEED_ID=""
OTHER_NODE_WS_ID=""
SNAP_STATE_RESTORE_ID=""

cleanup() {
  set +e
  # The CRDs are cluster-scoped and OWN everything namespaced they produced (namespace, pod,
  # deployments, services, policies), so deleting the four objects is the whole teardown —
  # garbage collection does the rest. The probe namespace is ours, not the controller's.
  for eid in "$ENV_ID" "$DELETED_ENV_ID"; do
    [ -n "$eid" ] && kubectl delete environment "$eid" --ignore-not-found --wait=false >/dev/null 2>&1
  done
  for id in "$WS_ID" "$CLONE1_ID" "$CLONE_ID" "$RESTORE_ID" "$SEED_ID" "$OTHER_NODE_WS_ID" "$SNAP_STATE_RESTORE_ID" "$CM_CLONE_ID" "$CM_RESTORE_ID"; do
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
    # Not a leak any more: the name is fixed, so the next run reuses this database rather than
    # adding one. Deleting it is a convenience when `az` happens to be present, not a requirement.
    echo "NOTE: az CLI not found — Cosmos test db '$COSMOS_DB' left in place for the next run" >&2
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
SERVER_HTTP_ADDR="127.0.0.1:8180"
SERVER_PEER_ADDR="127.0.0.1:8181"
# peer_stream binds PEER_ADDR+1 (8182), so ssh moves clear of it.
#
# SSH binds every interface, unlike the two HTTP listeners: the git-seeding init container clones
# from INSIDE a pod, so it reaches this process over the node's own IP, not over loopback. Nothing
# else in this script needs that reachability, and the box this runs on is a private test node.
SERVER_SSH_ADDR="0.0.0.0:8183"
SERVER_SSH_PORT="8183"
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
# Start the server tier: git, the registry, and the ssh listener the workspace pods clone
# through. Solo mode (no RUSTIC_GIT_PEER_SVC): a single node needs no ownership map.
#
# A file:// store, not mem://, and the api below shares the same one: mem:// is per-PROCESS, and
# three things here must see one set of credentials — the `admin` subcommands that seed a git repo,
# this server's ssh auth, and the api reading the owner's platform key to install it in the
# workspace namespace. Repo databases are still only ever opened by this process.
# ---------------------------------------------------------------------------
STORE_URL="file://$TMPD/store"
mkdir -p "$TMPD/store"
log "starting rustic-git serve on $SERVER_HTTP_ADDR (Cosmos db $COSMOS_DB)"
# Its own cache dir, like every other process here: the default is `./.local/cache` in the repo,
# which would make one run's leftovers an input to the next.
RUSTIC_GIT_CACHE_DIR="$TMPD/cache-server" \
RUSTIC_GIT_S3_URL="$STORE_URL" \
RUSTIC_GIT_JWT_SECRET="$JWT_SECRET" \
RUSTIC_GIT_HTTP_ADDR="$SERVER_HTTP_ADDR" \
RUSTIC_GIT_PEER_ADDR="$SERVER_PEER_ADDR" \
RUSTIC_GIT_SSH_ADDR="$SERVER_SSH_ADDR" \
RUSTIC_GIT_HOST_KEY="$TMPD/host_key" \
COSMOS_ENDPOINT="$COSMOS_ENDPOINT" \
COSMOS_KEY="$COSMOS_KEY" \
COSMOS_DB="$COSMOS_DB" \
"$SERVER_BIN" serve &
SERVER_PID=$!

log "waiting for the server to answer"
wait_for_listener "$SERVER_BASE/healthz" "rustic-git serve"

# ---------------------------------------------------------------------------
# Start the api: the user-facing /v1/workspaces|environments|regions|volumes surface — the one
# writer of the CRDs the controllers reconcile. It talks to no other tier: /v1/volumes/* reads
# SnapshotRequests out of the cluster, and only /v1/regions is Cosmos-backed (same db as the
# server above).
# ---------------------------------------------------------------------------
log "starting rustic-git-api on $API_ADDR (Cosmos db $COSMOS_DB)"
RUSTIC_GIT_CACHE_DIR="$TMPD/cache-api" \
RUSTIC_GIT_S3_URL="$STORE_URL" \
RUSTIC_GIT_JWT_SECRET="$JWT_SECRET" \
RUSTIC_GIT_PEER_SECRET="$PEER_SECRET" \
RUSTIC_GIT_API_ADDR="$API_ADDR" \
RUSTIC_GIT_WORKSPACES_ADMINS="$ADMIN_EMAIL" \
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
echo "$REGION_JSON" | grep -q "\"$REGION_ID\"" || fail "region create did not echo the region: $REGION_JSON"

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

# Where the git-seeding init container clones from. It runs in a POD, so loopback is not an address
# it shares with this script — the node's own InternalIP is what reaches the ssh listener above.
NODE_IP=$(kubectl get node "$E2E_NODE" -o jsonpath='{.status.addresses[?(@.type=="InternalIP")].address}')
[ -n "$NODE_IP" ] || fail "node $E2E_NODE has no InternalIP; the seeding init container has nothing to clone from"

# Without this the agent never mounts {pool}/homes and every workspace parks on HomeNotReady
# (lib.rs's run: unset means "no shared home on this node", fail closed) — the real Service the
# cluster's own agents point at (deploy/k3s/agent-daemonset.yaml), reachable here because this
# runs against the real k3s cluster, just with a loopback pool standing in for the node's btrfs.
# WS_REPLICA_SECS: the pull/retire beat, 300 s by default. The orphan-byte sweep and the
# unreferenced-volume collection (and its age floor) both ride it, so every reclaim assertion in
# this script would time out at the default; 20 s here, and every such wait below is at least
# three beats.
log "starting rustic-git-agent against pool $MOUNT as node $E2E_NODE"
NODE_NAME="$E2E_NODE" \
WS_GIT_SSH_HOST="$NODE_IP" \
WS_GIT_SSH_PORT="$SERVER_SSH_PORT" \
WS_REGION="$REGION_ID" \
WS_POOL="$MOUNT" \
WS_HOMES_EXPORT="zerofs.rustic-git-system.svc:/" \
WS_SYNC_SECS="5" \
WS_REPLICA_SECS="20" \
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
# The 60s ceiling is an assertion too: the stop cut and the teardown happen in one pass now, and
# whether a peer has the bytes is a condition read afterwards, not a gate the stop waits on. A stop
# that needs minutes is the old flush gate having come back.
wait_env_stopped() {
  kubectl wait --for=jsonpath='{.status.phase}'=stopped "environment/$1" --timeout=60s \
    || fail "environment $1 never reached phase=stopped"
}

live_dir() { echo "$MOUNT/vol/$1/live"; }

# The worktree of a restored working copy is `{pool}/vol/{volume}/live/{id}` — a RESTORE grafts
# onto the snapshot's own volume, so the path is not `live_dir <id>`. A volume migrated from the
# single-worktree layout keeps its bytes directly under `live/`, so take whichever exists.
worktree_dir() {
  wt_vol=$(kubectl get workspace "$1" -o jsonpath='{.status.volumeRef}' 2>/dev/null)
  [ -n "$wt_vol" ] || wt_vol=$(kubectl get environment "$1" -o jsonpath='{.status.volumeRef}' 2>/dev/null)
  [ -n "$wt_vol" ] || fail "$1 has no status.volumeRef"
  if [ -d "$MOUNT/vol/$wt_vol/live/$1" ]; then echo "$MOUNT/vol/$wt_vol/live/$1"; else echo "$MOUNT/vol/$wt_vol/live"; fi
}

# One `/v1/volumes` row on one line. The rows are flat objects, so splitting on `}` hands `field`
# a single row — `name` is the row's last key, so `field name` cannot pick up `display_name`.
volume_row() {
  curl -fsS "$BASE/v1/volumes" -H "Authorization: Bearer $USER_TOKEN" | tr '}' '\n' | grep "\"name\":\"$1\"" || true
}

# `curl -fsS` throws a refusal's body away, and the 409 TEXTS are the assertion here — this keeps
# the code and the body both, in $DEL_CODE and $DEL_BODY.
api_delete() {
  DEL_BODY=$(curl -sS -X DELETE "$1" -H "Authorization: Bearer $USER_TOKEN" -w '\n%{http_code}')
  DEL_CODE=$(printf '%s' "$DEL_BODY" | tail -1)
  DEL_BODY=$(printf '%s' "$DEL_BODY" | sed '$d')
}

# ---------------------------------------------------------------------------
# Create workspace, wait ready, write into live
# ---------------------------------------------------------------------------
# An EXPLICIT, non-default image, so the frozen-state restore assertion at the end of this script
# is not vacuous: with the default, a restore that ignored `spec.state` entirely would still land on
# the same image and pass. The agent's own pinned tag is the one to use — a TAGGED platform image is
# still `model::is_default_image`, so sshd, the nix profile and zsh all keep working below, while
# `spec.image` differs from the untagged marker a bare restore falls back to.
WS_IMAGE=$(kubectl -n kube-system get daemonset rustic-git-agent \
  -o jsonpath='{.spec.template.spec.containers[0].env[?(@.name=="WS_DEFAULT_IMAGE")].value}')
[ -n "$WS_IMAGE" ] || fail "the agent DaemonSet has no WS_DEFAULT_IMAGE to pin e2e-ws to"
case "$WS_IMAGE" in
  *:*) ;;
  *) fail "WS_DEFAULT_IMAGE ($WS_IMAGE) carries no tag, so it equals the restore fallback and proves nothing" ;;
esac

log "creating workspace on the explicit image $WS_IMAGE"
WS_JSON=$(curl -fsS -X POST "$BASE/v1/workspaces" -H "Authorization: Bearer $USER_TOKEN" \
  -H 'Content-Type: application/json' -d '{"name":"e2e-ws","region":"'"$REGION_ID"'","quota_gb":5,"image":"'"$WS_IMAGE"'"}')
WS_ID=$(echo "$WS_JSON" | field id)
[ -n "$WS_ID" ] || fail "no id in workspace create response: $WS_JSON"
wait_ws_ready "$WS_ID"
WS_NS="ws-$(echo "$USER_NAME" | tr '[:upper:]' '[:lower:]')"

log "checking the workspace pod is running with its live subvolume mounted from the node"
kubectl -n "$WS_NS" wait --for=condition=Ready "pod/$WS_ID" --timeout=120s \
  || fail "no ready pod $WS_ID in $WS_NS after the workspace reached Ready"

# No PV/PVC to check Bound any more — the property that matters is that the pod's hostPath mount
# actually landed on the workspace's own live subvolume, not just that /home/kl exists in the
# container. Write on the host side and read it back through the pod: that only succeeds if the
# hostPath is the same btrfs subvolume `live_dir` names, not an empty dir the kubelet invented.
log "writing a file into the live subvolume"
sudo bash -c "printf 'hello from ws_e2e' > '$(live_dir "$WS_ID")/hello.txt'"
[ -f "$(live_dir "$WS_ID")/hello.txt" ] || fail "write into live did not land"
kubectl -n "$WS_NS" exec "$WS_ID" -- grep -q 'hello from ws_e2e' /home/kl/workspaces/e2e-ws/hello.txt \
  || fail "workspace pod $WS_ID does not see the host's write into its live hostPath"

# ---------------------------------------------------------------------------
# Sync points: the beat (WS_SYNC_SECS, set to 5 above for this run) cuts a transient snapshot
# from a running worktree's moved generation, retaining exactly one per worktree. Two writes a
# beat apart should leave exactly one Ready transient, and it must not be the first one.
# ---------------------------------------------------------------------------
ready_transients() {
  # jsonpath filters on phase=="Ready" only (no && support); the name prefix and spec.transient
  # check are done in awk against the paired field this prints alongside each name.
  # Sorted by creationTimestamp, so a caller taking `head -1`/`tail -1` gets the oldest/newest
  # cut rather than whatever order the API server listed the names in.
  kubectl get snapshots -l "rustic-git.io/volume=$WS_ID" --sort-by=.metadata.creationTimestamp \
    -o jsonpath='{range .items[?(@.status.phase=="Ready")]}{.metadata.name}{" "}{.spec.transient}{"\n"}{end}' \
    2>/dev/null | awk -v p="^sync-$WS_ID-" '$1 ~ p && $2 == "true" {print $1}'
}

log "sync points: writing into the live subvolume and waiting for a Ready sync-$WS_ID-* transient"
sudo bash -c "printf 'sync one' > '$(live_dir "$WS_ID")/sync1.txt'"
SYNC1=""
for i in $(seq 1 60); do
  SYNC1=$(ready_transients | head -1)
  [ -n "$SYNC1" ] && break
  sleep 2
done
[ -n "$SYNC1" ] || fail "no Ready transient sync-$WS_ID-* appeared after writing into the live subvolume"

log "sync points: writing again and waiting for the previous transient to be retained away"
sudo bash -c "printf 'sync two' > '$(live_dir "$WS_ID")/sync2.txt'"
SYNC2=""
COUNT=0
for i in $(seq 1 60); do
  LIST=$(ready_transients)
  COUNT=$(printf '%s\n' "$LIST" | grep -c .)
  SYNC2=$(printf '%s\n' "$LIST" | head -1)
  [ "$COUNT" -eq 1 ] && [ "$SYNC2" != "$SYNC1" ] && break
  sleep 2
done
[ "$COUNT" -eq 1 ] || fail "expected exactly one Ready transient for $WS_ID, found $COUNT: $LIST"
[ "$SYNC2" != "$SYNC1" ] || fail "the previous transient $SYNC1 is still around; retain did not delete it"

# A definition change with NO byte change must cut too (`sync_one` compares the derived
# `spec.state` against the newest sync point's, not just the btrfs generation). Only a real
# generation read can tell the two triggers apart, so this cannot be asserted off-cluster: PATCH
# the packages, touch NOTHING on disk, and require a new sync point carrying the new list. The
# bound is ~50 s, not two 5 s beats: the PATCH triggers a nix build (~28 s cold) that clears
# `pod_ref` while it runs, so the first cut after it can be several beats out.
log "sync points: a packages change with no write must cut a new sync point carrying it"
curl -fsS -X PATCH "$BASE/v1/workspaces/$WS_ID" -H "Authorization: Bearer $USER_TOKEN" \
  -H 'Content-Type: application/json' -d '{"packages":["hello"]}' >/dev/null || fail "PATCH packages"
SYNC3=""
for i in $(seq 1 24); do
  SYNC3=$(ready_transients | tail -1)
  if [ -n "$SYNC3" ] && [ "$SYNC3" != "$SYNC2" ] \
    && [ "$(kubectl get snapshot "$SYNC3" -o jsonpath='{.spec.state.packages[0]}' 2>/dev/null)" = "hello" ]; then
    break
  fi
  SYNC3=""
  sleep 2
done
[ -n "$SYNC3" ] || fail "no new sync point carrying packages=[hello] within ~50 s of the PATCH; the definition-change trigger is not firing"

# ---------------------------------------------------------------------------
# Push: the one mutating verb — `/v1` writes a `Snapshot` CR and the owning node cuts it (btrfs
# snapshot + upload) and marks it Ready, which moves `Volume.status.head`. Nothing is POSTed
# anywhere: /v1/volumes/{id}/history reads the chain of Ready `Snapshot` CRs back, so history must
# grow by exactly one after this.
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
# Packages: spec.packages becomes tools on PATH, with no restart — placed before the
# running-source clone below so that clone's copied spec carries packages too, and the clone
# assertion after it proves the profile is built from the copied spec, not re-derived.
# ---------------------------------------------------------------------------
log "PATCHing packages onto the workspace"
curl -fsS -X PATCH "$BASE/v1/workspaces/$WS_ID" -H "Authorization: Bearer $USER_TOKEN" \
  -H 'Content-Type: application/json' -d '{"packages":["hello"]}' >/dev/null || fail "PATCH packages"
for i in $(seq 1 90); do
  kubectl get workspace "$WS_ID" -o jsonpath='{.status.conditions[?(@.type=="PackagesReady")].reason}' 2>/dev/null | grep -q '^Built$' && break
  sleep 2
  [ "$i" -eq 90 ] && fail "PackagesReady never became Built: $(kubectl get workspace "$WS_ID" -o jsonpath='{.status.conditions}')"
done
kubectl -n "$WS_NS" exec "$WS_ID" -- hello | grep -q 'Hello, world!' || fail "hello is not on PATH in the workspace pod"

log "adding a second package and checking it lands without a pod restart"
POD_UID=$(kubectl -n "$WS_NS" get pod "$WS_ID" -o jsonpath='{.metadata.uid}')
curl -fsS -X PATCH "$BASE/v1/workspaces/$WS_ID" -H "Authorization: Bearer $USER_TOKEN" \
  -H 'Content-Type: application/json' -d '{"packages":["hello","jq"]}' >/dev/null || fail "PATCH packages"
for i in $(seq 1 90); do
  kubectl -n "$WS_NS" exec "$WS_ID" -- jq --version >/dev/null 2>&1 && break
  sleep 2
  [ "$i" -eq 90 ] && fail "jq did not appear after PATCH"
done
[ "$(kubectl -n "$WS_NS" get pod "$WS_ID" -o jsonpath='{.metadata.uid}')" = "$POD_UID" ] \
  || fail "the pod was restarted to add a package; the profile swap must be live"

log "removing a package and checking it drops off PATH"
curl -fsS -X PATCH "$BASE/v1/workspaces/$WS_ID" -H "Authorization: Bearer $USER_TOKEN" \
  -H 'Content-Type: application/json' -d '{"packages":["jq"]}' >/dev/null || fail "PATCH packages"
for i in $(seq 1 90); do
  kubectl -n "$WS_NS" exec "$WS_ID" -- hello >/dev/null 2>&1 || break
  sleep 2
  [ "$i" -eq 90 ] && fail "hello is still on PATH after being removed"
done

log "checking a bad package name is rejected with 422, not silently dropped"
PATCH_CODE=$(curl -sS -o /dev/null -w '%{http_code}' -X PATCH "$BASE/v1/workspaces/$WS_ID" \
  -H "Authorization: Bearer $USER_TOKEN" -H 'Content-Type: application/json' \
  -d '{"packages":["$(id)"]}')
[ "$PATCH_CODE" = "422" ] || fail "a bad package name must be a 422, got $PATCH_CODE"

# ---------------------------------------------------------------------------
# Clone (running-source path): the source's pod is still up, so the controller's clone arm
# snapshots the live subvolume in place (`Engine::clone_local_snapshot`) instead of the
# registry-history path used above — same pushed content either way, since the source has
# nothing unpushed left after the push above.
# ---------------------------------------------------------------------------
log "cloning workspace (running-source path)"
CLONE_JSON=$(curl -fsS -X POST "$BASE/v1/workspaces/$WS_ID/clone" -H "Authorization: Bearer $USER_TOKEN" \
  -H 'Content-Type: application/json' -d '{"name":"e2e-ws-clone"}')
CLONE_ID=$(echo "$CLONE_JSON" | field id)
[ -n "$CLONE_ID" ] || fail "no id in clone response: $CLONE_JSON"
wait_ws_ready "$CLONE_ID"
[ -f "$(live_dir "$CLONE_ID")/hello.txt" ] || fail "cloned workspace is missing the file written into the source"

# ---------------------------------------------------------------------------
# SSH: `kl` mints a session against the api (a session JWT works as its bearer token, same as the
# CLI's own long-lived login) and tunnels through the gateway — this VM has no Cloudflare edge in
# front of it, so `KL_GATEWAY_OVERRIDE` (hidden, tests-only — see bins/kl/src/proxy.rs) swaps the
# `wss://ws-{region}.khost.dev` origin the api hands back for the gateway's own k3s Service,
# keeping the `/tunnel/{id}` path the api minted. Needs PackagesReady=Built (asserted above) so the
# pod is actually running sshd on the default image, and the running-source clone above so a SECOND
# workspace pod exists to probe the NetworkPolicy from. Exercises the whole path once — session
# mint, pinned host key, ProxyCommand, the gateway's own auth, the NetworkPolicy hole — not just
# the api route in isolation.
# ---------------------------------------------------------------------------
log "registering an ssh key for $USER_NAME"
ssh-keygen -q -t ed25519 -N '' -f "$TMPD/id"
KEY_LINE=$(cat "$TMPD/id.pub")
curl -fsS -X POST "$BASE/v1/keys" -H "Authorization: Bearer $USER_TOKEN" \
  -H 'Content-Type: application/json' \
  -d "{\"owner\":\"$USER_NAME\",\"name\":\"e2e-ssh\",\"key\":\"$KEY_LINE\"}" >/dev/null \
  || fail "POST /v1/keys failed"

# ssh-agent, not `-i`: `kl ws ssh <target> -- <args>` builds ssh's argv as
# `[options...] kl@<id> <args...>` — anything after the target lands AFTER the hostname, where
# ssh no longer reads it as an option and `-i path` would be swallowed into the remote command
# instead. An agent holding the key sidesteps kl's argv shape entirely, which is also what a real
# user's ssh-agent already does.
eval "$(ssh-agent -s)" >/dev/null
ssh-add "$TMPD/id" >/dev/null 2>&1 || fail "ssh-add failed"

log "pointing kl at the local api and the gateway's in-cluster Service"
export KL_CONFIG_DIR="$TMPD/kl-config"
mkdir -p "$KL_CONFIG_DIR"
GATEWAY_IP=$(kubectl get svc rustic-git-gateway -n rustic-git-system -o jsonpath='{.spec.clusterIP}')
[ -n "$GATEWAY_IP" ] || fail "rustic-git-gateway Service has no clusterIP"
export KL_GATEWAY_OVERRIDE="ws://$GATEWAY_IP:8080"
cat > "$KL_CONFIG_DIR/config.json" <<EOF
{"api":"$BASE","token":"$USER_TOKEN","expires_at":"2099-01-01T00:00:00Z","username":"$USER_NAME"}
EOF
chmod 600 "$KL_CONFIG_DIR/config.json"

log "kl ws ssh into the workspace"
"$KL_BIN" ws ssh "$WS_ID" -- true || fail "kl ws ssh $WS_ID -- true did not exit 0"

log "checking a second user's session mint is refused"
OTHER_TOKEN=$(mint_jwt "ws-e2e-other@example.test" "E2E Other" "e2eother")
OTHER_CODE=$(curl -sS -o /dev/null -w '%{http_code}' -X POST "$BASE/v1/workspaces/$WS_ID/ssh-session" \
  -H "Authorization: Bearer $OTHER_TOKEN")
[ "$OTHER_CODE" = "404" ] || fail "a second user's session mint must 404, got $OTHER_CODE"

log "checking the registered key landed in the pod's authorized_keys"
kubectl -n "$WS_NS" exec "$WS_ID" -- ls /home/kl/.ssh/authorized_keys >/dev/null \
  || fail "no /home/kl/.ssh/authorized_keys in the workspace pod"

# The negative half: a peer workspace pod is not `app=rustic-git-gateway` in `rustic-git-system`, so the
# default-deny-plus-gateway-hole NetworkPolicy must refuse it on port 22 — `kl` above only proved
# the gateway path works, not that the direct path is actually closed. `|| true` is load-bearing
# under `set -e`, same as the environment default-deny assertion above: a correctly refused `nc` is
# the pass, not a script failure.
log "checking the NetworkPolicy blocks a peer pod from reaching sshd directly"
WS_POD_IP=$(kubectl -n "$WS_NS" get pod "$WS_ID" -o jsonpath='{.status.podIP}')
[ -n "$WS_POD_IP" ] || fail "workspace pod $WS_ID has no podIP"
# exit 3 is our own signal for "nc isn't even in the image" so it can't be confused with nc's own
# exit codes (1 on refusal, 124-ish on timeout) — without it a missing `nc` and a correctly refused
# connection both exit non-zero and this would silently "pass" on a check that never ran.
NC_STATUS=0
kubectl -n "$WS_NS" exec "$CLONE_ID" -- sh -c "command -v nc >/dev/null || exit 3; nc -zw2 $WS_POD_IP 22" \
  || NC_STATUS=$?
if [ "$NC_STATUS" -eq 3 ]; then
  fail "nc not in the workspace image"
elif [ "$NC_STATUS" -eq 0 ]; then
  fail "a peer workspace pod reached sshd on port 22 directly; the gateway-only NetworkPolicy is not enforced"
fi
kubectl -n "$WS_NS" exec "$CLONE_ID" -- jq --version >/dev/null || fail "the clone did not build its profile from the copied spec"

# ---------------------------------------------------------------------------
# Persistent home: one region-shared NFS export (ZeroFS, deploy/k3s/zerofs.yaml), not a per-node
# btrfs subvolume any more. There is no home Volume, no OwnerBinding-owned CR, no push/pull and no
# per-owner node pin — every pod hostPaths the SAME export at /home/kl, so a write is visible from
# any node the instant it lands, and an owner's workspaces can now be claimed by any node whose
# VolumeReplica is Synced rather than only the one node that used to hold their btrfs home (see
# deploy/k3s/README.md, "Shared home"). zsh reads `$ZDOTDIR/.zshrc`, not `~/.zshrc` — the prelude
# seeds the former, so that is the file a person actually edits and the one whose survival matters.
# ---------------------------------------------------------------------------
ZSHRC=/home/kl/.config/zsh/.zshrc
log "writing $ZSHRC in one workspace and reading it from another pod, with no push in between"
kubectl -n "$WS_NS" exec "$WS_ID" -- sh -c "echo 'export WS_E2E_HOME=1' >> $ZSHRC" \
  || fail "could not append to $ZSHRC in $WS_ID"
kubectl -n "$WS_NS" exec "$CLONE_ID" -- grep -q 'WS_E2E_HOME=1' "$ZSHRC" \
  || fail "a second workspace pod does not see the shared NFS home's .zshrc"

log "stopping and restarting the workspace: the home lives on the export, not the pod"
curl -fsS -X POST "$BASE/v1/workspaces/$WS_ID/stop" -H "Authorization: Bearer $USER_TOKEN" >/dev/null
# A stop is now seconds, not minutes: the cut turns Ready and the pod goes in the same pass, and
# the replica wait moved into placement. 60s, not 300s, is the assertion — a stop that takes
# longer than that is the flush gate having come back.
kubectl wait --for=jsonpath='{.status.phase}'=stopped "workspace/$WS_ID" --timeout=60s \
  || fail "workspace $WS_ID never reached phase=stopped"
kubectl -n "$WS_NS" get "pod/$WS_ID" >/dev/null 2>&1 && fail "the pod is still there after the stop"

# `FlushUnreplicated` is gone entirely: a stop is never "unreplicated", only not-yet-replicated,
# and that is the `Replicated` condition's job for as long as it is true.
kubectl get "workspace/$WS_ID" -o json | grep -q FlushUnreplicated \
  && fail "FlushUnreplicated is gone; a stop no longer records a one-shot flush verdict"

# And the condition that replaced it is there, with one of the two reasons and nothing else.
REPL=$(kubectl get "workspace/$WS_ID" -o jsonpath='{.status.conditions[?(@.type=="Replicated")].reason}')
case "$REPL" in
  Replicated|AwaitingReplica) : ;;
  *) fail "expected a Replicated condition on the stopped workspace, got '$REPL'" ;;
esac
curl -fsS -X POST "$BASE/v1/workspaces/$WS_ID/start" -H "Authorization: Bearer $USER_TOKEN" >/dev/null
wait_ws_ready "$WS_ID"
kubectl -n "$WS_NS" wait --for=condition=Ready "pod/$WS_ID" --timeout=120s || fail "pod $WS_ID did not come back"
kubectl -n "$WS_NS" exec "$WS_ID" -- grep -q 'WS_E2E_HOME=1' "$ZSHRC" || fail "the home's .zshrc did not survive a stop/start"

# The thing this whole change buys: an owner's home is no longer pinned to one node. Placement is
# a controller-side claim race (bins/agent/src/claim.rs's bootstrap case: a fresh, UNPLACED
# workspace is claimable by any node), not something kube-scheduler decides — a cordon/taint/label
# on this node cannot steer it, and forcing the race by pausing this script's own agent process
# was tried and rejected: that agent runs as root under sudo holding the loopback pool mount, and
# a SIGKILL of this script (or a CI timeout) bypasses the trap, leaving a stopped root process
# behind with nothing to resume it. So this makes no attempt to steer where the claim lands —
# it just proves the property that matters holds WHEREVER it lands, which is true on both a
# single-node and a multi-node cluster and cannot strand anything.
log "checking a freshly claimed workspace sees the shared home over NFS, whichever node claims it"
OTHER_JSON=$(curl -fsS -X POST "$BASE/v1/workspaces" -H "Authorization: Bearer $USER_TOKEN" \
  -H 'Content-Type: application/json' -d '{"name":"e2e-ws-other-node","region":"'"$REGION_ID"'","quota_gb":5}')
OTHER_NODE_WS_ID=$(echo "$OTHER_JSON" | field id)
[ -n "$OTHER_NODE_WS_ID" ] || fail "no id in other-node workspace create response: $OTHER_JSON"
CLAIMED_ON=""
for i in $(seq 1 60); do
  CLAIMED_ON=$(kubectl get workspace "$OTHER_NODE_WS_ID" -o jsonpath='{.status.nodeName}' 2>/dev/null)
  [ -n "$CLAIMED_ON" ] && break
  sleep 2
done
[ -n "$CLAIMED_ON" ] || fail "e2e-ws-other-node was never claimed by any node"
log "e2e-ws-other-node claimed on $CLAIMED_ON (this script's own node is $E2E_NODE)"
wait_ws_ready "$OTHER_NODE_WS_ID"
kubectl -n "$WS_NS" exec "$OTHER_NODE_WS_ID" -- grep -q 'WS_E2E_HOME=1' "$ZSHRC" \
  || fail "a freshly claimed workspace (node $CLAIMED_ON) cannot see the home written earlier on $E2E_NODE — the NFS export is not actually shared"
curl -fsS -X DELETE "$BASE/v1/workspaces/$OTHER_NODE_WS_ID" -H "Authorization: Bearer $USER_TOKEN" >/dev/null
wait_ws_gone "$OTHER_NODE_WS_ID"
OTHER_NODE_WS_ID=""

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
# A real git repository on the server tier, and the owner's platform key — everything a seeded
# workspace clones FROM. Both are written straight through the `admin` subcommands against the
# shared file:// store, because the HTTP routes that would otherwise do it (/v1/repos, /v1/tokens,
# /v1/platform-key) all need the Mongo directory this script does not run.
# ---------------------------------------------------------------------------
E2E_REPO="e2e-seed"
admin() { RUSTIC_GIT_CACHE_DIR="$TMPD/cache-admin" RUSTIC_GIT_S3_URL="$STORE_URL" "$SERVER_BIN" admin "$@"; }

# ORDER: every write below must happen before anything probes that user, repo, token or
# fingerprint. The server's credential lookup caches MISSES for 60s per process, so a probe that
# runs first pins "no such thing" into the running server and the seeded workspace then fails
# authentication for a minute with nothing in the logs explaining why.

log "creating repository $USER_NAME/$E2E_REPO and a push token"
admin create-repo "$USER_NAME/$E2E_REPO" >/dev/null || fail "admin create-repo failed"
PUSH_TOKEN=$(admin add-token "$USER_NAME") || fail "admin add-token failed"
[ -n "$PUSH_TOKEN" ] || fail "admin add-token printed no token"

log "pushing two commits with the real git binary"
SEED_REPO_DIR="$TMPD/seed-repo"
git init -q -b main "$SEED_REPO_DIR"
# Pods have no git identity and neither does a CI runner: every commit-writing call sets its own.
git -C "$SEED_REPO_DIR" config user.email "$USER_EMAIL"
git -C "$SEED_REPO_DIR" config user.name "E2E User"
printf 'seeded by ws_e2e\n' > "$SEED_REPO_DIR/README.md"
git -C "$SEED_REPO_DIR" add README.md
git -C "$SEED_REPO_DIR" commit -q -m "first"
printf 'second\n' >> "$SEED_REPO_DIR/README.md"
git -C "$SEED_REPO_DIR" commit -qam "second"
git -C "$SEED_REPO_DIR" push -q \
  "http://$USER_NAME:$PUSH_TOKEN@$SERVER_HTTP_ADDR/$USER_NAME/$E2E_REPO.git" main \
  || fail "git push to the server tier failed"

# The key the workspace pod clones with. `/v1/platform-key` is how a real deployment generates this
# (see crates/api/src/credentials.rs), and it writes exactly these two things: the fingerprint,
# registered against the owner, and the private key at the object-store key the api reads to build
# the namespace Secret. Writing them here is writing the same two objects, not a second mechanism.
log "installing a platform key for $USER_NAME"
ssh-keygen -q -t ed25519 -N '' -C "ws-e2e" -f "$TMPD/platform_key"
admin add-key "$USER_NAME" "$TMPD/platform_key.pub" || fail "admin add-key failed"
mkdir -p "$TMPD/store/auth/userkey"
install -m 600 "$TMPD/platform_key" "$TMPD/store/auth/userkey/$USER_NAME"

# ---------------------------------------------------------------------------
# The seeded workspace: the 27 Aug bug, as a test. "Open in a workspace" produced a pod stuck on
# `path … does not exist` forever — the workspace made its pod before its Volume had made the disk,
# and the git-seeding path was never wired end to end anyway (the API named a token Secret nobody
# wrote and the agent had no permission to read one). Both halves are asserted here, on a FIRST
# workspace for this repository.
# ---------------------------------------------------------------------------
log "creating a workspace seeded from a platform repository"
SEED_JSON=$(curl -fsS -X POST "$BASE/v1/workspaces" -H "Authorization: Bearer $USER_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"name":"e2e-seeded","region":"'"$REGION_ID"'","quota_gb":5,"repo":"'"$USER_NAME"'/'"$E2E_REPO"'","branch":"main"}')
SEED_ID=$(echo "$SEED_JSON" | field id)
[ -n "$SEED_ID" ] || fail "no id in seeded workspace create response: $SEED_JSON"

log "checking the API wrote ONE object and named no node"
[ -z "$(kubectl get workspace "$SEED_ID" -o jsonpath='{.spec.nodeName}' 2>/dev/null)" ] \
  || fail "the API named a node; placement is a fact the controllers establish"
[ -z "$(kubectl get workspace "$SEED_ID" -o jsonpath='{.spec.volumeRef}' 2>/dev/null)" ] \
  || fail "the API named a child; volumeRef in spec was a wish about a fact"

log "waiting for the claim, then for the workspace"
kubectl wait --for=condition=Placed "workspace/$SEED_ID" --timeout=120s \
  || fail "workspace $SEED_ID was never claimed by any node"
# A list, purely for its side effect: the create only waits 5s for `Placed` before giving up on
# installing the owner's key Secret, and the list is what re-installs it when it is absent. A
# seeded pod cannot start without that Secret, so losing that race would strand it in Pending.
curl -fsS "$BASE/v1/workspaces" -H "Authorization: Bearer $USER_TOKEN" >/dev/null
wait_ws_ready "$SEED_ID"

log "checking the Volume is a child that dies with its parent"
kubectl get volume "$SEED_ID" -o jsonpath='{.metadata.ownerReferences[0].kind}' | grep -qx Workspace \
  || fail "the Volume has no controlling Workspace ownerReference"
[ "$(kubectl get workspace "$SEED_ID" -o jsonpath='{.status.volumeRef}')" = "$SEED_ID" ] \
  || fail "status.volumeRef does not report the child"

log "checking the init container actually cloned the repository into the workspace"
kubectl -n "$WS_NS" exec "$SEED_ID" -c workspace -- sh -c 'ls -a /home/kl/workspaces/e2e-seeded/.git >/dev/null' \
  || fail "no .git in ~/workspaces/<name>: the git-seeding init container did not run or did not clone"
# The working tree, read from the host this time: a `.git` directory proves a clone was attempted,
# the pushed file proves it was THIS repository's content that landed.
# sudo: the init container clones as root, so the tree is not readable as this user.
sudo grep -q "seeded by ws_e2e" "$(live_dir "$SEED_ID")/README.md" \
  || fail "the seeded workspace does not carry the pushed repository's content"

log "pushing the seeded workspace and reading its history back from SnapshotRequests"
SEED_BEFORE=$(id_count "$(curl -fsS "$BASE/v1/volumes/$SEED_ID/history" -H "Authorization: Bearer $USER_TOKEN")")
curl -fsS -X POST "$BASE/v1/workspaces/$SEED_ID/push" -H "Authorization: Bearer $USER_TOKEN" \
  -H 'Content-Type: application/json' -d '{"message":"seeded push"}' >/dev/null
SEED_AFTER="$SEED_BEFORE"
SEED_HISTORY=""
for i in $(seq 1 30); do
  SEED_HISTORY=$(curl -fsS "$BASE/v1/volumes/$SEED_ID/history" -H "Authorization: Bearer $USER_TOKEN")
  SEED_AFTER=$(id_count "$SEED_HISTORY")
  [ "$SEED_AFTER" -gt "$SEED_BEFORE" ] && break
  sleep 2
done
[ "$SEED_AFTER" -eq "$((SEED_BEFORE + 1))" ] \
  || fail "history did not grow by exactly one after push ($SEED_BEFORE -> $SEED_AFTER)"
echo "$SEED_HISTORY" | grep -q '"message":"seeded push"' || fail "push message missing: $SEED_HISTORY"
echo "$SEED_HISTORY" | grep -q '"created_at":"' || fail "history lost the created_at the web reads"
kubectl get snapshotrequests -l "rustic-git.io/volume=$SEED_ID" -o name | grep -q . \
  || fail "a push wrote no SnapshotRequest"

log "deleting the seeded workspace with ONE call and letting GC take the child"
curl -fsS -X DELETE "$BASE/v1/workspaces/$SEED_ID" -H "Authorization: Bearer $USER_TOKEN" >/dev/null
wait_ws_gone "$SEED_ID"
kubectl wait --for=delete "volume/$SEED_ID" --timeout=300s \
  || fail "the Volume outlived its Workspace: garbage collection did not follow the ownerReference"
SEED_ID=""

# ---------------------------------------------------------------------------
# Environment: an environment owns exactly ONE subvolume of its own (never a mounted
# workspace); every declared volume is a folder inside it (live/volumes/{name}), reached as a
# subPath on the env's one `live-{id}` claim. The service is named `db` and mounts volume "data":
# it writes a marker file into it and then serves that same folder over port 27017, which is what
# makes the connectivity assertions below non-vacuous — a port nobody listens on would "fail to
# connect" for every reason, including the wrong one. `env stop` (EnvDown) always pushes that one
# subvolume atomically before tearing the statefulsets down — see bins/agent/src/lib.rs — so,
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
kubectl -n "$ENV_NS" exec statefulset/db -- wget -qO- 127.0.0.1:27017/marker.txt \
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
kubectl -n "$ENV_NS" delete statefulset db --wait=true >/dev/null
# `rollout status` on an object that does not exist yet is an error, not a wait, so the recreate is
# waited for first and only then the rollout.
for i in $(seq 1 120); do
  kubectl -n "$ENV_NS" get statefulset db >/dev/null 2>&1 && break
  sleep 1
  [ "$i" -eq 120 ] && fail "controller did not converge: statefulset/db never came back"
done
kubectl -n "$ENV_NS" rollout status statefulset/db --timeout=120s \
  || fail "controller recreated statefulset/db but it never became available"

# --- assertion 4: attachment resolves a bare service name, with no pod restart -----------------
# $WS_ID has been running since its creation above (stopped/started once already, then left up) —
# reusing it rather than creating a fresh pod is the whole point: dnsConfig is immutable on a
# running pod, so if attach needed a restart to take effect this would be the phase that catches it.
log "attach: the workspace resolves an environment service by bare name"
curl -fsS -X POST "$BASE/v1/workspaces/$WS_ID/attach" -H "Authorization: Bearer $USER_TOKEN" \
  -H 'Content-Type: application/json' -d "{\"environment\":\"$ENV_ID\"}" >/dev/null
for i in $(seq 1 30); do
  kubectl -n "$WS_NS" exec "$WS_ID" -- getent hosts db >/dev/null 2>&1 && break
  sleep 2
  [ "$i" -eq 30 ] && fail "attached environment's service does not resolve by bare name"
done

log "detach: it stops resolving"
curl -fsS -X POST "$BASE/v1/workspaces/$WS_ID/detach" -H "Authorization: Bearer $USER_TOKEN" \
  -H 'Content-Type: application/json' -d '{}' >/dev/null
for i in $(seq 1 30); do
  kubectl -n "$WS_NS" exec "$WS_ID" -- getent hosts db >/dev/null 2>&1 || break
  sleep 2
  [ "$i" -eq 30 ] && fail "service still resolves by bare name after detach"
done
# The loop above also breaks on a broken/dead exec, which would silently pass a detach that never
# happened. Prove the exec path still works, using the FQDN (never gated by attachment) as the
# control: it must still resolve while the bare name (just proven to not resolve) stays refused.
kubectl -n "$WS_NS" exec "$WS_ID" -- getent hosts "db.$ENV_NS" >/dev/null \
  || fail "the exec itself is broken; the detach assertion proves nothing"

# Pushed while it is still RUNNING: only the node running the worktree fulfils a cut, so a push
# to a stopped environment would sit Working forever. Everything the twin below asserts hangs off
# this one snapshot.
log "durable snapshots (environment): pushing the running environment"
curl -fsS -X POST "$BASE/v1/environments/$ENV_ID/push" -H "Authorization: Bearer $USER_TOKEN" \
  -H 'Content-Type: application/json' -d '{"message":"env snapshot"}' >/dev/null
ENV_SNAP=""
for i in $(seq 1 60); do
  # Sorted, so `tail -1` is the cut this push just made and not the create-time one.
  ENV_SNAP=$(kubectl get snapshots -l "rustic-git.io/volume=$ENV_ID" --sort-by=.metadata.creationTimestamp \
    -o jsonpath='{range .items[?(@.status.phase=="Ready")]}{.metadata.name}{" "}{.spec.transient}{"\n"}{end}' \
    2>/dev/null | awk '$2 == "false" {print $1}' | tail -1)
  [ -n "$ENV_SNAP" ] && break
  sleep 2
done
[ -n "$ENV_SNAP" ] || fail "no Ready snapshot on the environment's volume $ENV_ID after push"
ENV_FROZEN_IMAGE=$(kubectl get snapshot "$ENV_SNAP" -o jsonpath='{.spec.state.services[0].image}')
[ "$ENV_FROZEN_IMAGE" = "busybox:1.36" ] || fail "the environment snapshot froze no service list: '$ENV_FROZEN_IMAGE'"
ENV_VOLUME=$(kubectl get environment "$ENV_ID" -o jsonpath='{.status.volumeRef}')
[ -n "$ENV_VOLUME" ] || fail "environment $ENV_ID has no status.volumeRef"

log "stopping environment (this pushes the env's own subvolume)"
curl -fsS -X POST "$BASE/v1/environments/$ENV_ID/stop" -H "Authorization: Bearer $USER_TOKEN" >/dev/null
wait_env_stopped "$ENV_ID"

# Same two assertions as the workspace stop: no one-shot flush verdict, and the `Replicated`
# condition in its place.
kubectl get "environment/$ENV_ID" -o json | grep -q FlushUnreplicated \
  && fail "FlushUnreplicated is gone; a stop no longer records a one-shot flush verdict"
REPL=$(kubectl get "environment/$ENV_ID" -o jsonpath='{.status.conditions[?(@.type=="Replicated")].reason}')
case "$REPL" in
  Replicated|AwaitingReplica) : ;;
  *) fail "expected a Replicated condition on the stopped environment, got '$REPL'" ;;
esac

log "checking the env's volume registry history is non-empty after stop"
for i in $(seq 1 30); do
  ENV_HISTORY=$(curl -fsS "$BASE/v1/volumes/$ENV_ID/history" -H "Authorization: Bearer $USER_TOKEN")
  [ "$ENV_HISTORY" != "[]" ] && break
  sleep 1
  [ "$i" -eq 30 ] && fail "env volume history is still empty after stop: $ENV_HISTORY"
done

# ---------------------------------------------------------------------------
# Durable snapshots, the environment twin: the same delete -> detached -> restore -> collect
# round trip the workspace makes below, with the services coming from the snapshot rather than
# from the request.
# ---------------------------------------------------------------------------
log "durable snapshots (environment): deleting the environment, snapshot and all"
curl -fsS -X DELETE "$BASE/v1/environments/$ENV_ID" -H "Authorization: Bearer $USER_TOKEN" >/dev/null
kubectl wait --for=delete "environment/$ENV_ID" --timeout=300s || fail "environment $ENV_ID still present after delete"
DELETED_ENV_ID="$ENV_ID"
ENV_ID=""

log "durable snapshots (environment): the volume is still listed, detached, with its snapshots"
ENV_ROW=""
for i in $(seq 1 30); do
  ENV_ROW=$(volume_row "$ENV_VOLUME")
  [ -n "$ENV_ROW" ] && [ "$(echo "$ENV_ROW" | field deleted)" = "true" ] && break
  sleep 2
done
[ -n "$ENV_ROW" ] || fail "the deleted environment's volume $ENV_VOLUME is not in /v1/volumes any more"
[ "$(echo "$ENV_ROW" | field snapshots)" -ge 1 ] || fail "detached volume $ENV_VOLUME lists no snapshots: $ENV_ROW"
[ "$(echo "$ENV_ROW" | field kind)" = "environment" ] || fail "the detached row is not an environment: $ENV_ROW"

# No `services` in the body: an environment restored from a snapshot runs what the snapshot froze.
log "durable snapshots (environment): restoring runs the snapshot's own services"
ENV_RESTORE_JSON=$(curl -fsS -X POST "$BASE/v1/environments/restore" -H "Authorization: Bearer $USER_TOKEN" \
  -H 'Content-Type: application/json' -d '{"name":"e2e-env-restore","snapshot_id":"'"$ENV_SNAP"'"}')
ENV_ID=$(echo "$ENV_RESTORE_JSON" | field id)
[ -n "$ENV_ID" ] || fail "no id in environment restore response: $ENV_RESTORE_JSON"
wait_env_ready "$ENV_ID"
[ "$(kubectl get environment "$ENV_ID" -o jsonpath='{.spec.services[0].image}')" = "$ENV_FROZEN_IMAGE" ] \
  || fail "the restored environment's services did not come from the snapshot"
[ "$(kubectl get environment "$ENV_ID" -o jsonpath='{.status.volumeRef}')" = "$ENV_VOLUME" ] \
  || fail "the restored environment is not a worktree of the snapshot's volume $ENV_VOLUME"
[ -f "$(worktree_dir "$ENV_ID")/volumes/data/marker.txt" ] || fail "the restored environment is missing the pushed marker"

log "durable snapshots (environment): deleting the restored environment and its snapshots collects the volume"
curl -fsS -X DELETE "$BASE/v1/environments/$ENV_ID" -H "Authorization: Bearer $USER_TOKEN" >/dev/null
kubectl wait --for=delete "environment/$ENV_ID" --timeout=300s || fail "restored environment $ENV_ID still present after delete"
ENV_ID=""
kubectl get volume "$ENV_VOLUME" >/dev/null 2>&1 || fail "the Volume CR $ENV_VOLUME went with its second working copy; its snapshots should have kept it"
for sn in $(kubectl get snapshots -l "rustic-git.io/volume=$ENV_VOLUME" \
  -o jsonpath='{range .items[?(@.spec.transient==false)]}{.metadata.name}{"\n"}{end}' 2>/dev/null); do
  api_delete "$BASE/v1/volumes/$ENV_VOLUME/snapshots/$sn"
  [ "$DEL_CODE" = 204 ] || fail "deleting environment snapshot $sn answered $DEL_CODE: $DEL_BODY"
done
kubectl wait --for=delete "volume/$ENV_VOLUME" --timeout=120s \
  || fail "the Volume CR $ENV_VOLUME survived the delete of its last snapshot"
for i in $(seq 1 60); do
  [ ! -e "$MOUNT/vol/$ENV_VOLUME" ] && break
  sleep 2
done
[ ! -e "$MOUNT/vol/$ENV_VOLUME" ] || fail "the volume tree $MOUNT/vol/$ENV_VOLUME is still on the node after its last snapshot was deleted"

# ---------------------------------------------------------------------------
# Task 7b: the commit model itself, minimal. The commit model is the only model now (WS_COMMIT_MODEL
# was deleted in 44b19fc), so this section always runs. Reuses $WS_ID: it has already been pushed
# once above, so that push landed as a Snapshot CR, not a SnapshotRequest.
# ---------------------------------------------------------------------------
log "commit model: push landed as a Ready Snapshot CR"
SNAP_NAME=""
for i in $(seq 1 30); do
  SNAP_NAME=$(kubectl get snapshots -l "rustic-git.io/volume=$WS_ID" \
    -o jsonpath='{.items[?(@.status.phase=="Ready")].metadata.name}' 2>/dev/null | awk '{print $1}')
  [ -n "$SNAP_NAME" ] && break
  sleep 2
done
[ -n "$SNAP_NAME" ] || fail "no Ready Snapshot CR for volume $WS_ID after push"

log "commit model: clone from head"
CM_CLONE_JSON=$(curl -fsS -X POST "$BASE/v1/workspaces/$WS_ID/clone" -H "Authorization: Bearer $USER_TOKEN" \
  -H 'Content-Type: application/json' -d '{"name":"e2e-ws-cm-clone"}')
CM_CLONE_ID=$(echo "$CM_CLONE_JSON" | field id)
[ -n "$CM_CLONE_ID" ] || fail "no id in commit-model clone response: $CM_CLONE_JSON"
wait_ws_ready "$CM_CLONE_ID"

log "commit model: restore to a named commit"
CM_RESTORE_JSON=$(curl -fsS -X POST "$BASE/v1/workspaces/restore" -H "Authorization: Bearer $USER_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"name":"e2e-ws-cm-restore","snapshot_id":"'"$SNAP_NAME"'","src_workspace":"'"$WS_ID"'"}')
CM_RESTORE_ID=$(echo "$CM_RESTORE_JSON" | field id)
[ -n "$CM_RESTORE_ID" ] || fail "no id in commit-model restore response: $CM_RESTORE_JSON"
wait_ws_ready "$CM_RESTORE_ID"

log "commit model: a replica appears on a second node, if this cluster has one"
SECOND_NODE=$(kubectl get nodes -o jsonpath='{.items[?(@.metadata.name!="'"$E2E_NODE"'")].metadata.name}' 2>/dev/null | awk '{print $1}')
if [ -n "$SECOND_NODE" ]; then
  REPLICA_SYNCED=""
  for i in $(seq 1 60); do
    REPLICA_SYNCED=$(kubectl get volumereplicas -l "rustic-git.io/volume=$WS_ID" \
      -o jsonpath='{.items[?(@.status.phase=="Synced")].spec.node}' 2>/dev/null | tr ' ' '\n' | grep -vx "$E2E_NODE" | head -1)
    [ -n "$REPLICA_SYNCED" ] && break
    sleep 2
  done
  [ -n "$REPLICA_SYNCED" ] || fail "no Synced VolumeReplica for $WS_ID on a node other than $E2E_NODE"
else
  log "single-node cluster: skipping the second-node replica check"
fi

# ---------------------------------------------------------------------------
# Restore (explicit snapshot), extended: the snapshot froze the source's definition at push time
# (`spec.state`), so a restore must come back with THAT image even with the source gone — not the
# default image a bare restore would otherwise fall back to. $WS_ID is not needed by anything
# after this point, so it is safe to delete here.
# ---------------------------------------------------------------------------
# Both of those are worktrees of $WS_ID's OWN volume (a clone and a restore share it), so the
# volume stays attached — and its bytes stay on the node — until they are gone too. The durable
# block below asserts a DETACHED volume and then its collection, so they go first.
log "deleting the clone and the restore that share the source's volume"
curl -fsS -X DELETE "$BASE/v1/workspaces/$CM_CLONE_ID" -H "Authorization: Bearer $USER_TOKEN" >/dev/null
wait_ws_gone "$CM_CLONE_ID"
CM_CLONE_ID=""
curl -fsS -X DELETE "$BASE/v1/workspaces/$CM_RESTORE_ID" -H "Authorization: Bearer $USER_TOKEN" >/dev/null
wait_ws_gone "$CM_RESTORE_ID"
CM_RESTORE_ID=""

log "restore (explicit snapshot): asserting the restored workspace keeps the source's frozen image after the source is deleted"
SRC_IMAGE=$(kubectl get workspace "$WS_ID" -o jsonpath='{.spec.image}')
SRC_PACKAGES=$(kubectl get snapshot "$SNAP_NAME" -o jsonpath='{.spec.state.packages}')
# The volume outlives the workspace from here on, and it is what every assertion below names.
WS_VOLUME=$(kubectl get workspace "$WS_ID" -o jsonpath='{.status.volumeRef}')
[ -n "$WS_VOLUME" ] || fail "workspace $WS_ID has no status.volumeRef"
curl -fsS -X DELETE "$BASE/v1/workspaces/$WS_ID" -H "Authorization: Bearer $USER_TOKEN" >/dev/null
wait_ws_gone "$WS_ID"

# Durable snapshots: the push outlived the workspace. The Volume is DETACHED — no working copy
# owns it any more — and it is still listed, with its snapshots, which is the only reason the
# restore below has anything to graft onto.
log "durable snapshots: the deleted workspace's volume is still listed, detached, with its snapshots"
DETACHED_ROW=""
for i in $(seq 1 30); do
  DETACHED_ROW=$(volume_row "$WS_VOLUME")
  [ -n "$DETACHED_ROW" ] && [ "$(echo "$DETACHED_ROW" | field deleted)" = "true" ] && break
  sleep 2
done
[ -n "$DETACHED_ROW" ] || fail "the deleted workspace's volume $WS_VOLUME is not in /v1/volumes any more; its snapshots went with it"
[ "$(echo "$DETACHED_ROW" | field deleted)" = "true" ] || fail "volume $WS_VOLUME is not marked as having no working copy: $DETACHED_ROW"
[ "$(echo "$DETACHED_ROW" | field snapshots)" -ge 1 ] || fail "detached volume $WS_VOLUME lists no snapshots: $DETACHED_ROW"
kubectl get volume "$WS_VOLUME" >/dev/null 2>&1 || fail "the Volume CR $WS_VOLUME was collected even though a snapshot references it"
SNAP_STATE_RESTORE_JSON=$(curl -fsS -X POST "$BASE/v1/workspaces/restore" -H "Authorization: Bearer $USER_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"name":"e2e-restore-state","snapshot_id":"'"$SNAP_NAME"'"}')
SNAP_STATE_RESTORE_ID=$(echo "$SNAP_STATE_RESTORE_JSON" | field id)
[ -n "$SNAP_STATE_RESTORE_ID" ] || fail "no id in state-restore response: $SNAP_STATE_RESTORE_JSON"
WS_ID=""
wait_ws_ready "$SNAP_STATE_RESTORE_ID"
RESTORED_IMAGE=$(kubectl get workspace "$SNAP_STATE_RESTORE_ID" -o jsonpath='{.spec.image}')
FROZEN_IMAGE=$(kubectl get snapshot "$SNAP_NAME" -o jsonpath='{.spec.state.image}')
[ "$RESTORED_IMAGE" = "$SRC_IMAGE" ] || fail "restore did not take the snapshot's frozen image: $RESTORED_IMAGE != $SRC_IMAGE"
[ "$RESTORED_IMAGE" = "$FROZEN_IMAGE" ] \
  || fail "the restored image is not the one the Snapshot froze: $RESTORED_IMAGE != $FROZEN_IMAGE"
# The whole point of creating e2e-ws with an explicit image: without this the assertion above would
# pass just as well on a restore that ignored `spec.state` and fell back to the default.
[ "$RESTORED_IMAGE" != "ghcr.io/kloudlite/rustic-git-workspace" ] \
  || fail "the restored image is the default fallback, so the frozen state proved nothing"
[ "$(kubectl get snapshot "$SNAP_NAME" -o jsonpath='{.spec.state.kind}')" = workspace ] \
  || fail "snapshot carries no state"

# The restore RE-ATTACHED: the new workspace is a worktree of the SOURCE's volume (not a fresh
# one), it is an owner of that Volume again, and the pushed bytes are there.
[ "$(kubectl get workspace "$SNAP_STATE_RESTORE_ID" -o jsonpath='{.status.volumeRef}')" = "$WS_VOLUME" ] \
  || fail "the restored workspace is not a worktree of the snapshot's volume $WS_VOLUME"
[ -f "$(worktree_dir "$SNAP_STATE_RESTORE_ID")/hello.txt" ] || fail "the restored workspace is missing the pushed file"
[ "$(kubectl get snapshot "$SNAP_NAME" -o jsonpath='{.spec.state.packages}')" = "$SRC_PACKAGES" ] \
  || fail "the snapshot's frozen packages changed under us"
[ "$(kubectl get workspace "$SNAP_STATE_RESTORE_ID" -o jsonpath='{.spec.packages}')" = "$SRC_PACKAGES" ] \
  || fail "restore did not take the snapshot's frozen packages: $(kubectl get workspace "$SNAP_STATE_RESTORE_ID" -o jsonpath='{.spec.packages}') != $SRC_PACKAGES"
ROW=$(volume_row "$WS_VOLUME")
[ "$(echo "$ROW" | field deleted)" = "false" ] || fail "the volume is still listed as having no working copy after the restore: $ROW"

# The three refusals, all of them exercised against the live objects: delete is the only explicit
# verb on a snapshot, and it refuses exactly two things; a volume with a working copy refuses too.
log "durable snapshots: the refusals (a running worktree's base, a sync point, an attached volume)"
api_delete "$BASE/v1/volumes/$WS_VOLUME/snapshots/$SNAP_NAME"
[ "$DEL_CODE" = 409 ] || fail "deleting the base of the running restored workspace answered $DEL_CODE, not 409: $DEL_BODY"
echo "$DEL_BODY" | grep -q "this snapshot is the base of a running worktree" \
  || fail "wrong refusal for a running worktree's base: $DEL_BODY"
api_delete "$BASE/v1/volumes/$WS_VOLUME"
[ "$DEL_CODE" = 409 ] || fail "deleting a volume that still has a workspace answered $DEL_CODE, not 409: $DEL_BODY"
echo "$DEL_BODY" | grep -q "the volume still has a workspace or environment" \
  || fail "wrong refusal for an attached volume: $DEL_BODY"
SYNC_POINT=""
for i in $(seq 1 30); do
  SYNC_POINT=$(kubectl get snapshots -l "rustic-git.io/volume=$WS_VOLUME" \
    -o jsonpath='{range .items[?(@.spec.transient==true)]}{.metadata.name}{"\n"}{end}' 2>/dev/null | head -1)
  [ -n "$SYNC_POINT" ] && break
  sleep 2
done
[ -n "$SYNC_POINT" ] || fail "no sync point on $WS_VOLUME to try deleting by hand"
api_delete "$BASE/v1/volumes/$WS_VOLUME/snapshots/$SYNC_POINT"
[ "$DEL_CODE" = 409 ] || fail "deleting a sync point by hand answered $DEL_CODE, not 409: $DEL_BODY"
echo "$DEL_BODY" | grep -q "a sync point cannot be deleted by hand" \
  || fail "wrong refusal for a sync point: $DEL_BODY"
kubectl get snapshot "$SYNC_POINT" >/dev/null 2>&1 || fail "the refused sync-point delete removed it anyway"

# And the end of the line: delete the working copy again, then every snapshot. The last one takes
# the Volume with it, and the agent's byte sweep reclaims the tree.
log "durable snapshots: deleting the restored workspace and then its snapshots collects the volume"
curl -fsS -X DELETE "$BASE/v1/workspaces/$SNAP_STATE_RESTORE_ID" -H "Authorization: Bearer $USER_TOKEN" >/dev/null
wait_ws_gone "$SNAP_STATE_RESTORE_ID"
SNAP_STATE_RESTORE_ID=""
kubectl get volume "$WS_VOLUME" >/dev/null 2>&1 || fail "the Volume CR $WS_VOLUME went with its second working copy; its snapshots should have kept it"
for sn in $(kubectl get snapshots -l "rustic-git.io/volume=$WS_VOLUME" \
  -o jsonpath='{range .items[?(@.spec.transient==false)]}{.metadata.name}{"\n"}{end}' 2>/dev/null); do
  api_delete "$BASE/v1/volumes/$WS_VOLUME/snapshots/$sn"
  [ "$DEL_CODE" = 204 ] || fail "deleting snapshot $sn answered $DEL_CODE: $DEL_BODY"
done
kubectl wait --for=delete "volume/$WS_VOLUME" --timeout=120s \
  || fail "the Volume CR $WS_VOLUME survived the delete of its last snapshot"
for i in $(seq 1 60); do
  [ ! -e "$MOUNT/vol/$WS_VOLUME" ] && break
  sleep 2
done
[ ! -e "$MOUNT/vol/$WS_VOLUME" ] || fail "the volume tree $MOUNT/vol/$WS_VOLUME is still on the node after its last snapshot was deleted"

echo "OK (commit model): push -> Ready Snapshot, clone from head, restore to a named commit, replica on a second node all passed"
echo "OK (snapshot state): restore with the source deleted kept the frozen image, and the Snapshot CR carries spec.state"
echo "OK (durable snapshots): a push outlived its workspace and its environment — detached volume listed with its snapshots, restore re-attached with the frozen definition and the pushed bytes, the three 409s refused, and the last snapshot's delete collected the Volume and the tree on the node"

# ---------------------------------------------------------------------------
# Volume takeover: node JOIN (spread + retire). Node DEATH is verified by hand on the cluster —
# `node_is_dead` reads the Node object's Ready condition, and nothing this harness can do from
# inside the script (a label flip, a nodeSelector trick) makes kubelet stop heartbeating, so there
# is no safe in-script way to simulate it; not covered here.
# ---------------------------------------------------------------------------
POOL_NODES=$(kubectl get nodes -l rustic-git.io/pool=true --no-headers 2>/dev/null | awk '{print $1}')
POOL_NODE_COUNT=$(printf '%s\n' "$POOL_NODES" | grep -c . || true)
if [ "$POOL_NODE_COUNT" -lt 3 ]; then
  log "volume takeover: skipping (need >=3 rustic-git.io/pool=true nodes, found $POOL_NODE_COUNT)"
else
  log "volume takeover: node JOIN — creating a workspace, then dropping the standby's pool label"
  TAKEOVER_WS_JSON=$(curl -fsS -X POST "$BASE/v1/workspaces" -H "Authorization: Bearer $USER_TOKEN" \
    -H 'Content-Type: application/json' -d '{"name":"e2e-takeover","region":"'"$REGION_ID"'","quota_gb":5}')
  TAKEOVER_WS_ID=$(echo "$TAKEOVER_WS_JSON" | field id)
  [ -n "$TAKEOVER_WS_ID" ] || fail "no id in takeover-workspace create response: $TAKEOVER_WS_JSON"
  wait_ws_ready "$TAKEOVER_WS_ID"

  STANDBY=""
  for i in $(seq 1 30); do
    STANDBY=$(kubectl get volumereplicas -l "rustic-git.io/volume=$TAKEOVER_WS_ID" \
      -o jsonpath='{.items[?(@.status.phase=="Synced")].spec.node}' 2>/dev/null | tr ' ' '\n' | grep -vx "$E2E_NODE" | head -1)
    [ -n "$STANDBY" ] && break
    sleep 2
  done
  [ -n "$STANDBY" ] || fail "takeover workspace $TAKEOVER_WS_ID never got a Synced standby replica"

  # Removing and re-adding a THIRD node's label restores the identical pool set, so rendezvous
  # re-elects the same standby and nothing has moved — the drill would pass on a no-op. Drop the
  # STANDBY's label instead: the slot has nowhere to stay and must move to another live node.
  kubectl label node "$STANDBY" rustic-git.io/pool- >/dev/null

  NEW_STANDBY=""
  for i in $(seq 1 90); do
    NEW_STANDBY=$(kubectl get volumereplicas -l "rustic-git.io/volume=$TAKEOVER_WS_ID" \
      -o jsonpath='{.items[?(@.status.phase=="Synced")].spec.node}' 2>/dev/null \
      | tr ' ' '\n' | grep -vx "$E2E_NODE" | grep -vx "$STANDBY" | head -1)
    [ -n "$NEW_STANDBY" ] && break
    sleep 4
  done
  [ -n "$NEW_STANDBY" ] || fail "no Synced VolumeReplica for $TAKEOVER_WS_ID appeared on a node other than $E2E_NODE/$STANDBY after dropping $STANDBY's pool label"

  # Retire is keep-biased: the old copy goes only on the beat AFTER its replacement reports
  # Synced, so this poll starts from a state where both rows legitimately exist.
  for i in $(seq 1 30); do
    kubectl get volumereplicas -l "rustic-git.io/volume=$TAKEOVER_WS_ID" \
      -o jsonpath="{.items[?(@.spec.node==\"$STANDBY\")]}" 2>/dev/null | grep -q . || break
    sleep 2
  done
  kubectl get volumereplicas -l "rustic-git.io/volume=$TAKEOVER_WS_ID" \
    -o jsonpath="{.items[?(@.spec.node==\"$STANDBY\")]}" 2>/dev/null | grep -q . \
    && fail "the displaced node $STANDBY's VolumeReplica row for $TAKEOVER_WS_ID is still around"
  # The displaced node's subvolume lives on ITS OWN pool, not this script's loopback $MOUNT, so it
  # can only be checked here when this script's own node happens to be the one that was displaced.
  TAKEOVER_VOL=$(kubectl get workspace "$TAKEOVER_WS_ID" -o jsonpath='{.status.volumeRef}')
  if [ "$STANDBY" = "$E2E_NODE" ]; then
    [ -e "$MOUNT/vol/$TAKEOVER_VOL" ] && fail "retired subvolume $MOUNT/vol/$TAKEOVER_VOL still present on $STANDBY"
  fi

  # Give the node its label back and let placement settle: the pool set is the original one again,
  # so rendezvous elects $STANDBY once more and $NEW_STANDBY retires. Bounded — a cluster left
  # mid-move by a timeout here is worse than a slow assert.
  kubectl label node "$STANDBY" rustic-git.io/pool=true --overwrite >/dev/null
  SETTLED=""
  for i in $(seq 1 90); do
    SETTLED=$(kubectl get volumereplicas -l "rustic-git.io/volume=$TAKEOVER_WS_ID" \
      -o jsonpath="{.items[?(@.spec.node==\"$STANDBY\")].status.phase}" 2>/dev/null)
    [ "$SETTLED" = "Synced" ] && break
    sleep 4
  done
  [ "$SETTLED" = "Synced" ] || fail "after restoring $STANDBY's pool label, its VolumeReplica for $TAKEOVER_WS_ID never came back Synced"
  log "volume takeover: node JOIN passed (standby moved $STANDBY -> $NEW_STANDBY, old row and subvolume gone, then settled back to $STANDBY)"
fi
echo
echo "OK: create -> Ready, write, push (message+history+refs), clone (pushed content), packages (build/patch/remove/reject), clone (running source), kl ws ssh through the gateway (session mint, other-user 404, authorized_keys, NetworkPolicy blocks a direct peer), persistent home (shared by two pods, stop pushes it, start keeps it, caches excluded), restore (explicit snapshot), git-seeded workspace (one unplaced object, claimed, cloned, child Volume GC'd), env up (own subvolume + write), cross-namespace DNS, default-deny enforced, controller reconcile, attach (bare-name resolution with no pod restart) and detach, env down (push+stop, history), durable snapshots for both kinds (delete -> detached -> restore -> collect) and a definition-change sync cut all passed"
