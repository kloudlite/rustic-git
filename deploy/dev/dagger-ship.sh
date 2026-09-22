#!/usr/bin/env bash
# Laptop-driven equivalent of deploy/dev/pod/ship.sh: no push, no remote exec — the laptop's
# dagger CLI drives the dagger-engine sidecar in the dev pod (deploy/dev/builder.yaml) over
# kubectl exec, and the laptop worktree is the source Dagger uploads.
#
# usage: deploy/dev/dagger-ship.sh [--no-gate] [--source DIR]
set -euo pipefail

NO_GATE=0
SRC=""
while [ $# -gt 0 ]; do
  case "$1" in
    --no-gate) NO_GATE=1; shift ;;
    --source) SRC=$2; shift 2 ;;
    *) echo "usage: deploy/dev/dagger-ship.sh [--no-gate] [--source DIR]" >&2; exit 2 ;;
  esac
done
[ -n "$SRC" ] || SRC=$(git rev-parse --show-toplevel)

# Never a hard-coded pod name: the dev Deployment is recreated by builder.yaml edits and gets a
# fresh pod name every time.
POD=$(kubectl -n kloudlite get pod -l app=dev -o jsonpath='{.items[0].metadata.name}')
export _EXPERIMENTAL_DAGGER_RUNNER_HOST="kube-pod://$POD?namespace=kloudlite&container=dagger-engine"

# deploy/pin.sh's own comment insists on "commit sha image.yml built, 40 hex" and its regex only
# matches a 40-hex tag — so the full SHA is the tag here, not the short-8 a person would type by
# hand, to keep `deploy/pin.sh $TAG` a straight copy-paste.
TAG=$(git -C "$SRC" rev-parse HEAD)
[ -z "$(git -C "$SRC" status --porcelain)" ] || TAG="${TAG}-dirty"
echo "tag: $TAG"

# ghcr credentials from docker's own store, never typed here: `auths["ghcr.io"].auth` is
# base64(user:token) the same way `docker login` writes it.
AUTH=$(jq -r '.auths["ghcr.io"].auth // empty' "$HOME/.docker/config.json" 2>/dev/null || true)
if [ -z "$AUTH" ]; then
  echo "no ghcr.io entry in ~/.docker/config.json — run: docker login ghcr.io" >&2
  exit 2
fi
DECODED=$(echo "$AUTH" | base64 -d)
GHCR_USER=${DECODED%%:*}
export GHCR_TOKEN=${DECODED#*:}

VERB=ship
[ "$NO_GATE" = 1 ] && VERB=publish

# Interactive: dagger's own step tree. Piped to a file: one line per step with its output
# (`--progress plain -v`), not the bare "N steps running" heartbeat plain mode prints alone.
PROGRESS=(); [ -t 1 ] || PROGRESS=(--progress plain -v)

cd "$SRC"
dagger "${PROGRESS[@]}" -m deploy/dagger call "$VERB" \
  --source "$SRC" --tag "$TAG" --ghcr-user "$GHCR_USER" --ghcr-token env://GHCR_TOKEN

unset GHCR_TOKEN

echo
echo "shipped $TAG — on the laptop:"
echo "  deploy/pin.sh $TAG $TAG"
echo "  deploy/roll.sh"
