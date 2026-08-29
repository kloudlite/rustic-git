#!/usr/bin/env bash
# Repin every image in deploy/ to one commit: `deploy/pin.sh <sha> [web-sha]`.
#
# THE CONTRACT. Five images, two SHAs, one edit:
#   - rustic-git, rustic-git-agent, rustic-git-gateway are three targets of ONE Dockerfile, built
#     from ONE commit by image.yml. The server tier, the agent and the gateway therefore always
#     pin the SAME sha — the agent speaks to the server's `vol/` surface, and two SHAs there is a
#     wire-compatibility bet nobody placed. That is <sha>: rustic-git-leader.yaml, rustic-git.yaml
#     (srv, api, worker), k3s/agent-daemonset.yaml, k3s/gateway.yaml.
#   - rustic-git-web is built by web.yml, which runs only when web/** changes, so its SHA is
#     usually older and is the optional second argument: rustic-git-web.yaml.
#   No kustomize, no envsubst: the manifests stay plain files kubectl applies as they are, and
#   this script is the one place that knows where the pins live. Edit a pin by hand and the next
#   run overwrites it.
#
# A tag that does not exist is refused: image.yml publishes a package only when the commit's
# tests passed, so "the tag exists" is the tests-passed signal, and an ImagePullBackOff at roll
# time is the wrong place to learn a SHA was red or is still building.
#
# It only edits files. Rolling is `deploy/roll.sh` (AKS, in the order that matters) and
# `kubectl apply -f deploy/k3s/{agent-daemonset,gateway}.yaml` on the k3s side.
set -euo pipefail
cd "$(dirname "$0")"
SHA=${1:?commit sha image.yml built, 40 hex}
WEB=${2:-}

tag_exists() {
  # Anonymous pull token: the packages are public. The Accept list covers an index or a single
  # manifest, whichever buildx wrote — without it ghcr answers 404 for a perfectly good tag.
  local tok
  tok=$(curl -sS "https://ghcr.io/token?scope=repository:kloudlite/$1:pull" | sed -n 's/.*"token":"\([^"]*\)".*/\1/p')
  curl -sfI -o /dev/null -H "Authorization: Bearer $tok" \
    -H "Accept: application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json" \
    "https://ghcr.io/v2/kloudlite/$1/manifests/$2"
}

for img in rustic-git rustic-git-agent rustic-git-gateway; do
  tag_exists "$img" "$SHA" || { echo "ghcr.io/kloudlite/$img:$SHA does not exist — tests red, still building, or a typo" >&2; exit 1; }
done
[ -z "$WEB" ] || tag_exists rustic-git-web "$WEB" || { echo "ghcr.io/kloudlite/rustic-git-web:$WEB does not exist" >&2; exit 1; }

# The tag character class also swallows a `dev-<sha>[-dirty]` tag dev-push.sh left behind.
perl -pi -e "s#(ghcr\.io/kloudlite/rustic-git(-agent|-gateway)?:)[A-Za-z0-9_.-]+#\${1}$SHA#" \
  rustic-git-leader.yaml rustic-git.yaml k3s/agent-daemonset.yaml k3s/gateway.yaml
[ -z "$WEB" ] || perl -pi -e "s#(ghcr\.io/kloudlite/rustic-git-web:)[A-Za-z0-9_.-]+#\${1}$WEB#" rustic-git-web.yaml

grep -rn --include='*.yaml' -E 'image: ghcr\.io/kloudlite/' . | sed 's/^\.\///'
cat <<EOF

pinned. Next:
  git commit -am "Pin every tier to $SHA"
  deploy/roll.sh                                   # AKS: leader, wait, then the rest
  KUBECONFIG=.local/k3s.yaml kubectl apply -f deploy/k3s/agent-daemonset.yaml -f deploy/k3s/gateway.yaml
EOF
