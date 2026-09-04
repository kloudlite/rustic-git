#!/usr/bin/env bash
# Build both images on the build VM and roll them straight onto the cluster.
#
# The fast loop. CI takes ~8 minutes because it starts from a cold GitHub Actions cache and builds
# with the production profile; this reuses the build VM's warm cargo target and the `dev-image`
# profile (no LTO, 16 codegen units), so an incremental change is a couple of minutes.
#
# NOT for production. `dev-image` skips exactly the optimizations that make the release binary
# worth its build time, and the images are tagged `dev-{short-sha}` so nothing can mistake one for
# a CI artifact. Deploy manifests pin CI's SHA tags; this only ever moves what is running now.
set -euo pipefail
cd "$(dirname "$0")"
. ./env.sh

: "${BUILD_HOST:?set BUILD_HOST, e.g. azureuser@20.0.0.1}"
REPO_ROOT="$(cd ../.. && pwd)"
SHA="$(git -C "$REPO_ROOT" rev-parse --short HEAD)"
DIRTY=""
git -C "$REPO_ROOT" diff --quiet || DIRTY="-dirty"
TAG="dev-${SHA}${DIRTY}"
REG="${DEV_REGISTRY:-ghcr.io/kloudlite}"

echo "==> syncing to $BUILD_HOST"
# --delete so a file removed locally cannot linger on the builder and get compiled in.
rsync -az --delete \
  --exclude target --exclude .git --exclude node_modules --exclude web \
  "$REPO_ROOT/" "$BUILD_HOST:~/kloudlite-git/"

echo "==> building $TAG (profile dev-image)"
# The Dockerfile is runtime-only (see its header): cargo runs on the VM itself, against its warm
# target dir, and the two docker builds only COPY target/dev-image/. The VM needs rustup's stable
# toolchain; the compile lands binaries for bookworm as long as the VM's glibc is <= 2.36.
ssh "$BUILD_HOST" "cd ~/kloudlite-git && \
  cargo build --profile dev-image --locked && \
  sudo docker build --build-arg PROFILE=dev-image --target server -t '$REG/kloudlite-git:$TAG' . && \
  sudo docker build --build-arg PROFILE=dev-image --target agent  -t '$REG/kloudlite-git-agent:$TAG' ."

echo "==> pushing"
# GHCR needs a token with write:packages on the builder, once:
#   echo $PAT | sudo docker login ghcr.io -u <user> --password-stdin
# NOT `gh auth token` — the CLI's default scopes do not include write:packages, and the push fails
# with `permission_denied: The token provided does not match expected scopes` only at the very end,
# after the whole build. Use a PAT created with write:packages.
ssh "$BUILD_HOST" "sudo docker push '$REG/kloudlite-git:$TAG' && sudo docker push '$REG/kloudlite-git-agent:$TAG'"

echo "==> rolling"
# Through the manifest, never `kubectl set image` on the live DaemonSet: that left the yaml
# claiming a SHA that was not running, with nothing in `git status` to say so. Now the yaml is
# the thing that changed — `git diff` shows the dev tag, and putting CI's pin back is
# `deploy/pin.sh <sha>` (or `git checkout -- agent-daemonset.yaml`) plus one more apply.
perl -pi -e "s#^(\s+image: ).*/kloudlite-git-agent:\S+#\${1}$REG/kloudlite-git-agent:$TAG#" agent-daemonset.yaml
kubectl apply -f agent-daemonset.yaml
kubectl -n kube-system rollout status daemonset/kloudlite-git-agent --timeout=300s
echo "agent-daemonset.yaml now pins $TAG — a dev image. Do not commit it; restore CI's pin with deploy/pin.sh <sha> and apply again."
echo "server image: $REG/kloudlite-git:$TAG  (roll it with your own context, this script does not
touch the server tier — it lives on a different cluster)"
