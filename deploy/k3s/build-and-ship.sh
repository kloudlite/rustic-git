#!/usr/bin/env bash
# Build the agent image on the build VM and load it into each worker's containerd.
#
# WHY a separate build VM: the toolchain used to live on session-0, a cluster node. A Rust build
# tree plus Docker filled its 30 GB OS disk, the kubelet tainted the node `disk-pressure`, every
# pod stopped scheduling, and the kubelet garbage-collected the agent image out from under the
# DaemonSet. None of those failures looked like "the disk is full" from the cluster's side. A node
# that runs workloads should not also be a build box.
#
# WHY `ctr images import` and not a registry: there is no registry in this environment yet, and a
# tarball into each node's containerd is the shortest path that works. It does not scale past a
# handful of nodes and it leaves each node's copy at the mercy of kubelet image GC.
# ponytail: hand-shipped images; the real answer is CI pushing to a registry and the DaemonSet
# pulling a digest — do that before there are more nodes than fit on one screen.
set -euo pipefail
cd "$(dirname "$0")"
. ./env.sh

: "${BUILD_HOST:?set BUILD_HOST to the build VM, e.g. azureuser@20.0.0.1}"
: "${WORKERS:?set WORKERS to a space-separated list of worker ssh targets}"
TAG="${TAG:-rustic-git-agent:dev}"
REPO_ROOT="$(cd ../.. && pwd)"

echo "==> syncing source to $BUILD_HOST"
# --delete so a file removed locally cannot linger and silently get built in.
rsync -az --delete \
  --exclude target --exclude .git --exclude node_modules --exclude web \
  "$REPO_ROOT/" "$BUILD_HOST:~/rustic-git/"

echo "==> building $TAG"
ssh "$BUILD_HOST" "cd ~/rustic-git && sudo docker build -q -f deploy/k3s/Dockerfile.agent -t '$TAG' ." >/dev/null

echo "==> saving image"
ssh "$BUILD_HOST" "sudo docker save '$TAG' -o /tmp/agent.tar && sudo chmod 644 /tmp/agent.tar"

for w in $WORKERS; do
  echo "==> shipping to $w"
  # -3 routes through here rather than requiring the build VM to hold the workers' keys.
  scp -q -3 "$BUILD_HOST:/tmp/agent.tar" "$w:/tmp/agent.tar"
  ssh "$w" "sudo k3s ctr images import /tmp/agent.tar >/dev/null && sudo rm -f /tmp/agent.tar"
  echo "    imported on $w"
done
ssh "$BUILD_HOST" "sudo rm -f /tmp/agent.tar"

echo "==> restarting the DaemonSet so the new image is picked up"
# The tag does not change, so a rollout restart is what makes kubelet re-read it. With
# imagePullPolicy IfNotPresent and a same-named tag, nothing else would.
kubectl -n kube-system rollout restart daemonset/rustic-git-agent
kubectl -n kube-system rollout status daemonset/rustic-git-agent --timeout=180s
