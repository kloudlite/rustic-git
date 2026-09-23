#!/usr/bin/env bash
# Repin every image in deploy/ to one commit: `deploy/pin.sh <sha> [web-sha]`.
#
# THE CONTRACT. Ten images, two SHAs, one edit:
#   - kloudlite, kloudlite-agent, kloudlite-gateway, kloudlite-builder-gate, kloudlite-workspace,
#     kloudlite-bench, kloudlite-intercept-proxy, kloudlite-slo, kloudlite-controller are nine
#     targets of ONE Dockerfile (the bench's is deploy/bench/Dockerfile over the same context), built
#     from ONE commit by image.yml. The server tier, the agent and the gateway therefore always
#     pin the SAME sha — the agent speaks to the server's `vol/` surface, and two SHAs there is a
#     wire-compatibility bet nobody placed. That is <sha>: kloudlite.yaml (srv, api, worker, the
#     three slo CronJobs), k3s/agent-daemonset.yaml, k3s/gateway.yaml. The probe rides the same SHA
#     for the same reason: one pinned behind the fleet reports the previous release's journey.
#   - kloudlite-web is built by web.yml, which runs only when web/** changes, so its SHA is
#     usually older and is the optional second argument: kloudlite-web.yaml.
#   No kustomize, no envsubst: the manifests stay plain files kubectl applies as they are, and
#   this script is the one place that knows where the pins live. Edit a pin by hand and the next
#   run overwrites it.
#
# A tag that does not exist is refused: image.yml publishes a package only when the commit's
# tests passed, so "the tag exists" is the tests-passed signal, and an ImagePullBackOff at roll
# time is the wrong place to learn a SHA was red or is still building.
#
# It only edits files. Rolling is `deploy/roll.sh` (AKS, one apply) and
# `kubectl apply -f deploy/k3s/{agent-daemonset,gateway}.yaml` on the k3s side.
set -euo pipefail
cd "$(dirname "$0")"
SHA=${1:?commit sha image.yml built, 40 hex}
WEB=${2:-}

# digest_of also proves the tag exists (curl -f fails the manifest fetch on a 404), so it
# replaces tag_exists rather than running a second round-trip: same token, same request, the
# digest is just a response header we weren't reading before. A GHCR tag is mutable — a
# re-pushed :<sha> would change what an IfNotPresent node pulls next — so the digest, not the
# tag, is what actually pins the image; the tag stays in the reference for legibility.
digest_of() {
  # Anonymous pull token: the packages are public. The Accept list covers an index or a single
  # manifest, whichever buildx wrote — without it ghcr answers 404 for a perfectly good tag.
  local tok digest
  tok=$(curl -sS "https://ghcr.io/token?scope=repository:kloudlite/$1:pull" | sed -n 's/.*"token":"\([^"]*\)".*/\1/p')
  digest=$(curl -sfI -H "Authorization: Bearer $tok" \
    -H "Accept: application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json" \
    "https://ghcr.io/v2/kloudlite/$1/manifests/$2" | tr -d '\r' | sed -n 's/^[Dd]ocker-[Cc]ontent-[Dd]igest: *//p') || return 1
  [ -n "$digest" ] || return 1
  echo "$digest"
}

declare -A DIGEST
for img in kloudlite kloudlite-agent kloudlite-gateway kloudlite-controller kloudlite-builder-gate kloudlite-workspace kloudlite-bench kloudlite-shell kloudlite-intercept-proxy kloudlite-slo kloudlite-kompress; do
  # The shell image is OPTIONAL while it is landing: a SHA built before `image.yml` grew its stage
  # has no package, and a hard failure here would block every unrelated roll — the same reasoning
  # the controller's manifest carries below. It becomes mandatory by simply existing.
  if [ "$img" = kloudlite-shell ] && ! digest_of "$img" "$SHA" >/dev/null 2>&1; then
    echo "note: no ghcr.io/kloudlite/kloudlite-shell:$SHA yet — leaving its pin alone" >&2
    continue
  fi
  # Only an image that is actually pinned is demanded. The controller's manifest
  # (k3s/controller.yaml) lands after its Dockerfile stage does, and a SHA built before that stage
  # existed has no controller package at all — a hard failure here would block every unrelated
  # roll. The tests-passed signal stays hard for every image that IS pinned.
  if [ "$img" = kloudlite-controller ] && [ ! -f k3s/controller.yaml ]; then continue; fi
  DIGEST[$img]=$(digest_of "$img" "$SHA") || { echo "ghcr.io/kloudlite/$img:$SHA does not exist — tests red, still building, or a typo" >&2; exit 1; }
done
if [ -n "$WEB" ]; then
  DIGEST[kloudlite-web]=$(digest_of kloudlite-web "$WEB") || { echo "ghcr.io/kloudlite/kloudlite-web:$WEB does not exist" >&2; exit 1; }
fi

# Match a bare :<sha> or an already digest-pinned :<sha>@sha256:<old-digest> and replace the
# whole tail, so a second run is a no-op. The tag character
# class also swallows a `dev-<sha>[-dirty]` tag dev-push.sh left behind, but only on the first
# (bare-tag) alternative — a dev tag is never digest-pinned by this script.
pin() {
  # The digest-pinned alternative comes FIRST: perl alternation is ordered, and the loose tag
  # class would otherwise stop at the `@`, leave the old digest in place and append a second one.
  # `*` on the digest group also collapses a reference that already got doubled that way.
  # A manifest that does not exist yet is skipped with a notice, not an error: a new tier's image
  # is built a commit or two before the yaml that runs it lands, and perl -pi only warns on an
  # unopenable file, so without this the run would "succeed" having pinned nothing.
  local f targets=()
  for f in "${@:4}"; do
    [ -f "$f" ] || { echo "pin: $f not present yet — skipping $1" >&2; continue; }
    targets+=("$f")
  done
  [ ${#targets[@]} -gt 0 ] || return 0
  perl -pi -e "s#(ghcr\.io/kloudlite/$1:)(?:[0-9a-f]{40}(?:\@sha256:[0-9a-f]{64})*|[A-Za-z0-9_.-]+)#\${1}$2\@$3#" "${targets[@]}"
}
pin 'kloudlite(?!-)' "$SHA" "${DIGEST[kloudlite]}" kloudlite.yaml
pin 'kloudlite-agent' "$SHA" "${DIGEST[kloudlite-agent]}" k3s/agent-daemonset.yaml
pin 'kloudlite-gateway' "$SHA" "${DIGEST[kloudlite-gateway]}" k3s/gateway.yaml
# The cluster controller. NOTE: a new ghcr package is PRIVATE by default, and digest_of fetches
# the manifest anonymously — a new package must be made public once in the GitHub UI before the
# first pin succeeds.
pin 'kloudlite-controller' "$SHA" "${DIGEST[kloudlite-controller]:-}" k3s/controller.yaml
pin 'kloudlite-builder-gate' "$SHA" "${DIGEST[kloudlite-builder-gate]}" k3s/builder-gate.yaml
pin 'kloudlite-kompress' "$SHA" "${DIGEST[kloudlite-kompress]}" k3s/kompress.yaml
# The workspace image is not a workload of ours: the agent hands it to tenant pods
# (WS_DEFAULT_IMAGE), so it lives in the DaemonSet's env, not an image: line.
pin 'kloudlite-workspace' "$SHA" "${DIGEST[kloudlite-workspace]}" k3s/agent-daemonset.yaml
# The proxy image is not a workload of ours either: the agent hands it to a pod in a tenant
# namespace (WS_INTERCEPT_PROXY_IMAGE), so it lives in the DaemonSet's env, not an `image:` line.
pin 'kloudlite-intercept-proxy' "$SHA" "${DIGEST[kloudlite-intercept-proxy]}" k3s/agent-daemonset.yaml
# The bench image is the agent's to put on a bench workspace's second container
# (KLOUDLITE_BENCH_IMAGE in the DaemonSet's env), never a workload of ours — the api does not read
# it at all since a bench became a Workspace.
pin 'kloudlite-bench' "$SHA" "${DIGEST[kloudlite-bench]}" k3s/agent-daemonset.yaml
# The shell sidecar, the same way: the agent puts it on every workspace and bench pod
# (KLOUDLITE_SHELL_IMAGE), so it is the DaemonSet's env and never an `image:` line of ours.
[ -z "${DIGEST[kloudlite-shell]:-}" ] || pin 'kloudlite-shell' "$SHA" "${DIGEST[kloudlite-shell]}" k3s/agent-daemonset.yaml
pin 'kloudlite-slo' "$SHA" "${DIGEST[kloudlite-slo]}" kloudlite.yaml
pin 'kloudlite-kompress' "$SHA" "${DIGEST[kloudlite-kompress]}" k3s/kompress.yaml
[ -z "$WEB" ] || pin 'kloudlite-web' "$WEB" "${DIGEST[kloudlite-web]}" kloudlite-web.yaml

grep -rn --include='*.yaml' -E 'image: ghcr\.io/kloudlite/' . | sed 's/^\.\///'
cat <<EOF

pinned. Next:
  git commit -am "Pin every tier to $SHA"
  deploy/roll.sh                                   # AKS: one apply, then the rollout waits
  KUBECONFIG=.local/k3s.yaml kubectl apply -f deploy/k3s/agent-daemonset.yaml -f deploy/k3s/gateway.yaml -f deploy/k3s/builder-gate.yaml -f deploy/k3s/controller.yaml
EOF
