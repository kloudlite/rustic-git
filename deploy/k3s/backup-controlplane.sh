#!/usr/bin/env bash
# Back up the k3s control plane to Azure Blob storage.
#
# WHY this exists: the CRDs are the source of truth for every workspace and environment, and with
# the SQLite datastore that truth is one file on one node. The btrfs subvolumes and the pushed
# blobs survive losing this node; the record of what they ARE does not. Cosmos used to be a managed,
# replicated store — `state.db` is not, so the replication has to be done here.
#
# Consistency: `VACUUM INTO` is used rather than `cp`. SQLite in WAL mode is being written while we
# run, so a plain copy can capture a torn page or miss committed transactions sitting in the WAL.
# `VACUUM INTO` takes a read transaction and writes a fully consistent database file.
#
# The certificates and node token are backed up alongside it, deliberately: restoring `state.db`
# onto a k3s that generated a DIFFERENT cluster CA gives you a cluster none of your agents can join
# and no client can authenticate to. The database alone is not a restorable backup.
set -euo pipefail

SRC=/var/lib/rancher/k3s/server
WORK=$(mktemp -d)
# Dead man's snitch. A timer that silently stopped firing looks exactly like one that works, so
# the check is inverted: a healthchecks.io-style monitor expects a ping every hour and pages when
# one is MISSING. `/fail` is healthchecks' failure suffix; a monitor without it just sees the
# missed ping. Unset means no ping — the unit's failed state is then the only signal.
: "${SNITCH_URL:=}"
finish() {
  rc=$?
  rm -rf "$WORK"
  [ -n "$SNITCH_URL" ] || return 0
  if [ "$rc" -eq 0 ]; then curl -sS -m 10 --retry 3 -o /dev/null "$SNITCH_URL" || true
  else curl -sS -m 10 --retry 3 -o /dev/null "$SNITCH_URL/fail" || true; fi
}
trap finish EXIT

# Retention without needing list or delete permission on the container: fixed names that
# overwrite. 24 hourly slots covering a day, 7 daily slots covering a week. Anything older than a
# week exists only through blob versioning on the container (deploy/BACKUPS.md), which is also
# what keeps a bad backup from destroying the good one it overwrites.
HOUR=$(date -u +%H)
DOW=$(date -u +%a)

sqlite3 "file:$SRC/db/state.db?mode=ro" "VACUUM INTO '$WORK/state.db'"
# Small and slow-changing, but without them the database restores into a cluster nobody can talk to.
tar -czf "$WORK/identity.tgz" -C "$SRC" tls token cred 2>/dev/null || \
  tar -czf "$WORK/identity.tgz" -C "$SRC" tls token
# The CRD objects as plain YAML as well. `state.db` restores only onto the same k3s version with
# the same identity; the YAML restores onto ANY cluster that has the CRDs applied, which is the
# path when this node is gone for good. All five kinds are cluster-scoped. Best-effort: an API
# server that is down must not stop the database from being uploaded — but the run still fails at
# the end (exit code below), so the unit and the snitch both show it.
crd_ok=0
k3s kubectl get volumes,workspaces,environments,snapshotrequests,ownerbindings -A -o yaml \
  > "$WORK/objects.yaml" 2> "$WORK/objects.err" \
  || { crd_ok=1; echo "CRD dump failed: $(cat "$WORK/objects.err")" >&2; : > "$WORK/objects.yaml"; }
tar -czf "$WORK/k3s-backup.tgz" -C "$WORK" state.db identity.tgz objects.yaml

: "${SAS_FILE:=/etc/rustic-git/k3s-backup.sas}"
: "${ACCOUNT:=rusticgitkolomi}"
: "${CONTAINER:=k3s-backup}"
SAS=$(cat "$SAS_FILE")

put() {
  # `--fail` so a rejected upload is a non-zero exit and therefore a failed systemd unit, not a
  # silent success that leaves you with no backup and no alert.
  curl -sS --fail -X PUT \
    -H "x-ms-blob-type: BlockBlob" \
    -H "Content-Type: application/gzip" \
    --data-binary "@$WORK/k3s-backup.tgz" \
    "https://${ACCOUNT}.blob.core.windows.net/${CONTAINER}/$1?${SAS}" >/dev/null
}

put "hourly-${HOUR}.tgz"
put "daily-${DOW}.tgz"

echo "backed up $(stat -c %s "$WORK/k3s-backup.tgz" 2>/dev/null || stat -f %z "$WORK/k3s-backup.tgz") bytes to hourly-${HOUR} and daily-${DOW}"
exit "$crd_ok"

# ---------------------------------------------------------------------------
# RESTORE, onto a fresh control-plane node:
#
#   1. Install the SAME k3s version, but do not let it start a new cluster:
#        curl -sfL https://get.k3s.io | INSTALL_K3S_VERSION=v1.33.5+k3s1 INSTALL_K3S_SKIP_START=true sh -s - server ...
#   2. systemctl stop k3s
#   3. Fetch and unpack:
#        curl -sS "https://ACCOUNT.blob.core.windows.net/k3s-backup/daily-Mon.tgz?SAS" -o /tmp/b.tgz
#        tar -xzf /tmp/b.tgz -C /tmp
#        install -m600 /tmp/state.db /var/lib/rancher/k3s/server/db/state.db
#        rm -f /var/lib/rancher/k3s/server/db/state.db-wal /var/lib/rancher/k3s/server/db/state.db-shm
#        tar -xzf /tmp/identity.tgz -C /var/lib/rancher/k3s/server
#      Node gone for good and a fresh cluster in its place? Skip state.db and identity.tgz:
#        kubectl apply -f deploy/k3s/crds.yaml && kubectl apply -f /tmp/objects.yaml
#      then roll the agent DaemonSet — its startup migration re-claims what it finds on the pool.
#      `status` is not restorable this way (it is a subresource); the controllers rebuild it.
#   4. systemctl start k3s
#   5. Agents rejoin on their own IF the restored identity matches the token they hold. Verify with
#      `kubectl get nodes` and `kubectl get volumes,workspaces,environments`.
#
# The `-wal`/`-shm` removal in step 3 matters: leaving a WAL from the OLD database beside a restored
# one is how a restore silently reintroduces the state you were trying to roll back.
# ---------------------------------------------------------------------------
